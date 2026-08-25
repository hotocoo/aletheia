//! The VT-d hardware rung: the IOMMU contract programmed into the REAL DMA-remapping unit
//! QEMU emulates on q35 (ALET-P1-018, ADR-071 contract, ADR-073 delivery).
//!
//! Everything arch-independent lives in `kernel_core::vtd` behind two seams; this file supplies
//! the platform facts and runs the live boot suite:
//!
//! * **Discovery, not poking.** The unit answers at an address the firmware DECLARES: the ACPI
//!   DMAR table's DRHD structure names the register base; the variable-position registers (IOTLB,
//!   fault records) are found through the unit's OWN capability fields. No DMAR table, no unit,
//!   no programming - the suite skips green and says why (a VirtualBox boot has no DMAR and
//!   stays healthy).
//! * **Identity domains over owned frames.** One shared second-level tree translates
//!   conventional RAM minus the kernel image, 1:1, built entirely from frames the ownership
//!   model claims (`Owner::PAGETABLE`). Every present bus-0 function gets a context entry naming
//!   that tree under its own domain id; everything else on the wire faults.
//! * **Live proofs, not assertions.** After translation turns ON, the LIVE functions this boot
//!   already drives are kicked under enforcement, and the EVIDENCE is taken from the unit itself:
//!   a granted function walks clean (the fault-record bank stays empty); a revoked function is
//!   DENIED with an ACTIVE record naming its source-id and the absent context entry; restoration
//!   that function to silence; enforcement stays latched for every later suite this boot runs.
//!   Guest-visible request COMPLETIONS are deliberately not the assertion: QEMU's TCG loses
//!   virtio completions across a mid-run enablement (its static ring caches resolve against the
//!   stale flatview - 'bogus descriptor or out of resources'), which masks END-TO-END transport
//!   but not the unit's own translation verdicts. ADR-073 carries the full evidence trail.
//!
//! Exit codes: this suite fails at 500 + invariant-index. What stays claimed NOWHERE here:
//! inter-device isolation (device-A buffers are translatable for device B until the registry-
//! driven per-device-window rung), interrupt remapping, queued invalidation, pass-through
//! types - each sits in the gap register.

use crate::acpi;
use crate::frames;
use crate::kmap;
use crate::pci::{self, Bdf};
use crate::virtio::VirtioBlk;
use crate::vm;
use kernel_core::frameown::Owner;
use kernel_core::iommu::{IommuFault, Perm, SoftIommu, PAGE};
use kernel_core::storage::{BlockDevice, BLOCK_SIZE};
use kernel_core::vtd::{self, decode_fault_record, fr, Controller, RegLayout, TableMem};
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};

/// The register file at the DRHD base. Widths follow the spec: 32-bit registers as 32, 64-bit
/// ones as 64, all volatile - the unit sits behind MMIO and must not see torn or merged accesses.
struct MmioRegs {
    base: usize,
}

