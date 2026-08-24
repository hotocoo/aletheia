//! The firmware that ANSWERS: QEMU's fw_cfg config surface, as a transport-agnostic contract
//! (the delivery half of ALET-P1-034, ADR-072).
//!
//! ADR-070 left the vault root in the caller's hands: "custody of the ROOT remains whoever calls
//! open". That was honest but unfinished — a root handed in by the caller is a root nobody
//! delivered. This module is the delivery CHANNEL: QEMU's fw_cfg firmware interface, the same
//! mechanism UEFI itself uses to hand a kernel its boot parameters. It exists on every machine
//! Aletheia boots under (q35 ioports 0x510/0x511; MMIO 0x0902_0000 on virt/aarch64; MMIO
//! 0x1010_0000 on virt/RISC-V — all three verified against the machines' own device trees and
//! ISA defaults), so the custody anchor can cross the PLATFORM boundary instead of being minted
//! from thin air inside the kernel.
//!
//! The protocol (QEMU spec, "fw_cfg"):
//!
//! * write a 16-bit SELECTOR to the selector register;
//! * read data bytes one at a time from the data register until the item is consumed;
//! * selector 0x00 answers the 4-byte signature "QEMU"; selector 0x19 is the file DIRECTORY:
//!   a big-endian u32 entry count, then per entry { BE u32 size | BE u16 selector | BE u16
//!   reserved | 56-byte NUL-padded name }.
//!
//! What makes this fail-closed rather than optimistic:
//!
//! * **A dead bus reads as 0xFF forever.** Every read returns *something*; a signature probe
//!   against absent firmware therefore fails honestly and the caller sees "firmware absent",
//!   never a hang and never a garbage byte pattern accepted as custody.
//! * **A lying directory cannot overread us.** The entry count is bounded; entries are skipped
//!   by DECLARED length; nothing past the modeled data is ever interpreted as structure.
//! * **Sizes are checked before bytes are wanted.** The vault-root consumer refuses any size
//!   other than exactly 32 — this driver merely reports what the firmware declared.
//!
//! Arch-independent by construction: the MMIO/ioport transports live in the target crates behind
//! the tiny FwCfgBus trait, which is also what the host tests implement over plain memory — the
//! same parsing and directory-walk logic is proved on the host today and runs unmodified in
//! kernel space.
//!
//! Not claimed (ADR-072): fw_cfg delivery is a TRUSTED platform channel, not yet a MEASURED one —
//! whoever controls the platform controls the root, exactly as whoever controls firmware does.
//! Attestation / measured-boot anchoring stays REQ-BOOT-001 Phase 2/3 scope.

use alloc::vec::Vec;

/// Selector 0: the 4-byte signature, "QEMU" on real firmware.
pub const FW_CFG_SIGNATURE: u16 = 0x00;
/// Selector 0x19: the file directory — how named items ("opt/...") are found without hard-wiring
/// selector numbers that firmware may reorder.
pub const FW_CFG_FILE_DIR: u16 = 0x19;

/// The only firmware signature this driver accepts.
const SIGNATURE: &[u8; 4] = b"QEMU";
/// One directory entry on the wire: BE32 size + BE16 selector + BE16 reserved + name.
const ENTRY_LEN: usize = 64;
/// The name field inside a directory entry, NUL-padded.
const NAME_LEN: usize = 56;
/// Upper bound on directory entries honored. Real machines carry a few dozen; a corrupted or
/// hostile count must end the scan here rather than loop for hours reading a dead bus.
const MAX_DIR_ENTRIES: u32 = 0x1000;

/// One transport: put a selector in the selector register, pull data bytes out of the data
/// register. Implemented over MMIO (aarch64/RISC-V), ioports (x86-64), and host memory (tests).
pub trait FwCfgBus {
    /// Write the item selector. Only the low 16 bits are meaningful; wider register writes on
    /// 64-bit MMIO transports pass the value zero-extended.
    fn select(&mut self, selector: u16);
    /// Read the next data byte of the selected item. On a bus with no firmware behind it this
    /// returns 0xFF — reads never fail, they return *dead* data the caller's checks reject.
    fn read_byte(&mut self) -> u8;
}

