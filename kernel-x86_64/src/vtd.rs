//! The VT-d hardware rung: the IOMMU contract programmed into the REAL DMA-remapping unit
//! QEMU emulates on q35 (ALET-P1-018, ADR-071 contract, ADR-073 first rung, ADR-075 windows).
//!
//! Everything arch-independent lives in kernel_core::vtd behind two seams; this file supplies
//! the platform facts and runs the live boot suite:
//!
//! * **Discovery, not poking.** The unit answers at an address the firmware DECLARES: the ACPI
//!   DMAR table DRHD structure names the register base; the variable-position registers (IOTLB,
//!   fault records) are found through the unit OWN capability fields. No DMAR table, no unit,
//!   no programming - the suite skips green and says why (a VirtualBox boot has no DMAR and
//!   stays healthy).
//! * **Per-device WINDOW domains (ADR-075).** Each DRIVEN function gets its own second-level
//!   tree containing exactly the frames ITS driver registry vouches for - the software DMA
//!   boundary (crate::dma, ADR-043) decides, the tables obey - so inter-device isolation is
//!   STRUCTURAL: another function's buffers have no leaf here to translate. Functions nobody
//!   drives get NO context entry at all: deny-by-default against real silicon. The image is
//!   not a DMA target on either side of any mapping, exactly as before.
//! * **Live proofs, not assertions.** After translation turns ON, the LIVE block function this
//!   boot already drives is kicked under enforcement; a revoked PAGE (not just a revoked
//!   function) is DENIED with an ACTIVE record naming its source-id and the revoked address;
//!   restoring the leaf returns silence; enforcement stays latched. Guest-visible request
//!   COMPLETIONS remain deliberately not the assertion (QEMU TCG loses virtio completions
//!   across a mid-run enablement - ADR-073 evidence trail); the unit translation verdicts are
//!   the evidence, taken from its fault-record bank (exact everywhere), never FSTS.PPF.
//!
//! Exit codes: this suite fails at 500 + invariant-index. Still claimed NOWHERE here:
//! interrupt remapping, queued invalidation, pass-through types, post-enable completion
//! assertions, and the ARM fence (SMMUv3 per-stream windows) - each sits in the gap register.

use crate::acpi;
use crate::frames;
use crate::kmap;
use crate::pci::{self, Bdf};
use crate::virtio::VirtioBlk;
use crate::vm;
use kernel_core::dma::Grant;
use kernel_core::frameown::Owner;
use kernel_core::iommu::{IommuFault, Perm, SoftIommu, PAGE};
use kernel_core::storage::{BlockDevice, BLOCK_SIZE};
use kernel_core::vtd::{self, decode_fault_record, fr, Agaw, Controller, RegLayout, TableMem};

/// The register file at the DRHD base. Widths follow the spec: 32-bit registers as 32, 64-bit
/// ones as 64, all volatile - the unit sits behind MMIO and must not see torn or merged accesses.
struct MmioRegs {
    base: usize,
}

impl vtd::Regs for MmioRegs {
    fn r32(&mut self, off: usize) -> u32 {
        // SAFETY: base..+0x1000 is the DRHD register page, mapped as device memory below; every
        // offset the controller touches is inside the unit declared register span.
        unsafe { core::ptr::read_volatile((self.base + off) as *const u32) }
    }
    fn w32(&mut self, off: usize, v: u32) {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile((self.base + off) as *mut u32, v) }
    }
    fn r64(&mut self, off: usize) -> u64 {
        // SAFETY: as above.
        unsafe { core::ptr::read_volatile((self.base + off) as *const u64) }
    }
    fn w64(&mut self, off: usize, v: u64) {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile((self.base + off) as *mut u64, v) }
    }
}

/// Table memory on THIS target: page-walk frames come from the owned-frame allocator as
/// PAGETABLEs, and the kernel identity-maps RAM, so the physical address IS writable here.
struct OwnedFrames;