impl vtd::Regs for MmioRegs {
    fn r32(&mut self, off: usize) -> u32 {
        // SAFETY: base..+0x1000 is the DRHD register page, mapped as device memory below; every
        // offset the controller touches is inside the unit's declared register span.
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
    // structure's declared length.
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

/// Conventional RAM spans from the UEFI map, page-aligned, above the same 1 MiB floor the frame
/// allocator uses (so the null page and the real-mode estate are never translatable).
fn conventional_ranges(memory_map: &MemoryMapOwned) -> alloc::vec::Vec<(usize, usize)> {
    const LOW_FLOOR: u64 = 0x10_0000;
    let mut out = alloc::vec::Vec::new();
    for d in memory_map.entries() {
        if d.ty != MemoryType::CONVENTIONAL {
            continue;
        }
        let s = d.phys_start.max(LOW_FLOOR) as usize;
        let e = (d.phys_start + d.page_count * 4096) as usize;
        if s < e && s.is_multiple_of(PAGE) {
            out.push((s, e));
        }
    }
    out
}

/// Split spans around the kernel image. The domain builder REFUSES image-touching input by
/// design (it never decides memory policy silently), so the CALLER hands it exact spans - the
/// same discipline the DMA registry applies at registration time.
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

fn dev_id(bdf: Bdf) -> u16 {
    ((bdf.bus as u16) << 8) | ((bdf.device as u16) << 3) | bdf.function as u16
}

// --- the boot suite ----------------------------------------------------------------------------------------

/// Scan the declared bank for an ACTIVE record (F bit set). The EVIDENCE lives here - the record
/// layout is exact on every implementation - not in FSTS.PPF, whose OFFSET one emulator gets wrong
/// (QEMU serves FSTS at 0x34 where the VT-d spec puts 0x30, so a spec-exact driver reads silence
/// while records pile up; the bank itself is faithful on both).
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

pub fn dmar_suite(
    memory_map: &MemoryMapOwned,
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

    // --- 3: the machine arrives quiet ------------------------------------------------------------------------------
    n += 1;
    const NAME_3: &str = "translation starts OFF (firmware hands the machine quiet)";
    if ctrl.translation_enabled() {
        bail!("GSTS.TES already set before we touched anything", NAME_3);
    }
    pass!(NAME_3);

    // --- 4: build the domain from OWNED frames ----------------------------------------------------------------------
    let ranges = conventional_ranges(memory_map);
    let spans = split_around_image(&ranges, image);
    let free_before = frames::free_count();
    let mut mem = OwnedFrames;
    let (tree, stats) = match vtd::program_identity_domain(&mut mem, &spans, image, agaw) {
        Ok(x) => x,
        Err(e) => bail!(
            format_args!("domain builder refused {:?}", e),
            "domain built from owned frames"
        ),
    };
    // Root table + one bus-0 context table, through the same seam.
    let root = mem.alloc_zeroed_page().expect("root frame");
    let ctx = mem.alloc_zeroed_page().expect("context frame");
    let claimed = free_before - frames::free_count();
    kprintln!(
        "[dmar] domain: {} table frames ({} huge + {} page leaves) over {} spans, image [{:#x},{:#x}) punched out",
        stats.tables, stats.huge_leaves, stats.page_leaves, spans.len(), image.0, image.1
    );
    n += 1;
    const NAME_4: &str = "domain built from owned frames and the counts balance";
    if claimed != stats.tables + 2 || tree % PAGE != 0 {
        bail!(
            format_args!(
                "claimed {} frames but built {} (+root+ctx)",
                claimed, stats.tables
            ),
            NAME_4
        );
    }
    pass!(NAME_4);

    // --- 5: the image has NO leaf -------------------------------------------------------------------------------------
    n += 1;
    const NAME_5: &str = "kernel image has NO leaf (live audit zero violations)";
    let audit = vtd::audit_tree(&mut mem, tree, agaw, image);
    kprintln!(
        "[dmar] audit: {} tables walked, {} huge + {} page leaves, {} IMAGE violations",
        audit.tables,
        audit.huge_leaves,
        audit.page_leaves,
        audit.image_violations
    );
    if audit.image_violations != 0 || audit.huge_leaves + audit.page_leaves == 0 {
        bail!(
            "live tree audit found an image leaf or an empty domain",
            NAME_5
        );
    }
    pass!(NAME_5);

    // --- grant contexts BEFORE enabling ----------------------------------------------------------------------------------
    let funcs = unsafe { pci::enumerate_bus0() };
    if funcs.is_empty() {
        bail!("no PCI functions enumerated to program", NAME_5);
    }
    let (rlo, rhi) = vtd::root_entry_encode(ctx).expect("root entry encodes");
    mem.write_u64(root, rlo);
    mem.write_u64(root + 8, rhi);
    let blk_bdf = unsafe { pci::find_virtio_blk() };
    let persist_bdf = unsafe { pci::find_virtio_blk_nth(1) };
    let mut blk_saved: Option<(u64, u64)> = None;
    let mut persist_saved: Option<(u64, u64)> = None;
    for (did_counter, &(f, vendor, devid)) in (1u16..).zip(funcs.iter()) {
        let (lo, hi) =
            vtd::context_entry_encode(tree, did_counter, agaw).expect("context entry encodes");
        let _ = vtd::rewrite_context_entry(&mut mem, ctx, f.device, f.function, Some((lo, hi)));
        if blk_bdf == Some(f) {
            blk_saved = Some((lo, hi));
        }
        if persist_bdf == Some(f) {
            persist_saved = Some((lo, hi));
        }
        kprintln!(
            "[dmar] context {:02x}:{:02x}.{} did={} vendor={:#06x} devid={:#06x}",
            f.bus,
            f.device,
            f.function,
            did_counter,
            vendor,
            devid
        );
    }

    // --- 6: model and machine agree ------------------------------------------------------------------------------------------
    n += 1;
    const NAME_6: &str = "model and machine agree (image refused on both sides)";
    let mut mirror = SoftIommu::new();
    mirror.declare_kernel_image(image.0, image.1);
    for &(f, _, _) in funcs.iter() {
        let _ = mirror.attach(dev_id(f) as u32);
    }
    let probe_dev = dev_id(funcs[0].0) as u32;
    let mut agree = mirror.image_declared();
    let mut identity_checked = false;
    for &(s, e) in spans.iter().take(4) {
        let pages = ((e - s) / PAGE).clamp(1, 16);
        agree &= mirror.map(probe_dev, s, s, pages, true).is_ok();
        match mirror.translate(probe_dev, s, Perm::Read) {
            Ok(pa) => {
                agree &= pa == s;
                identity_checked = true;
            }
            Err(_) => agree &= false,
        }
    }
    agree &= identity_checked;
    agree &= matches!(
        mirror.map(probe_dev, image.0, image.0, 1, true),
        Err(IommuFault::KernelImage { .. })
    );
    if !agree {
        bail!(
            "SoftIommu disagreed with the shape programmed into the tables",
            NAME_6
        );
    }
    pass!(NAME_6);

    // --- 7/8: adopt the root, turn enforcement ON -------------------------------------------------------------------------------
    n += 1;
    const NAME_7: &str = "root table adopted (SRTP handshake)";
    if let Err(e) = ctrl.set_root(root) {
        bail!(format_args!("set_root {:?}", e), NAME_7);
    }
    pass!(NAME_7);
    n += 1;
    const NAME_8: &str = "translation ENABLED (TES observed)";
    if let Err(e) = ctrl.enable_translation() {
        bail!(format_args!("enable_translation {:?}", e), NAME_8);
    }
    kprintln!("[dmar] enforcement LIVE: every PCI DMA now walks the tables");
    pass!(NAME_8);

    // --- the live probes --------------------------------------------------------------------------------------------
    // Every PCI device this boot drives was brought up BEFORE the flip above, so its queues were
    // published while the machine was still quiet - how a real platform meets an IOMMU. The
    // enforcement proofs below drive those live functions and take their EVIDENCE FROM THE UNIT
    // ITSELF: a granted function must walk clean (the bank stays empty); a revoked function must
    // fault BY NAME (an ACTIVE record naming its source-id and the absent context entry); a
    // restored function must return to silence. Guest-visible request COMPLETIONS are deliberately
    // NOT the assertion: QEMU 11.x TCG loses virtio completions across a mid-run enablement (its
    // static per-device ring caches and bounce maps resolve against the pre/post flip flatviews -
    // 'bogus descriptor or out of resources'), which masks END-TO-END transport but not the unit's
    // own translation verdicts. See ADR-073 for the full evidence trail.
    let blk_bdf = match blk_bdf {
        Some(b) => b,
        None => bail!("gate requires a scratch disk for the live proofs", NAME_9),
    };
    let saved_pair = match blk_saved {
        Some(p) => p,
        None => bail!("block function received no context entry", NAME_9),
    };
    let dev: &mut VirtioBlk = match blk.as_mut() {
        Some(d) => d,
        None => bail!("no live block device crossed into enforcement", NAME_9),
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

    // --- 9: the granted function walks CLEAN - the unit records nothing ------------------------------------------------------------------------
    n += 1;
    const NAME_9: &str = "granted function serves under enforcement - the unit records NOTHING";
    kprintln!(
        "[dmar] live kick: block function {:02x}:{:02x}.{} under enforcement",
        blk_bdf.bus,
        blk_bdf.device,
        blk_bdf.function
    );
    // Two attempts: the completion transport is unreliable under the emulator artifact, so each
    // attempt is just another stimulus; the ASSERTION is the bank's silence throughout.
    for attempt in 0..2u32 {
        let served = dev.write_block(last, &pattern);
        kprintln!(
            "[dmar] kick {} outcome (informational): {:?}",
            attempt,
            served
        );
        if let Some((idx, rec)) = scan_faults(&mut ctrl, &lay) {
            bail!(
                format_args!(
                    "the unit faulted a GRANTED function: FRCD[{}] sid={:#06x} reason={}",
                    idx, rec.source_id, rec.reason
                ),
                NAME_9
            );
        }
    }
    pass!(NAME_9);

    // --- 10: revoke a live function - the unit DENIES it and records evidence naming it ---------------------------------------------------------
    n += 1;
    const NAME_10: &str = "revoked context denies a live function - the unit names it";
    // Prove denial on a function that has NOT been kicked yet (the persistent medium when present,
    // otherwise the scratch disk itself), so the probe starts from a pristine queue state.
    let probe_is_persist = blk_persist.is_some();
    let (probe_dev, probe_bdf, probe_saved): (&mut VirtioBlk, Bdf, (u64, u64)) = if probe_is_persist
    {
        let d = blk_persist.as_mut().unwrap();
        (d, persist_bdf.unwrap(), persist_saved.unwrap())
    } else {
        (dev, blk_bdf, saved_pair)
    };
    let revoked =
        vtd::rewrite_context_entry(&mut mem, ctx, probe_bdf.device, probe_bdf.function, None)
            .is_ok()
            && ctrl.invalidate_context_cache_global().is_ok()
            && ctrl.invalidate_iotlb_global(&lay).is_ok();
    if !revoked {
        bail!(
            "context revocation refused by the programming seam",
            NAME_10
        );
    }
    // Repeated stimulus: each attempt re-posts the queue and re-kicks; the unit records the FIRST
    // walk against the revoked context and collapses repeats for the same source-id.
    let mut hit = None;
    for attempt in 0..3u32 {
        let denied_kick = probe_dev.write_block(last, &pattern);
        kprintln!(
            "[dmar] revoked-kick {} (informational): {:?}",
            attempt,
            denied_kick
        );
        if let Some(found) = wait_for_fault(&mut ctrl, &lay) {
            hit = Some(found);
            break;
        }
    }
    let (fri, rec) = match hit {
        Some(x) => x,
        None => bail!("no fault record after kicking a REVOKED function", NAME_10),
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
    if rec.source_id != dev_id(probe_bdf) {
        bail!(
            format_args!(
                "record names {:#06x}, probed device was {:#06x}",
                rec.source_id,
                dev_id(probe_bdf)
            ),
            NAME_10
        );
    }
    if rec.reason != fr::CONTEXT_ENTRY_P {
        bail!(
            format_args!(
                "reason {} does not name the absent context entry",
                rec.reason
            ),
            NAME_10
        );
    }
    pass!(NAME_10);

    // --- 11: restore the grant - the same function returns to silence ----------------------------------------------------------------------------
    n += 1;
    const NAME_11: &str = "restored grant returns the function to good standing";
    let restored = vtd::rewrite_context_entry(
        &mut mem,
        ctx,
        probe_bdf.device,
        probe_bdf.function,
        Some(probe_saved),
    )
    .is_ok()
        && ctrl.invalidate_context_cache_global().is_ok()
        && ctrl.invalidate_iotlb_global(&lay).is_ok();
    // Clear the recorded evidence AT THE BANK, then kick again: silence NOW means the restored
    // context walked clean, not residue of the revocation record.
    let _ = ctrl.clear_fault_record(&lay, fri);
    for _ in 0..2u32 {
        let _ = probe_dev.write_block(last, &pattern);
        if scan_faults(&mut ctrl, &lay).is_some() {
            break;
        }
    }
    if !restored {
        bail!("context restore refused by the programming seam", NAME_11);
    }
    if let Some((idx, rec)) = scan_faults(&mut ctrl, &lay) {
        bail!(
            format_args!(
                "restored function still faulted: FRCD[{}] sid={:#06x} reason={}",
                idx, rec.source_id, rec.reason
            ),
            NAME_11
        );
    }
    pass!(NAME_11);

    // --- 12: enforcement REMAINS on - latched, rooted, and layered with the software registry ----------------------------------------------------
    n += 1;
    const NAME_12: &str = "enforcement remains ON, layered over the software DMA registry";
    let residency = ctrl.translation_enabled() && ctrl.rtaddr() == root;
    let software_layer = dev.dma_gate_refuses_unregistered() && dev.dma_regions() == 2;
    if !residency {
        bail!(
            "translation did not stay enabled / root pointer drifted",
            NAME_12
        );
    }
    if !software_layer {
        bail!(
            "software DMA registry stopped refusing unregistered addresses",
            NAME_12
        );
    }
    pass!(NAME_12);

    kprintln!("[dmar] ALL {} VT-D INVARIANTS HOLD", n);
    kprintln!("[dmar] translation REMAINS ON: every DMA this machine issues from here to halt walks those tables");
    Ok(n as u32)
}
