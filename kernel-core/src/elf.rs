//! Which ELF64 executables this kernel will run, judged without touching hardware (ADR-201).
//!
//! A program reaches the machine as bytes in the namespace. Before a single byte is mapped, this
//! module decides whether those bytes describe a program the target can place: an ELF64,
//! little-endian, `ET_EXEC` image for THIS CPU, with exactly one loadable segment that is readable
//! and executable but never writable, sitting at the target's user code address and no larger than
//! one page, entered inside itself. Everything else is refused by name. The judgement is total on
//! any byte slice: every read is bounds-checked, every sum checked.
//!
//! [`build`] is the other half: it writes the smallest image [`judge`] accepts around a code blob,
//! so the machine can seed a real program into its own namespace without a cross toolchain.
//! ponytail: one segment, no relocations, no data segment, one page; a userland crate built by a
//! cross toolchain is the upgrade path once every CI runner carries the three bare-metal targets.

use alloc::vec::Vec;

/// The CPUs this kernel boots on, by their ELF `e_machine` numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Machine {
    Aarch64,
    Riscv64,
    X86_64,
}

impl Machine {
    /// `EM_AARCH64` 183, `EM_RISCV` 243, `EM_X86_64` 62.
    pub const fn e_machine(self) -> u16 {
        match self {
            Machine::Aarch64 => 183,
            Machine::Riscv64 => 243,
            Machine::X86_64 => 62,
        }
    }
}

/// Where a target places a program: its CPU and the one user code page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub machine: Machine,
    pub code_va: u64,
}

/// A program the target can place: the segment's bytes from the file, where they go, and where
/// execution starts. Bytes past `code.len()` up to the page are zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement<'a> {
    pub code: &'a [u8],
    pub vaddr: u64,
    pub entry: u64,
}

/// Why bytes are not a program this machine runs. Each renders to a line an operator can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Shorter than an ELF64 header.
    TooShort,
    /// No `\x7fELF` magic.
    NotElf,
    /// Not ELFCLASS64, not little-endian, or not version 1.
    NotElf64Le,
    /// Not `ET_EXEC` (shared objects and relocatables need a loader this kernel does not have).
    NotExecutable,
    /// Built for another CPU; carries the `e_machine` found.
    WrongMachine(u16),
    /// The program-header table is malformed or runs past the file.
    BadHeaderTable,
    /// Not exactly one `PT_LOAD` segment.
    SegmentCount,
    /// The segment is writable and executable at once.
    WritableAndExecutable,
    /// The segment is not executable.
    NotExecutableSegment,
    /// The segment is not at the target's user code address, or its offset and address disagree
    /// modulo the alignment.
    WrongAddress,
    /// The segment's memory is larger than one page, or its file part larger than its memory.
    TooLarge,
    /// The segment's file bytes run past the end of the file.
    OutsideFile,
    /// The entry point is not inside the segment's file bytes.
    EntryOutside,
}

impl Refusal {
    pub const fn describe(self) -> &'static str {
        match self {
            Refusal::TooShort => "shorter than an ELF64 header",
            Refusal::NotElf => "not an ELF image",
            Refusal::NotElf64Le => "not a 64-bit little-endian ELF",
            Refusal::NotExecutable => "not an ET_EXEC executable",
            Refusal::WrongMachine(_) => "built for another CPU",
            Refusal::BadHeaderTable => "malformed program-header table",
            Refusal::SegmentCount => "not exactly one loadable segment",
            Refusal::WritableAndExecutable => "a segment is writable and executable",
            Refusal::NotExecutableSegment => "the segment is not executable",
            Refusal::WrongAddress => "the segment is not at this machine's user code address",
            Refusal::TooLarge => "the segment is larger than one page",
            Refusal::OutsideFile => "the segment runs past the end of the file",
            Refusal::EntryOutside => "the entry point is outside the segment",
        }
    }
}

