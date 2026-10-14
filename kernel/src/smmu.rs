//! The SMMUv3 hardware rung: the IOMMU contract programmed into the REAL remapping unit QEMU
//! emulates on virt (ALET-P1-018; ADR-071 contract, ADR-073 x86-64 twin, ADR-074 delivery).
//!
//! Everything arch-independent lives in kernel_core::smmu behind two seams. This file supplies
//! the platform facts and runs the live boot suite:
//!
//! * **Discovery, not poking.** The machine's own tree arrives over the firmware configuration
//!   channel (see dtb.rs); it names the unit's register base and binds PCIe requesters to it
//!   (iommu-map), and platform devices carry NO iommus property on this machine - asserted.
//! * **Identity domains over owned frames.** One shared STAGE-2 domain translates the frame
//!   pool minus the protected image span 1:1, built entirely from PAGETABLE-owned frames;
//!   every present PCI function gets an STE naming that tree under its own stream id (the
//!   RID), and the block function behind the bridge is brought up THROUGH that programming:
//!   ECAM enumeration, kernel-assigned BARs, capability resolution, driver init.
//! * **Enforcement is turned ON and stays ON** from the gate to halt - every later suite this
//!   boot runs already executes alongside a latched remapping unit whose tables were audited.
//!
//! ## What THIS emulator cannot prove (named, not hidden)
//!
//! On QEMU 11.1, a virtio-blk-pci attached on the COMMAND LINE does not route its DMA through
//! the legacy iommu=smmuv3 unit: with every STE programmed CONFIG=ABORT as a canary, the
//! device's completions still arrived intact (measured). The unit's register interface,
//! enablement handshake, and OUR table walks are fully live - what is unreachable is a
//! DEVICE-SIDE walk, so grant-serves-clean / revocation-faults / restore-silences probes have
//! nothing to provoke. They stay open in the gap register beside ADR-073's lost-completion
//! artifact, with this measurement as evidence, until an emulator revision attaches
//! CLI-plugged devices to the unit.
//!
//! Exit codes: this suite fails at 480 + invariant-index. NOT claimed anywhere here:
//! inter-device isolation (all functions share the one domain), stage-1 translation,
//! interrupt remapping, ATS/PRI, MSI event signaling.
use crate::dtb::{Dtb, PcieDt, SmmuDt};
use crate::frames::{self, RAM_END};
use crate::pci;
use kernel_core::frameown::Owner;
use kernel_core::iommu::{IommuFault, Perm, SoftIommu, PAGE};
use kernel_core::smmu::{self, Controller, QueueGeom, Regs, TableMem};
use kernel_core::virtiopci::Bdf;

/// How long one PROBE kick may wait for a completion the platform is expected to LOSE (the kicks
/// below deliberately provoke the IOMMU into dropping them). Half a second of the platform clock:
/// long enough that a slow emulator never mistakes a delivered completion for a lost one, short
/// enough that a suite paying one timeout per kick stays inside the boot watchdog (ADR-150).
const PROBE_BUDGET_NS: u64 = 500_000_000;

/// The discovery facts parsed before any frame churned. Leaked deliberately: the boot heap
/// never frees (ADR-063), and one boxed struct is cheaper than re-walking a tree the pool may
/// already have overwritten.
static mut DISCOVERY: Option<&'static Discovery> = None;

/// What the early parse found; see dtb.rs for the field meanings.
pub struct Discovery {
    pub smmu: SmmuDt,
    pub pcie: PcieDt,
    /// Platform devices carrying an iommus property - this machine must report ZERO here.
    pub mmio_attached: usize,
}