impl<T: FwCfgBus + ?Sized> FwCfgBus for &mut T {
    fn select(&mut self, selector: u16) {
        (**self).select(selector);
    }
    fn read_byte(&mut self) -> u8 {
        (**self).read_byte()
    }
}

/// Big-endian u32 off the data register (directory fields are BE on the wire).
fn read_be_u32(bus: &mut impl FwCfgBus) -> u32 {
    let mut b = [0u8; 4];
    for slot in b.iter_mut() {
        *slot = bus.read_byte();
    }
    u32::from_be_bytes(b)
}

/// Select the item and fill 'out' with its next bytes. Returns bytes filled (= out.len(): the
/// dead-bus rule means reads produce bytes even when nothing is behind them — validation happens
/// against DECLARED sizes, never against read success).
pub fn read_bytes(bus: &mut impl FwCfgBus, selector: u16, out: &mut [u8]) -> usize {
    bus.select(selector);
    let mut n = 0;
    for b in out.iter_mut() {
        *b = bus.read_byte();
        n += 1;
    }
    let _ = n;
    out.len()
}

/// True iff the firmware answers the signature probe with "QEMU". This is the difference between
/// "the platform chose not to hand us a root" and "there is no platform channel at all".
pub fn signature_matches(bus: &mut impl FwCfgBus) -> bool {
    let mut sig = [0u8; 4];
    read_bytes(bus, FW_CFG_SIGNATURE, &mut sig);
    &sig == SIGNATURE
}

/// One file the firmware exposes, found by walking the directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// Declared byte size of the item. AUTHORITATIVE: consumers validate against it before
    /// wanting the bytes.
    pub size: u32,
    /// Selector to hand back before reading the item's bytes.
    pub selector: u16,
}

/// Walk the file directory looking for 'name' (exact match against the NUL-padded field).
/// Returns None when there is no directory, the count lies past its own bound, or the name
/// simply is not there — all the same outcome for the caller: not provided.
pub fn find_file(bus: &mut impl FwCfgBus, name: &[u8]) -> Option<FileEntry> {
    if name.is_empty() || name.len() > NAME_LEN {
        return None;
    }
    if name.len() >= ENTRY_LEN {
        return None;
    }
    bus.select(FW_CFG_FILE_DIR);
    // Directory head: BE32 entry count — entries begin IMMEDIATELY after it (the spec shows
    // no gap; a reserved word read here shifts every later field by four and silently
    // unmatches every name, which is exactly what the live walk caught).
    let count = read_be_u32(bus);
    if count == 0 || count > MAX_DIR_ENTRIES {
        return None;
    }
    for _ in 0..count {
        let size = read_be_u32(bus);
        let sel_hi = bus.read_byte() as u16;
        let sel_lo = bus.read_byte() as u16;
        let selector = (sel_hi << 8) | sel_lo;
        // Reserved pair.
        let _r0 = bus.read_byte();
        let _r1 = bus.read_byte();
        let mut fname = [0u8; NAME_LEN];
        for b in fname.iter_mut() {
            *b = bus.read_byte();
        }
        // Exact match against the NUL-padded field: our name, then padding zeros. A name that
        // merely PREFIXES another ("opt/x" vs "opt/x-longer") must not match.
        let exact = fname[..name.len()] == *name && fname[name.len()..].iter().all(|&b| b == 0);
        if exact {
            return Some(FileEntry { size, selector });
        }
    }
    None
}

/// Convenience wrapper for callers that already hold a FileEntry: select it and fill 'out'.
pub fn read_entry(bus: &mut impl FwCfgBus, entry: &FileEntry, out: &mut [u8]) -> usize {
    read_bytes(bus, entry.selector, out)
}

/// Build a directory model the way firmware lays it out — exposed for tests, which synthesize
/// directories to prove the walker against liars and truncations. Kernels have no reason to
/// BUILD directories, only parse them; this lives behind the test surface convention.
#[doc(hidden)]
pub fn encode_directory_for_test(entries: &[(u32, u16, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    // Entries begin IMMEDIATELY after the count - there is no reserved word on the wire.
    for (size, sel, name) in entries {
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(&sel.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        let mut nm = [0u8; NAME_LEN];
        let take = core::cmp::min(name.len(), NAME_LEN);
        nm[..take].copy_from_slice(&name[..take]);
        out.extend_from_slice(&nm);
    }
    out
}