const PAGE: u64 = 4096;
const EHDR: usize = 64;
const PHDR: usize = 56;
const ET_EXEC: u16 = 2;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(off..off.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(off..off.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(off..off.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// Decide whether `bytes` is a program `target` can place, and where.
pub fn judge(bytes: &[u8], target: Target) -> Result<Placement<'_>, Refusal> {
    if bytes.get(..4) != Some(b"\x7fELF".as_slice()) {
        return Err(Refusal::NotElf);
    }
    if bytes.len() < EHDR {
        return Err(Refusal::TooShort);
    }
    if bytes[4] != 2 || bytes[5] != 1 || bytes[6] != 1 {
        return Err(Refusal::NotElf64Le);
    }
    let bad = Refusal::BadHeaderTable;
    if u16_at(bytes, 16).ok_or(bad)? != ET_EXEC {
        return Err(Refusal::NotExecutable);
    }
    let machine = u16_at(bytes, 18).ok_or(bad)?;
    if machine != target.machine.e_machine() {
        return Err(Refusal::WrongMachine(machine));
    }
    let entry = u64_at(bytes, 24).ok_or(bad)?;
    let phoff = usize::try_from(u64_at(bytes, 32).ok_or(bad)?).map_err(|_| bad)?;
    if usize::from(u16_at(bytes, 54).ok_or(bad)?) != PHDR {
        return Err(bad);
    }
    let phnum = usize::from(u16_at(bytes, 56).ok_or(bad)?);
    let table_end = phnum
        .checked_mul(PHDR)
        .and_then(|n| n.checked_add(phoff))
        .ok_or(bad)?;
    if table_end > bytes.len() {
        return Err(bad);
    }

    let mut load = None;
    for i in 0..phnum {
        let ph = phoff + i * PHDR;
        if u32_at(bytes, ph).ok_or(bad)? != PT_LOAD {
            continue;
        }
        if load.is_some() {
            return Err(Refusal::SegmentCount);
        }
        load = Some(ph);
    }
    let ph = load.ok_or(Refusal::SegmentCount)?;
    let flags = u32_at(bytes, ph + 4).ok_or(bad)?;
    if flags & PF_W != 0 && flags & PF_X != 0 {
        return Err(Refusal::WritableAndExecutable);
    }
    if flags & PF_X == 0 || flags & PF_R == 0 {
        return Err(Refusal::NotExecutableSegment);
    }
    let offset = u64_at(bytes, ph + 8).ok_or(bad)?;
    let vaddr = u64_at(bytes, ph + 16).ok_or(bad)?;
    let filesz = u64_at(bytes, ph + 32).ok_or(bad)?;
    let memsz = u64_at(bytes, ph + 40).ok_or(bad)?;
    let align = u64_at(bytes, ph + 48).ok_or(bad)?;
    if vaddr != target.code_va || (align > 1 && offset % align != vaddr % align) {
        return Err(Refusal::WrongAddress);
    }
    if memsz > PAGE || filesz > memsz {
        return Err(Refusal::TooLarge);
    }
    let start = usize::try_from(offset).map_err(|_| Refusal::OutsideFile)?;
    let end = start
        .checked_add(filesz as usize)
        .ok_or(Refusal::OutsideFile)?;
    let code = bytes.get(start..end).ok_or(Refusal::OutsideFile)?;
    if entry < vaddr || entry >= vaddr + filesz {
        return Err(Refusal::EntryOutside);
    }
    Ok(Placement { code, vaddr, entry })
}

/// The smallest image [`judge`] accepts: header, one program header, then `code`, all in one
/// read+execute segment loaded at `target.code_va` from file offset 0, entered at the first byte of
/// `code`. `code` plus the 120 header bytes must fit in one page.
pub fn build(target: Target, code: &[u8]) -> Vec<u8> {
    let head = (EHDR + PHDR) as u64;
    let total = head + code.len() as u64;
    let mut b = Vec::with_capacity(total as usize);
    // e_ident: magic, ELFCLASS64, ELFDATA2LSB, EV_CURRENT, System V ABI, padding.
    b.extend_from_slice(b"\x7fELF");
    b.extend_from_slice(&[2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    b.extend_from_slice(&ET_EXEC.to_le_bytes());
    b.extend_from_slice(&target.machine.e_machine().to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes()); // e_version
    b.extend_from_slice(&(target.code_va + head).to_le_bytes()); // e_entry
    b.extend_from_slice(&(EHDR as u64).to_le_bytes()); // e_phoff
    b.extend_from_slice(&0u64.to_le_bytes()); // e_shoff
    b.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    b.extend_from_slice(&(EHDR as u16).to_le_bytes()); // e_ehsize
    b.extend_from_slice(&(PHDR as u16).to_le_bytes()); // e_phentsize
    b.extend_from_slice(&1u16.to_le_bytes()); // e_phnum
    b.extend_from_slice(&[0; 6]); // e_shentsize, e_shnum, e_shstrndx
    b.extend_from_slice(&PT_LOAD.to_le_bytes());
    b.extend_from_slice(&(PF_R | PF_X).to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes()); // p_offset
    b.extend_from_slice(&target.code_va.to_le_bytes()); // p_vaddr
    b.extend_from_slice(&target.code_va.to_le_bytes()); // p_paddr
    b.extend_from_slice(&total.to_le_bytes()); // p_filesz
    b.extend_from_slice(&total.to_le_bytes()); // p_memsz
    b.extend_from_slice(&PAGE.to_le_bytes()); // p_align
    b.extend_from_slice(code);
    b
}

/// The program every machine seeds as `hello` (ADR-201): sum 1..=10 in user mode, then exit with
/// the sum, so the exit status (55) is proof the code ran rather than merely loaded. Encodings
/// checked against LLVM's assembler for each CPU.
pub fn hello_code(machine: Machine) -> &'static [u8] {
    match machine {
        // mov x0,#0; mov x1,#10; 1: add x0,x0,x1; subs x1,x1,#1; b.ne 1b; mov x8,#3; svc #0; b .
        Machine::Aarch64 => &[
            0x00, 0x00, 0x80, 0xd2, 0x41, 0x01, 0x80, 0xd2, 0x00, 0x00, 0x01, 0x8b, 0x21, 0x04,
            0x00, 0xf1, 0xc1, 0xff, 0xff, 0x54, 0x68, 0x00, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4,
            0x00, 0x00, 0x00, 0x14,
        ],
        // li a0,0; li a1,10; 1: add a0,a0,a1; addi a1,a1,-1; bnez a1,1b; li a7,3; ecall; j .
        Machine::Riscv64 => &[
            0x13, 0x05, 0x00, 0x00, 0x93, 0x05, 0xa0, 0x00, 0x33, 0x05, 0xb5, 0x00, 0x93, 0x85,
            0xf5, 0xff, 0xe3, 0x9c, 0x05, 0xfe, 0x93, 0x08, 0x30, 0x00, 0x73, 0x00, 0x00, 0x00,
            0x6f, 0x00, 0x00, 0x00,
        ],
        // xor edi,edi; mov ecx,10; 2: add rdi,rcx; dec rcx; jnz 2b; mov eax,3; int 0x80; jmp .
        Machine::X86_64 => &[
            0x31, 0xff, 0xb9, 0x0a, 0x00, 0x00, 0x00, 0x48, 0x01, 0xcf, 0x48, 0xff, 0xc9, 0x75,
            0xf8, 0xb8, 0x03, 0x00, 0x00, 0x00, 0xcd, 0x80, 0xeb, 0xfe,
        ],
    }
}

/// The program every machine seeds as `trap` (ADR-202): its first instruction is architecturally
/// undefined, so running it must cost exactly one task and never the machine.
pub fn trap_code(machine: Machine) -> &'static [u8] {
    match machine {
        // udf #0; b .
        Machine::Aarch64 => &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x14],
        // an all-zero word is defined to be illegal; j .
        Machine::Riscv64 => &[0x00, 0x00, 0x00, 0x00, 0x6f, 0x00, 0x00, 0x00],
        // ud2; jmp .
        Machine::X86_64 => &[0x0f, 0x0b, 0xeb, 0xfe],
    }
}

/// The program every machine seeds as `spin` (ADR-203): a branch to itself. It never yields and
/// never exits, so only the timer can end its slice, and only the slice budget can end it.
pub fn spin_code(machine: Machine) -> &'static [u8] {
    match machine {
        Machine::Aarch64 => &[0x00, 0x00, 0x00, 0x14], // b .
        Machine::Riscv64 => &[0x6f, 0x00, 0x00, 0x00], // j .
        Machine::X86_64 => &[0xeb, 0xfe],              // jmp .
    }
}

/// The status `hello` exits with.
pub const HELLO_STATUS: u64 = 55;