/// Called ONCE, first thing in kmain - before frames::init hands out any frame.
pub fn discover_early() {
    let found = Dtb::load().and_then(|dtb| {
        let mmio_attached = dtb.virtio_mmio_attached_count();
        match dtb.discover_smmu() {
            Ok((smmu, pcie)) => {
                kprintln!(
                    "[smmu] DT discovery: smmuv3 @ {:#x}, ecam @ {:#x}, iommu-map entries {}",
                    smmu.base,
                    pcie.ecam_base,
                    pcie.map.len()
                );
                Some(Discovery {
                    smmu,
                    pcie,
                    mmio_attached,
                })
            }
            Err(e) => {
                kprintln!("[smmu] DT discovery refused: {}", e);
                None
            }
        }
    });
    // SAFETY: single-core at this point (secondaries are parked in boot.s until SMP bring-up).
    unsafe {
        DISCOVERY = found.map(|d| &*alloc::boxed::Box::leak(alloc::boxed::Box::new(d)));
    }
}

pub(crate) fn discovery() -> Option<&'static Discovery> {
    // SAFETY: written once before secondaries exist; read-only afterwards.
    unsafe { DISCOVERY }
}

// --- the two seams ------------------------------------------------------------------------------

/// The unit's register page. Widths follow the spec: 32-bit registers as 32, queue bases as 64.
struct MmioRegs {
    base: usize,
}

impl Regs for MmioRegs {
    fn r32(&mut self, off: usize) -> u32 {
        // SAFETY: base..+0x20000 is the declared SMMU register window inside the Device-mapped
        // GiB; every offset the controller touches is inside the unit's own map.
        unsafe { core::ptr::read_volatile((self.base + off) as *const u32) }
    }
    fn w32(&mut self, off: usize, v: u32) {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile((self.base + off) as *mut u32, v) }
    }
    fn r64(&mut self, off: usize) -> u64 {
        ((self.r32(off + 4) as u64) << 32) | self.r32(off) as u64
    }
    fn w64(&mut self, off: usize, v: u64) {
        self.w32(off, v as u32);
        self.w32(off + 4, (v >> 32) as u32);
    }
}

/// Walk frames from the owned-frame allocator - the same discipline as every other table this
/// kernel builds (ADR-030).
struct OwnedFrames;

impl TableMem for OwnedFrames {
    fn read_u64(&self, pa: usize) -> u64 {
        // SAFETY: alloc_zeroed_page below only returns owned, identity-mapped frames.
        unsafe { core::ptr::read_volatile(pa as *const u64) }
    }
    fn write_u64(&mut self, pa: usize, v: u64) {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile(pa as *mut u64, v) }
    }
    fn alloc_zeroed_page(&mut self) -> Option<usize> {
        frames::alloc_zeroed_as(Owner::PAGETABLE).map(|f| f.addr())
    }
}

fn rid(bdf: Bdf) -> u32 {
    ((bdf.bus as u32) << 8) | ((bdf.device as u32) << 3) | bdf.function as u32
}

/// Conventional-RAM spans this target can express: the frame allocator's whole pool. The image
/// subtraction happens in split_around_image - the builder REFUSES image-touching input rather
/// than deciding memory policy silently.
fn ram_ranges() -> alloc::vec::Vec<(usize, usize)> {
    alloc::vec![(frames::base(), RAM_END)]
}

fn split_around_image(
    ranges: &[(usize, usize)],
    image: (usize, usize),
) -> alloc::vec::Vec<(usize, usize)> {
    let mut out = alloc::vec::Vec::new();
    for &(s, e) in ranges {
        if e <= image.0 || s >= image.1 {
            out.push((s, e));
        } else {
            if s < image.0 {
                out.push((s, image.0));
            }
            if image.1 < e {
                out.push((image.1, e));
            }
        }
    }
    out
}