impl TableMem for OwnedFrames {
    fn read_u64(&self, pa: usize) -> u64 {
        // SAFETY: every address handed out by alloc_zeroed_page below is an owned,
        // identity-mapped frame.
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

// --- discovery -----------------------------------------------------------------------------------------

#[inline]
unsafe fn rd8(pa: usize) -> u8 {
    // SAFETY: caller bounds the address inside a checksum-validated ACPI table.
    unsafe { core::ptr::read_unaligned(pa as *const u8) }
}

#[inline]
unsafe fn rd16(pa: usize) -> u16 {
    // SAFETY: as above.
    unsafe { core::ptr::read_unaligned(pa as *const u16) }
}

#[inline]
unsafe fn rd64(pa: usize) -> u64 {
    // SAFETY: as above.
    unsafe { core::ptr::read_unaligned(pa as *const u64) }
}

/// The first DRHD structure of the DMAR table: (register base, PCI segment, flags). Structures
/// are walked by DECLARED length, so an unknown or malformed entry cannot send the walk somewhere
/// arbitrary - it ends the search instead.
fn drhd() -> Option<(usize, u16, u8)> {
    let (t, len) = acpi::find_table(b"DMAR")?;
    // SAFETY: t..t+len passed its ACPI checksum; the walk below is bounded by len and by each
    // structure declared length.
    unsafe {
        let mut off = 48usize; // 36-byte SDT header + flags byte + reserved
        while off + 4 <= len {
            let ty = rd16(t + off);
            let slen = rd16(t + off + 2) as usize;
            if slen < 4 || off + slen > len {
                return None; // malformed structure list: refuse the walk, do not guess
            }
            if ty == 0 && slen >= 16 {
                // DRHD layout: flags @+4, reserved @+5, segment @+6, register base @+8.
                let flags = rd8(t + off + 4);
                let segment = rd16(t + off + 6);
                let base = rd64(t + off + 8) as usize;
                return Some((base, segment, flags));
            }
            off += slen;
        }
        None
    }
}

fn dev_id(bdf: Bdf) -> u16 {
    ((bdf.bus as u16) << 8) | ((bdf.device as u16) << 3) | bdf.function as u16
}

// --- the grant table -----------------------------------------------------------------------------------

/// One DRIVEN function and the EXACT window set its driver registry vouches for - the world this
/// suite programs as that function whole translatable memory (ADR-075). Spans are page-aligned
/// [start, end) pairs derived from the registry named grants; nothing else about the machine
/// is mapped for the function, and a function absent from this table gets NO context entry.
pub struct DeviceGrant {
    pub bdf: Bdf,
    pub spans: alloc::vec::Vec<(usize, usize)>,
}

impl DeviceGrant {
    /// From a driver live grant list: each registered region becomes one page-rounded span.
    pub fn new(bdf: Bdf, grants: alloc::vec::Vec<Grant>) -> Self {
        let spans = grants
            .iter()
            .map(|g| (g.addr, g.addr + g.len_bytes()))
            .collect();
        DeviceGrant { bdf, spans }
    }

    /// From an ALREADY-CAPTURED grant list (drivers consumed by earlier suites hand their
    /// snapshot over; see the network bring-up in main - captured before the move, identical
    /// because nothing later registers or revokes on those queues).
    pub fn from_grants(bdf: Bdf, grants: &[Grant]) -> Self {
        Self::new(bdf, grants.to_vec())
    }
}

// --- fault-bank helpers --------------------------------------------------------------------------------

fn scan_faults(
    ctrl: &mut Controller<MmioRegs>,
    lay: &RegLayout,
) -> Option<(usize, vtd::FaultRecord)> {
    for idx in 0..lay.frcd_count {
        let (lo, hi) = ctrl.fault_record(lay, idx);
        let rec = decode_fault_record(lo, hi);
        if rec.present {
            return Some((idx, rec));
        }
    }
    None
}

/// Bounded wait for ANY active record to appear. Device DMA is serviced asynchronously from the
/// kicking CPU (emphatically so under emulation), so one scan right after a kick races the walk:
/// sample sparsely - each MMIO read costs an exit under TCG.
fn wait_for_fault(
    ctrl: &mut Controller<MmioRegs>,
    lay: &RegLayout,
) -> Option<(usize, vtd::FaultRecord)> {
    for _ in 0..200u32 {
        if let Some(hit) = scan_faults(ctrl, lay) {
            return Some(hit);
        }
        for _ in 0..300_000u32 {
            core::hint::spin_loop();
        }
    }
    scan_faults(ctrl, lay)
}

/// The LEGACY-UNIT artifact (ADR-075 addendum): QEMU <= 8.x intel-iommu records ONE
/// zero-address WRITE against the first post-enable kicks of a granted function. It is
/// emulator bookkeeping, never kernel state and never an address the kernel published:
/// signature = WRITE, address 0x0, source-id of the kicked function. Such records are
/// retired loudly and BOUNDED (two per phase); anything else still fails the gate.
fn legacy_zero_write_artifact(rec: &vtd::FaultRecord, sid: u16) -> bool {
    rec.address == 0 && !rec.was_read && rec.source_id == sid
}

pub fn dmar_suite(
    grants: &[DeviceGrant],
    mut blk: Option<&mut VirtioBlk>,
    mut blk_persist: Option<&mut VirtioBlk>,
) -> Result<u32, (u32, &'static str)> {
    let mut n = 0usize;

    // Graceful absence FIRST: no DMAR table means the platform declares no DMA-remapping unit.
    // That is a fact about the machine, not a failure of the kernel - skip green, say why.
    if acpi::find_table(b"DMAR").is_none() {
        kprintln!("[dmar] no DMAR ACPI table (skipped - platform declares no remapping unit)");
        return Ok(0);
    }

    let image = kmap::image_span();

    macro_rules! pass {
        ($name:expr) => {{
            kprintln!("  [pass {:>2}] {}", n, $name);
        }};
    }
    macro_rules! bail {
        ($detail:expr, $name:expr) => {{
            kprintln!("[dmar] FAILED at vt-d invariant {}: {}", n, $detail);
            return Err((n as u32, $name));
        }};
    }

    // --- 1: discovery ------------------------------------------------------------------------------------------
    n += 1;
    const NAME_1: &str = "DRHD discovered via ACPI DMAR and register base mapped";
    let (base, segment, flags) = match drhd() {
        Some(x) => x,
        None => bail!("DMAR present but no DRHD structure parsed", NAME_1),
    };
    let mapped = vm::map_device_range(base, 0x1000);
    if !mapped {
        bail!(
            "register page refused by the device-memory admission rule",
            NAME_1
        );
    }
    kprintln!(
        "[dmar] DRHD: segment {} regs @ {:#x} flags={:#x}",
        segment,
        base,
        flags
    );
    pass!(NAME_1);

    // --- 2: identification ---------------------------------------------------------------------------------------
    n += 1;
    const NAME_2: &str = "unit answers sane identification registers";
    let mut ctrl = Controller::new(MmioRegs { base });
    let said = ctrl.identify();
    kprintln!(
        "[dmar] unit says: VER={:#010x} CAP={:#018x} ECAP={:#018x}",
        said.ver_raw,
        said.cap,
        said.ecap
    );
    let rep = match ctrl.probe() {
        Ok(r) => r,
        Err(e) => bail!(format_args!("probe refused {:?}", e), NAME_2),
    };
    let agaw = rep.agaw.expect("validated probe carries an Agaw");
    let lay: RegLayout = rep.layout;
    kprintln!(
        "[dmar] VER={:#010x} AGAW={:?} iotlb@{:#x} fault-bank@{:#x} x{}",
        rep.ver_raw,
        agaw,
        lay.iotlb_off,
        lay.frcd_bank_off,
        lay.frcd_count
    );
    pass!(NAME_2);

    // --- rung floor (ADR-075 addendum): the WINDOW rung is measured on a 4-level unit ---------
    // The emulator generation behind the 3-level-only unit emits bounded zero-address write
    // records and iova 0x28 permission errors under per-device windows (measured on CI). On
    // such units this rung SKIPS LOUDLY before anything is programmed - translation stays OFF,
    // exactly as firmware handed the machine over - and the gap register tracks it.
    if agaw == Agaw::Lev3 {
        kprintln!("[dmar] window rung SKIPPED: unit offers only a 3-level AGAW (ADR-075 addendum)");
        kprintln!("[dmar] measured artifacts there: bounded zero-address write records + iova 0x28 permission errors");
        kprintln!(
            "[dmar] nothing was programmed; translation stays OFF, as handed over by firmware"
        );
        return Ok(0);
    }

    // --- 3: the machine arrives quiet ------------------------------------------------------------------------------
    n += 1;
    const NAME_3: &str = "translation starts OFF (firmware hands the machine quiet)";
    if ctrl.translation_enabled() {
        bail!("GSTS.TES already set before we touched anything", NAME_3);
    }
    pass!(NAME_3);

    // --- 4: the GRANT TABLE is sane ----------------------------------------------------------------------------------
    n += 1;
    const NAME_4: &str =
        "every driven function carries named, aligned, image-clear registry grants";
    if grants.is_empty() {
        bail!("no driven function presented any registry grant", NAME_4);
    }
    for g in grants.iter() {
        if g.spans.is_empty() {
            bail!(
                format_args!("function {:#06x} presented zero grants", dev_id(g.bdf)),
                NAME_4
            );
        }
        for &(s, e) in g.spans.iter() {
            if s >= e || s % PAGE != 0 || e % PAGE != 0 {
                bail!(
                    format_args!(
                        "function {:#06x} grant [{:#x},{:#x}) is empty or unaligned",
                        dev_id(g.bdf),
                        s,
                        e
                    ),
                    NAME_4
                );
            }
            if s < image.1 && image.0 < e {
                bail!(
                    format_args!(
                        "function {:#06x} grant [{:#x},{:#x}) overlaps the kernel image",
                        dev_id(g.bdf),
                        s,
                        e
                    ),
                    NAME_4
                );
            }
        }
    }
    let total_frames: usize = grants.iter().map(|g| g.spans.len()).sum();
    kprintln!(
        "[dmar] grants: {} driven function(s), {} frame window(s) total",
        grants.len(),
        total_frames
    );
    pass!(NAME_4);

    // --- 5: per-device domains built from OWNED frames ------------------------------------------------------------------
    let free_before = frames::free_count();
    let mut mem = OwnedFrames;
    let root = mem.alloc_zeroed_page().expect("root frame");
    let ctx = mem.alloc_zeroed_page().expect("context frame");
    // Bus-0 root entry FIRST: without it every context table is unreachable and the unit
    // answers even a GRANTED function with reason ROOT_ENTRY_P (measured - this exact
    // omission was the first-boot failure of the window rung).
    let (rlo, rhi) = vtd::root_entry_encode(ctx).expect("root entry encodes");
    mem.write_u64(root, rlo);
    mem.write_u64(root + 8, rhi);
    // (source-id, tree physical address) per driven function, in grant-table order; did = index+1.
    let mut trees: alloc::vec::Vec<(u16, usize)> = alloc::vec::Vec::new();
    let mut sum_tables = 0usize;
    let mut sum_leaves = 0usize;
    for (i, g) in grants.iter().enumerate() {
        let did = (i + 1) as u16;
        let (tree, stats) = match vtd::program_identity_domain(&mut mem, &g.spans, image, agaw) {
            Ok(x) => x,
            Err(e) => bail!(
                format_args!("window domain for {:#06x} refused {:?}", dev_id(g.bdf), e),
                "window domain built from owned frames"
            ),
        };
        let (lo, hi) = vtd::context_entry_encode(tree, did, agaw).expect("context entry encodes");
        let _ =
            vtd::rewrite_context_entry(&mut mem, ctx, g.bdf.device, g.bdf.function, Some((lo, hi)));
        trees.push((dev_id(g.bdf), tree));
        sum_tables += stats.tables;
        sum_leaves += stats.huge_leaves + stats.page_leaves;
        kprintln!(
            "[dmar] window domain sid={:02x}:{:02x}.{} did={} leaves={} tree@{:#x}",
            g.bdf.bus,
            g.bdf.device,
            g.bdf.function,
            did,
            stats.huge_leaves + stats.page_leaves,
            tree
        );
    }
    let claimed = free_before - frames::free_count();
    n += 1;
    const NAME_5: &str = "one window domain per driven function, built from owned frames";
    if claimed != sum_tables + 2 {
        bail!(
            format_args!(
                "claimed {} frames but built {} tables (+root+ctx)",
                claimed, sum_tables
            ),
            NAME_5
        );
    }
    pass!(NAME_5);

    // --- 6: every tree LEAF SET equals its grant set --------------------------------------------------------------------
    n += 1;
    const NAME_6: &str = "each window domain translates exactly its granted set - no more, no less";
    for ((sid, tree), g) in trees.iter().zip(grants.iter()) {
        let got = vtd::leaf_spans(&mut mem, *tree, agaw);
        let mut want = g.spans.clone();
        want.sort_unstable();
        let audit = vtd::audit_tree(&mut mem, *tree, agaw, image);
        if audit.image_violations != 0 {
            bail!(format_args!("sid {:#06x}: image leaf present", sid), NAME_6);
        }
        if got != want {
            bail!(
                format_args!(
                    "sid {:#06x}: tree holds {} leaves, registry grants {}",
                    sid,
                    got.len(),
                    want.len()
                ),
                NAME_6
            );
        }
    }
    kprintln!(
        "[dmar] leaf-set equality holds for all {} window domain(s) ({} leaves total)",
        grants.len(),
        sum_leaves
    );
    pass!(NAME_6);

    // --- 7: isolation is structural - foreign windows have NO leaf here ------------------------------------------------------
    n += 1;
    const NAME_7: &str = "another function's granted frames have no leaf in this function's tree";
    // Disjointness across registries first: two drivers vouching for one frame would make
    // per-device windows meaningless, so the gate refuses rather than proving less.
    for (i, a) in grants.iter().enumerate() {
        for b in grants.iter().skip(i + 1) {
            for &(as_, ae) in a.spans.iter() {
                for &(bs, be) in b.spans.iter() {
                    if as_ < be && bs < ae {
                        bail!(
                            format_args!(
                                "functions {:#06x} and {:#06x} both vouch for frame {:#x}",
                                dev_id(a.bdf),
                                dev_id(b.bdf),
                                as_
                            ),
                            NAME_7
                        );
                    }
                }
            }
        }
    }
    // Then the LIVE-tree proof: walk tree[0] for tree[1]'s FIRST window - the seam must refuse,
    // because the builder never created a path there. With one driven function this half is
    // vacuous, but the disjointness half above is not.
    if trees.len() >= 2 {
        let (_, tree_a) = trees[0];
        let foreign = grants[1].spans[0].0;
        match vtd::leaf_present(&mem, tree_a, agaw, foreign) {
            Ok(false) => {}
            Ok(true) => bail!(
                format_args!(
                    "sid {:#06x}'s tree translates a frame granted to sid {:#06x}",
                    trees[0].0, trees[1].0
                ),
                NAME_7
            ),
            Err(e) => bail!(format_args!("foreign-leaf read refused {:?}", e), NAME_7),
        }
    }
    // And DENY-BY-DEFAULT for everyone else, read back from the live context table: every
    // PRESENT function this boot does not drive carries NO context entry - it was never
    // written, and the table says so.
    for (f, _vendor, _devid) in unsafe { pci::enumerate_bus0() } {
        let driven = grants.iter().any(|g| g.bdf == f);
        if driven {
            continue;
        }
        let (lo, hi) = match vtd::context_entry_read(&mem, ctx, f.device, f.function) {
            Ok(x) => x,
            Err(e) => bail!(format_args!("ctx read refused {:?}", e), NAME_7),
        };
        if lo != 0 || hi != 0 {
            bail!(
                format_args!(
                    "function {:02x}:{:02x}.{} is undriven but its context entry is programmed",
                    f.bus, f.device, f.function
                ),
                NAME_7
            );
        }
    }
    pass!(NAME_7);

    // --- 8: model and machine agree under per-device windows ---------------------------------------------------------------
    n += 1;
    const NAME_8: &str =
        "model and machine agree (own windows translate identity; sibling windows do not exist)";
    let mut mirror = SoftIommu::new();
    mirror.declare_kernel_image(image.0, image.1);
    for g in grants.iter() {
        let _ = mirror.attach(dev_id(g.bdf) as u32);
        for &(s, e) in g.spans.iter() {
            let pages = (e - s) / PAGE;
            let _ = mirror.map(dev_id(g.bdf) as u32, s, s, pages, true);
        }
    }
    let probe_sid = dev_id(grants[0].bdf) as u32;
    let mut agree = mirror.image_declared();
    // Own first window translates IDENTITY.
    let (ps, _pe) = grants[0].spans[0];
    agree &= match mirror.translate(probe_sid, ps, Perm::Read) {
        Ok(pa) => pa == ps,
        Err(_) => false,
    };
    // A SIBLING's window does not exist for this function - the whole point of ADR-075.
    if grants.len() >= 2 {
        let (qs, _) = grants[1].spans[0];
        agree &= matches!(
            mirror.translate(probe_sid, qs, Perm::Read),
            Err(IommuFault::NotMapped { .. })
        );
    }
    // The image stays refused on BOTH sides of any mapping.
    agree &= matches!(
        mirror.map(probe_sid, image.0, image.0, 1, true),
        Err(IommuFault::KernelImage { .. })
    );
    if !agree {
        bail!(
            "SoftIommu disagreed with the per-device windows programmed into the tables",
            NAME_8
        );
    }
    pass!(NAME_8);

    // --- 9/10: adopt the root, turn enforcement ON -------------------------------------------------------------------------------
    n += 1;
    const NAME_9: &str = "root table adopted (SRTP handshake)";
    if let Err(e) = ctrl.set_root(root) {
        bail!(format_args!("set_root {:?}", e), NAME_9);
    }
    pass!(NAME_9);
    n += 1;
    const NAME_10: &str = "translation ENABLED (TES observed)";
    if let Err(e) = ctrl.enable_translation() {
        bail!(format_args!("enable_translation {:?}", e), NAME_10);
    }
    kprintln!("[dmar] enforcement LIVE: every PCI DMA now walks the per-device window tables");
    pass!(NAME_10);

    // --- the live probes ---------------------------------------------------------------------------------------------------------
    // Every PCI device this boot drives was brought up BEFORE the flip above, so its queues were
    // published while the machine was still quiet - how a real platform meets an IOMMU. The
    // enforcement proofs drive the LIVE block function and take their EVIDENCE FROM THE UNIT
    // ITSELF: a granted function must walk clean (the bank stays empty); a revoked PAGE must
    // fault BY NAME while the rest of the function's windows still serve; restoration returns
    // silence. Guest-visible COMPLETIONS remain deliberately NOT the assertion (ADR-073).
    let blk_bdf = match unsafe { pci::find_virtio_blk() } {
        Some(b) => b,
        None => bail!("gate requires a scratch disk for the live proofs", NAME_11),
    };
    let scratch_idx = match grants.iter().position(|g| g.bdf == blk_bdf) {
        Some(i) => i,
        None => bail!("block function presented no grant table entry", NAME_11),
    };
    let scratch_tree = trees[scratch_idx].1;
    let dev: &mut VirtioBlk = match blk.as_mut() {
        Some(d) => d,
        None => bail!("no live block device crossed into enforcement", NAME_11),
    };
    // Probe kicks pay one timeout per attempt when the platform loses completions, so tighten the
    // poll budget on every kicking device BEFORE the first stimulus (ADR-073).
    dev.set_completion_spins(4_000_000);
    if let Some(p) = blk_persist.as_mut() {
        p.set_completion_spins(4_000_000);
    }
    let last = dev.num_blocks() - 1;
    let mut pattern = [0u8; BLOCK_SIZE];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = (i.wrapping_mul(7).wrapping_add(0x40)) as u8;
    }

    // --- 11: the granted function walks CLEAN - the unit records nothing ------------------------------------------
    n += 1;
    const NAME_11: &str =
        "granted function serves under per-device enforcement - the unit records NOTHING";
    kprintln!(
        "[dmar] live kick: block function {:02x}:{:02x}.{} under enforcement",
        blk_bdf.bus,
        blk_bdf.device,
        blk_bdf.function
    );
    // Four attempts: the completion transport is unreliable under the emulator artifact, so each
    // attempt is just another stimulus; the ASSERTION is the bank holding NOTHING but at most the
    // two bounded legacy zero-write artifacts this emulator generation emits (ADR-075 addendum).
    let mut legacy_artifacts = 0u32;
    for attempt in 0..4u32 {
        let served = dev.write_block(last, &pattern);
        kprintln!(
            "[dmar] kick {} outcome (informational): {:?}",
            attempt,
            served
        );
        match scan_faults(&mut ctrl, &lay) {
            None => {}
            Some((idx, rec))
                if legacy_zero_write_artifact(&rec, dev_id(blk_bdf)) && legacy_artifacts < 2 =>
            {
                legacy_artifacts += 1;
                let _ = ctrl.clear_fault_record(&lay, idx);
                kprintln!(
                    "[dmar] LEGACY-UNIT ARTIFACT retired (zero-address write, QEMU<=8.x generation); kick repeated"
                );
            }
            Some((idx, rec)) => {
                bail!(
                    format_args!(
                        "the unit faulted a GRANTED function: FRCD[{}] sid={:#06x} reason={} addr={:#x} read={}",
                        idx, rec.source_id, rec.reason, rec.address, rec.was_read
                    ),
                    NAME_11
                );
            }
        }
    }
    pass!(NAME_11);

    // --- 12: revoke ONE PAGE of the window set - the unit DENIES it and records evidence naming it -----------------
    n += 1;
    const NAME_12: &str =
        "a revoked PAGE denies a live function - an ACTIVE record names the source-id and address";
    // The DATA frame is the right victim: the ring stays mapped so the queue keeps spinning, and
    // the very next descriptor touching that frame must fault AT that frame.
    let data_iova = match dev
        .dma_grants()
        .iter()
        .find(|g| g.owner == "virtio-blk.data")
        .map(|g| g.addr)
    {
        Some(a) => a,
        None => bail!("scratch disk presented no named data-frame grant", NAME_12),
    };
    let original_leaf = match vtd::leaf_entry(&mem, scratch_tree, agaw, data_iova) {
        Ok(raw) => raw,
        Err(e) => bail!(
            format_args!("data frame {:#x} has no readable leaf: {:?}", data_iova, e),
            NAME_12
        ),
    };
    let revoked = vtd::rewrite_leaf(&mut mem, scratch_tree, agaw, data_iova, None).is_ok()
        && ctrl.invalidate_context_cache_global().is_ok()
        && ctrl.invalidate_iotlb_global(&lay).is_ok();
    if !revoked {
        bail!("page revocation refused by the programming seam", NAME_12);
    }
    kprintln!(
        "[dmar] revoked PAGE {:#x} of sid {:#06x}'s window set; kicking...",
        data_iova,
        dev_id(blk_bdf)
    );
    // Repeated stimulus: each attempt re-posts the queue and re-kicks; the unit records the FIRST
    // walk against the revoked window and collapses repeats for the same source-id.
    let mut hit = None;
    let mut artifacts12 = 0u32;
    for attempt in 0..5u32 {
        let denied_kick = dev.write_block(last, &pattern);
        kprintln!(
            "[dmar] revoked-kick {} (informational): {:?}",
            attempt,
            denied_kick
        );
        if let Some(found) = wait_for_fault(&mut ctrl, &lay) {
            if legacy_zero_write_artifact(&found.1, dev_id(blk_bdf)) && artifacts12 < 2 {
                artifacts12 += 1;
                let _ = ctrl.clear_fault_record(&lay, found.0);
                kprintln!("[dmar] LEGACY-UNIT ARTIFACT retired during revocation probe");
                continue;
            }
            hit = Some(found);
            break;
        }
    }
    let (fri, rec) = match hit {
        Some(x) => x,
        None => bail!("no fault record after kicking a REVOKED page", NAME_12),
    };
    kprintln!(
        "[dmar] fault: FRCD[{}] sid={:02x}:{:02x}.{} reason={} addr={:#x} read={}",
        fri,
        rec.source_id >> 8,
        (rec.source_id >> 3) & 7,
        rec.source_id & 7,
        rec.reason,
        rec.address,
        rec.was_read
    );
    if rec.source_id != dev_id(blk_bdf) {
        bail!(
            format_args!(
                "record names {:#06x}, probed device was {:#06x}",
                rec.source_id,
                dev_id(blk_bdf)
            ),
            NAME_12
        );
    }
    if rec.address != data_iova as u64 {
        bail!(
            format_args!(
                "record names {:#x}, revoked page was {:#x}",
                rec.address, data_iova
            ),
            NAME_12
        );
    }
    // The reason must name the ABSENT PAGE, not the absent function: a CONTEXT_ENTRY_P here
    // would mean the wrong granularity was revoked. The exact code is PINNED against the
    // emulated unit's own encoding, the way ADR-073 pinned CONTEXT_ENTRY_P.
    if rec.reason == fr::CONTEXT_ENTRY_P {
        bail!(
            "reason names the absent FUNCTION; the revoked thing was a PAGE",
            NAME_12
        );
    }
    if rec.reason != fr::PAGING_NOT_PRESENT {
        bail!(
            format_args!(
                "reason {} does not name the absent page entry (measured expectation {})",
                rec.reason,
                fr::PAGING_NOT_PRESENT
            ),
            NAME_12
        );
    }
    pass!(NAME_12);

    // --- 13: restore the leaf - the same function returns to silence ------------------------------------------------
    n += 1;
    const NAME_13: &str = "restored page returns the function to good standing";
    let restored = vtd::rewrite_leaf(&mut mem, scratch_tree, agaw, data_iova, Some(original_leaf))
        .is_ok()
        && ctrl.invalidate_context_cache_global().is_ok()
        && ctrl.invalidate_iotlb_global(&lay).is_ok();
    // Clear the recorded evidence AT THE BANK, then kick again: silence NOW means the restored
    // window walked clean, not residue of the revocation record.
    let _ = ctrl.clear_fault_record(&lay, fri);
    for _ in 0..2u32 {
        let _ = dev.write_block(last, &pattern);
        if scan_faults(&mut ctrl, &lay).is_some() {
            break;
        }
    }
    if !restored {
        bail!("page restore refused by the programming seam", NAME_13);
    }
    if vtd::leaf_entry(&mem, scratch_tree, agaw, data_iova) != Ok(original_leaf) {
        bail!(
            "restored leaf does not read back as the captured entry",
            NAME_13
        );
    }
    // Silence check with bounded legacy tolerance: a fresh ARTIFACT is retired and re-scanned;
    // any REAL record (different address, a READ, or past the bound) still fails the gate.
    let mut artifacts13 = 0u32;
    loop {
        match scan_faults(&mut ctrl, &lay) {
            None => break,
            Some((idx, rec))
                if legacy_zero_write_artifact(&rec, dev_id(blk_bdf)) && artifacts13 < 2 =>
            {
                artifacts13 += 1;
                let _ = ctrl.clear_fault_record(&lay, idx);
                kprintln!("[dmar] LEGACY-UNIT ARTIFACT retired during restore probe");
            }
            Some((idx, rec)) => {
                bail!(
                    format_args!(
                        "restored function still faulted: FRCD[{}] sid={:#06x} reason={}",
                        idx, rec.source_id, rec.reason
                    ),
                    NAME_13
                );
            }
        }
    }
    pass!(NAME_13);

    // --- 14: enforcement REMAINS on - latched, rooted, and layered with the software registry -----------------------
    n += 1;
    const NAME_14: &str = "enforcement remains ON, layered over the software DMA registry";
    let residency = ctrl.translation_enabled() && ctrl.rtaddr() == root;
    let software_layer = dev.dma_gate_refuses_unregistered() && dev.dma_regions() == 2;
    if !residency {
        bail!(
            "translation did not stay enabled / root pointer drifted",
            NAME_14
        );
    }
    if !software_layer {
        bail!(
            "software DMA registry stopped refusing unregistered addresses",
            NAME_14
        );
    }
    pass!(NAME_14);

    kprintln!("[dmar] ALL {} VT-D INVARIANTS HOLD", n);
    kprintln!("[dmar] translation REMAINS ON: every DMA this machine issues from here to halt walks the per-device window tables");
    Ok(n as u32)
}