/// The live gate. Ok(count) => ALL count invariants hold (0 = skipped green); Err exits 480+i.
pub fn suite(report: &mut dyn FnMut(u32, bool, &'static str)) -> Result<u32, (u32, &'static str)> {
    let _ = report;
    let mut n = 0usize;
    macro_rules! pass {
        ($name:expr) => {{
            kprintln!("  [pass {:>2}] {}", n, $name);
        }};
    }
    macro_rules! bail {
        ($detail:expr, $name:expr) => {{
            kprintln!("[smmu] FAILED at smmuv3 invariant {}: {}", n, $detail);
            return Err((n as u32, $name));
        }};
    }

    // --- 1: discovery ---------------------------------------------------------------------------------
    n += 1;
    const NAME_1: &str = "the device tree declares arm,smmu-v3 inside the identity window";
    let Some(disc) = discovery() else {
        kprintln!("[smmu] no device tree discovered at boot (skipped - no declaration channel)");
        return Ok(0);
    };
    if !crate::vm::is_mapped_identity(disc.smmu.base)
        || !crate::vm::is_mapped_identity(disc.smmu.base + disc.smmu.size - 1)
    {
        bail!(
            "unit registers fall outside the identity-mapped window",
            NAME_1
        );
    }
    pass!(NAME_1);

    // --- 2: the PCIe hierarchy is behind THIS unit; platform devices are not --------------------------
    n += 1;
    const NAME_2: &str = "iommu-map binds PCIe to the unit; virtio-mmio stays outside it";
    if disc.mmio_attached != 0 {
        bail!(
            "a virtio-mmio node carries an iommus property - coverage changed",
            NAME_2
        );
    }
    pass!(NAME_2);

    // --- 3: identification ------------------------------------------------------------------------------
    n += 1;
    const NAME_3: &str = "the unit answers sane identification and speaks stage 2";
    let mut ctrl = Controller::new(
        MmioRegs {
            base: disc.smmu.base,
        },
        OwnedFrames,
    );
    let said = ctrl.identify();
    kprintln!(
        "[smmu] IDR0={:#010x} IDR1={:#010x} IDR5={:#010x} iidr={:#010x} aidr={:#010x}",
        said.idr[0],
        said.idr[1],
        said.idr[5],
        said.iidr,
        said.aidr
    );
    let rep = match ctrl.probe() {
        Ok(r) => r,
        Err(e) => bail!(format_args!("probe refused {:?}", e), NAME_3),
    };
    kprintln!(
        "[smmu] s1p={} s2p={} sidsize={} oas={} gran4k={}",
        rep.s1p(),
        rep.s2p(),
        rep.sid_size(),
        rep.oas_bits(),
        rep.gran4k()
    );
    pass!(NAME_3);

    // --- 4: the machine arrives quiet -------------------------------------------------------------------
    n += 1;
    const NAME_4: &str = "enforcement starts OFF (firmware hands the machine quiet)";
    if ctrl.smmu_enabled() {
        bail!("CR0ACK.SMMUEN set before we touched anything", NAME_4);
    }
    pass!(NAME_4);

    // --- 5: the domain, from owned frames, minus the image -----------------------------------------------
    n += 1;
    const NAME_5: &str = "domain built from owned frames, image punched out, counts balance";
    let geom = smmu::S2Geometry::standard(1);
    let image = crate::vm::protected_span();
    let spans = split_around_image(&ram_ranges(), image);

    // The stream table. A unit that offers two-level tables (IDR0.ST_LEVEL) gets one over all
    // 256 bus-0 requester ids: a level-1 frame and four 64-STE level-2 frames (ADR-259). Otherwise
    // one linear frame of 64 STEs, naturally aligned to its size (ADR-256), covering devices 0..7;
    // a function past it is refused by name below rather than written outside the frame.
    let two_level = rep.two_level();
    let strtab_log2: u32 = if two_level { 8 } else { 6 };
    let alloc_frame = |what: &'static str| -> Result<usize, (u32, &'static str)> {
        match frames::alloc_zeroed_as(Owner::PAGETABLE) {
            Some(f) => Ok(f.addr()),
            None => {
                kprintln!(
                    "[smmu] FAILED at smmuv3 invariant {}: no frame for the {}",
                    n,
                    what
                );
                Err((n as u32, NAME_5))
            }
        }
    };
    let strtab = if two_level {
        match smmu::build_strtab_2level(ctrl.table_mem(), strtab_log2, smmu::STRTAB_SPLIT) {
            Ok(l1) => l1,
            Err(e) => bail!(format_args!("two-level stream table {:?}", e), NAME_5),
        }
    } else {
        alloc_frame("stream table")?
    };
    // One STE rewrite, whichever table this unit was given.
    let write_ste = |mem: &mut OwnedFrames, sid: u32, e: Option<&smmu::Ste>| {
        if two_level {
            smmu::rewrite_ste_2level(mem, strtab, strtab_log2, sid, e)
        } else {
            smmu::rewrite_ste(mem, strtab, sid, e)
        }
    };
    let cmdq = alloc_frame("command queue")?;
    let evtq = alloc_frame("event queue")?;

    let free_before = frames::free_count();
    let (tree, stats) = {
        let mem = ctrl.table_mem();
        match smmu::program_identity_domain(&mut *mem, &spans, image, &geom) {
            Ok(x) => x,
            Err(e) => bail!(format_args!("builder refused {:?}", e), NAME_5),
        }
    };
    let ste = match smmu::ste_s2_encode(&geom, tree) {
        Ok(s) => s,
        Err(e) => bail!(format_args!("STE encode refused {:?}", e), NAME_5),
    };
    // free_before was sampled AFTER the three ring/table frames above, so the delta here is
    // exactly what the WALK consumed - the balance proof compares like with like.
    let claimed = free_before - frames::free_count();
    if claimed != stats.tables || !tree.is_multiple_of(PAGE) {
        bail!(
            format_args!(
                "claimed {} frames but the walk built {}",
                claimed, stats.tables
            ),
            NAME_5
        );
    }
    kprintln!(
        "[smmu] domain: {} table frames ({} huge + {} page leaves) over {} spans, image [{:#x},{:#x}) punched out",
        stats.tables, stats.huge_leaves, stats.page_leaves, spans.len(), image.0, image.1
    );
    pass!(NAME_5);

    // --- 6: the image has NO leaf -------------------------------------------------------------------------
    n += 1;
    const NAME_6: &str = "kernel image has NO leaf (live audit zero violations)";
    let audit = {
        let mem = ctrl.table_mem();
        smmu::audit_tree(&mut *mem, tree, &geom, image)
    };
    kprintln!(
        "[smmu] audit: {} tables walked, {} huge + {} page leaves, {} IMAGE violations",
        audit.tables,
        audit.tables,
        audit.huge_leaves + audit.page_leaves,
        audit.image_violations
    );
    if audit.image_violations != 0 || audit.huge_leaves + audit.page_leaves == 0 {
        bail!(
            "live tree audit found an image leaf or an empty domain",
            NAME_6
        );
    }
    pass!(NAME_6);

    // --- 7: grant every present function under its DECLARED stream id --------------------------------------
    n += 1;
    const NAME_7: &str = "every present function granted an STE under its DECLARED stream id";
    let env = pci::Ecam::new(disc.pcie.ecam_base);
    let funcs = unsafe { kernel_core::virtiopci::enumerate_bus0(&env) };
    if funcs.is_empty() {
        bail!("no PCI functions enumerated behind the bridge", NAME_7);
    }
    {
        let mem = ctrl.table_mem();
        for (f, vendor, devid) in &funcs {
            let r = rid(*f);
            if r >= 1 << strtab_log2 {
                bail!(
                    format_args!("rid {:#x} lies past the one-frame stream table", r),
                    NAME_7
                );
            }
            match disc.pcie.sid_for_rid(r) {
                Some(sid) if sid == r => {} // identity binding, as the virt map declares
                Some(sid) => bail!(
                    format_args!("rid {:#x} bound to sid {:#x}, not identity", r, sid),
                    NAME_7
                ),
                None => bail!(format_args!("rid {:#x} not behind this unit", r), NAME_7),
            }
            if let Err(e) = write_ste(&mut *mem, r, Some(&ste)) {
                bail!(format_args!("grant refused {:?}", e), NAME_7);
            }
            kprintln!(
                "[smmu] ste {:02x}:{:02x}.{} rid={:#04x} vendor={:#06x} devid={:#06x}",
                f.bus,
                f.device,
                f.function,
                r,
                vendor,
                devid
            );
        }
    }
    pass!(NAME_7);

    // --- 8: bring the block function up THROUGH the programmed path -----------------------------------------
    // ECAM enumeration, kernel-assigned BARs (no firmware ran), capability resolution and
    // driver init - every step real, none dependent on device-side translation.
    n += 1;
    const NAME_8: &str = "block function brought up through the programmed path";
    let Some(mut blk) = (unsafe { pci::open_block(&disc.pcie) }) else {
        bail!("no virtio-blk-pci attached behind the unit", NAME_8);
    };
    blk.dev.set_completion_budget_ns(PROBE_BUDGET_NS);
    pass!(NAME_8);

    // --- 9: model and machine agree ----------------------------------------------------------------------------
    n += 1;
    const NAME_9: &str = "model and machine agree (identity windows, image refused both sides)";
    {
        let probe_rid = rid(blk.bdf);
        let mut mirror = SoftIommu::new();
        mirror.declare_kernel_image(image.0, image.1);
        mirror.attach(probe_rid).expect("attach");
        let mut agree = mirror.image_declared();
        let mut checked = false;
        for &(s, e) in spans.iter().take(4) {
            let pages = ((e - s) / PAGE).clamp(1, 16);
            agree &= mirror.map(probe_rid, s, s, pages, true).is_ok();
            match mirror.translate(probe_rid, s, Perm::Read) {
                Ok(pa) => {
                    agree &= pa == s;
                    checked = true;
                }
                Err(_) => agree &= false,
            }
        }
        agree &= checked;
        agree &= matches!(
            mirror.map(probe_rid, image.0, image.0, 1, true),
            Err(IommuFault::KernelImage { .. })
        );
        if !agree {
            bail!("SoftIommu disagreed with the programmed shape", NAME_9);
        }
    }
    pass!(NAME_9);

    // --- 10: publish the tables and queues, turn enforcement ON, and stay latched -------------------------------
    n += 1;
    const NAME_10: &str =
        "stream table adopted, queues live, enforcement ENABLED and latched over the registry";
    let published = if two_level {
        ctrl.set_strtab_2level(strtab, strtab_log2)
    } else {
        ctrl.set_strtab(strtab, strtab_log2)
    };
    kprintln!(
        "[smmu] stream table: {} over {} requester ids",
        if two_level { "two-level" } else { "linear" },
        1u32 << strtab_log2
    );
    if let Err(e) = published {
        bail!(format_args!("set_strtab {:?}", e), NAME_10);
    }
    if let Err(e) = ctrl.set_queue(false, cmdq, &QueueGeom::new(6, smmu::CMDQ_ENTRY_BYTES)) {
        bail!(format_args!("cmdq {:?}", e), NAME_10);
    }
    if let Err(e) = ctrl.set_queue(true, evtq, &QueueGeom::new(6, smmu::EVTQ_ENTRY_BYTES)) {
        bail!(format_args!("evtq {:?}", e), NAME_10);
    }
    let want_strtab = strtab as u64 & 0x000F_FFFF_FFFF_FFC0;
    if ctrl.strtab_base() != want_strtab || ctrl.smmu_enabled() {
        bail!(
            "stream table pointer did not read back / enforcement crept on early",
            NAME_10
        );
    }
    if let Err(e) = ctrl.enable_translation() {
        bail!(format_args!("enable {:?}", e), NAME_10);
    }
    kprintln!("[smmu] enforcement LIVE: every translated PCIe DMA now walks the stage-2 tables");
    // Latched: the ack stays set, the published pointers did not drift, and the software DMA
    // registry still refuses what it always refused - two independent layers, both alive.
    let residency = ctrl.smmu_enabled() && ctrl.strtab_base() == want_strtab;
    let software_layer = blk.dev.dma_gate_refuses_unregistered()
        && blk.dev.dma_regions() == kernel_core::virtioblk::DMA_REGIONS;
    if !residency {
        bail!(
            "translation did not stay enabled / pointers drifted",
            NAME_10
        );
    }
    if !software_layer {
        bail!(
            "software DMA registry stopped refusing unregistered addresses",
            NAME_10
        );
    }
    pass!(NAME_10);

    // --- 11..13: the unit ENFORCES, observed from the device side (ADR-257) ---------------------------------
    // Until ADR-256 every STE was illegal, so nothing could DMA under enforcement and these were
    // left unasserted. Now: a granted stream's DMA completes and the unit records nothing; the
    // same stream with its STE revoked is denied BY NAME at its own stream id and the request
    // never completes; restoring the STE (plus the device's own reset, since the denied DMA broke
    // its queue) brings back the very bytes, with the unit quiet again.
    let probe_rid = rid(blk.bdf);
    let last = kernel_core::storage::BlockDevice::num_blocks(&blk.dev) - 1;
    let pattern: [u8; kernel_core::storage::BLOCK_SIZE] =
        core::array::from_fn(|i| (i.wrapping_mul(13) ^ 0xA5) as u8);
    let mut seen = alloc::vec::Vec::new();

    n += 1;
    const NAME_11: &str =
        "a granted function's DMA completes under enforcement, byte for byte, and the unit records nothing";
    {
        use kernel_core::storage::BlockDevice;
        let _ = ctrl.drain_events(&mut seen);
        seen.clear();
        let mut back = [0u8; kernel_core::storage::BLOCK_SIZE];
        let ok = blk.dev.write_block(last, &pattern).is_ok()
            && blk.dev.read_block(last, &mut back).is_ok()
            && back == pattern;
        let quiet = ctrl.drain_events(&mut seen).is_ok() && seen.is_empty();
        if !ok || !quiet {
            bail!(
                format_args!("completed={} events={}", ok, seen.len()),
                NAME_11
            );
        }
    }
    pass!(NAME_11);

    n += 1;
    const NAME_12: &str =
        "a revoked stream is denied by name (C_BAD_STE at its own stream id) and its request never completes";
    {
        use kernel_core::storage::BlockDevice;
        {
            let mem = ctrl.table_mem();
            if let Err(e) = write_ste(&mut *mem, probe_rid, None) {
                bail!(format_args!("revoke refused {:?}", e), NAME_12);
            }
        }
        if let Err(e) = ctrl.invalidate_stream(probe_rid, geom.vmid) {
            bail!(format_args!("invalidate {:?}", e), NAME_12);
        }
        let mut back = [0u8; kernel_core::storage::BLOCK_SIZE];
        let refused = blk.dev.read_block(last, &mut back).is_err() && back != pattern;
        seen.clear();
        let _ = ctrl.drain_events(&mut seen);
        let named = seen
            .iter()
            .any(|e| e.kind == smmu::evt::C_BAD_STE && e.sid == probe_rid);
        if !refused || !named {
            bail!(
                format_args!("refused={} events={}", refused, seen.len()),
                NAME_12
            );
        }
    }
    pass!(NAME_12);

    n += 1;
    const NAME_13: &str =
        "restoring the grant brings the device back: the same bytes, and the unit quiet again";
    {
        use kernel_core::storage::BlockDevice;
        {
            let mem = ctrl.table_mem();
            if let Err(e) = write_ste(&mut *mem, probe_rid, Some(&ste)) {
                bail!(format_args!("restore refused {:?}", e), NAME_13);
            }
        }
        if let Err(e) = ctrl.invalidate_stream(probe_rid, geom.vmid) {
            bail!(format_args!("invalidate {:?}", e), NAME_13);
        }
        if let Err(e) = blk.dev.reset() {
            bail!(format_args!("device reset {}", e), NAME_13);
        }
        let mut back = [0u8; kernel_core::storage::BLOCK_SIZE];
        let ok = blk.dev.read_block(last, &mut back).is_ok() && back == pattern;
        seen.clear();
        let quiet = ctrl.drain_events(&mut seen).is_ok() && seen.is_empty();
        if !ok || !quiet {
            bail!(
                format_args!("restored={} events={}", ok, seen.len()),
                NAME_13
            );
        }
    }
    pass!(NAME_13);

    kprintln!("[smmu] translation REMAINS ON from here to halt");
    Ok(n as u32)
}
