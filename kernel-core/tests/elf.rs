//! The ELF judgement (ADR-201): the seeded program is accepted on its own CPU and every other
//! shape is refused by name.

use kernel_core::elf::{
    build, hello_code, judge, trap_code, Machine, Refusal, Target, HELLO_LINE, HELLO_STATUS,
};

const A64: Target = Target {
    machine: Machine::Aarch64,
    code_va: 0x5000_0000,
    data_va: 0x5000_2000,
};
const RV: Target = Target {
    machine: Machine::Riscv64,
    code_va: 0x5000_0000,
    data_va: 0x5000_2000,
};
const X86: Target = Target {
    machine: Machine::X86_64,
    code_va: 0x4000_0000,
    data_va: 0x4000_2000,
};

fn put(b: &mut [u8], off: usize, v: &[u8]) {
    b[off..off + v.len()].copy_from_slice(v);
}

#[test]
fn the_seeded_program_is_accepted_on_its_own_cpu() {
    for t in [A64, RV, X86] {
        let img = build(t, hello_code(t.machine));
        let p = judge(&img, t).expect("hello judged");
        assert_eq!(p.vaddr, t.code_va);
        assert_eq!(p.entry, t.code_va + 120);
        assert_eq!(p.code.len(), img.len(), "the segment is the whole file");
        assert_eq!(&p.code[120..], hello_code(t.machine));
        assert!(
            judge(&build(t, trap_code(t.machine)), t).is_ok(),
            "trap judged"
        );
    }
    assert_eq!(HELLO_STATUS, (1..=10).sum::<u64>());
    for m in [Machine::Aarch64, Machine::Riscv64, Machine::X86_64] {
        assert!(
            hello_code(m).ends_with(HELLO_LINE),
            "hello carries its line"
        );
    }
}

#[test]
fn a_program_for_another_cpu_is_refused_by_name() {
    let img = build(RV, hello_code(Machine::Riscv64));
    assert_eq!(judge(&img, A64), Err(Refusal::WrongMachine(243)));
    let img = build(X86, hello_code(Machine::X86_64));
    assert_eq!(judge(&img, RV), Err(Refusal::WrongMachine(62)));
}

#[test]
fn every_malformed_shape_is_refused_by_name() {
    let good = build(A64, hello_code(Machine::Aarch64));
    let with = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = good.clone();
        f(&mut b);
        judge(&b, A64).map(|_| ())
    };
    assert_eq!(judge(&good[..63], A64).map(|_| ()), Err(Refusal::TooShort));
    assert_eq!(
        judge(
            b"the OS you can sit in front of, and it is text not code, padded to 64+ bytes",
            A64
        ),
        Err(Refusal::NotElf)
    );
    assert_eq!(with(&|b| b[4] = 1), Err(Refusal::NotElf64Le));
    assert_eq!(with(&|b| b[5] = 2), Err(Refusal::NotElf64Le));
    assert_eq!(
        with(&|b| put(b, 16, &3u16.to_le_bytes())),
        Err(Refusal::NotExecutable)
    );
    assert_eq!(
        with(&|b| put(b, 54, &64u16.to_le_bytes())),
        Err(Refusal::BadHeaderTable)
    );
    assert_eq!(
        with(&|b| put(b, 56, &500u16.to_le_bytes())),
        Err(Refusal::BadHeaderTable)
    );
    assert_eq!(
        with(&|b| put(b, 32, &u64::MAX.to_le_bytes())),
        Err(Refusal::BadHeaderTable)
    );
    assert_eq!(
        with(&|b| put(b, 64, &6u32.to_le_bytes())),
        Err(Refusal::SegmentCount)
    );
    assert_eq!(
        with(&|b| put(b, 68, &7u32.to_le_bytes())),
        Err(Refusal::WritableAndExecutable)
    );
    // Read+write only: the image declares a data segment and no code at all (ADR-210).
    assert_eq!(
        with(&|b| put(b, 68, &6u32.to_le_bytes())),
        Err(Refusal::SegmentCount)
    );
    // Read only, neither writable nor executable: not a segment this machine can run.
    assert_eq!(
        with(&|b| put(b, 68, &4u32.to_le_bytes())),
        Err(Refusal::NotExecutableSegment)
    );
    assert_eq!(
        with(&|b| put(b, 64 + 16, &0x6000_0000u64.to_le_bytes())),
        Err(Refusal::WrongAddress)
    );
    assert_eq!(
        with(&|b| put(b, 64 + 8, &4u64.to_le_bytes())),
        Err(Refusal::WrongAddress)
    );
    assert_eq!(
        with(&|b| put(b, 64 + 40, &8192u64.to_le_bytes())),
        Err(Refusal::TooLarge)
    );
    assert_eq!(
        with(&|b| put(b, 64 + 32, &4000u64.to_le_bytes())),
        Err(Refusal::TooLarge)
    );
    assert_eq!(
        with(&|b| {
            put(b, 64 + 32, &4000u64.to_le_bytes());
            put(b, 64 + 40, &4000u64.to_le_bytes());
        }),
        Err(Refusal::OutsideFile)
    );
    assert_eq!(
        with(&|b| put(b, 24, &0x5000_1000u64.to_le_bytes())),
        Err(Refusal::EntryOutside)
    );
}

/// A program may also declare ONE writable page (ADR-210): at the target's data address, readable,
/// never executable, at most a page, with `.bss` past its file bytes. Anything else is refused.
#[test]
fn a_writable_data_segment_is_accepted_only_where_the_target_puts_one() {
    fn with_data(
        t: Target,
        code: &[u8],
        dvaddr: u64,
        filesz: u64,
        memsz: u64,
        flags: u32,
    ) -> Vec<u8> {
        // `build`'s image, then a second program header spliced in and the data bytes appended.
        let mut img = build(t, code);
        let phoff = 64usize;
        let mut ph = vec![0u8; 56];
        ph[0..4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        ph[4..8].copy_from_slice(&flags.to_le_bytes());
        ph[16..24].copy_from_slice(&dvaddr.to_le_bytes());
        ph[24..32].copy_from_slice(&dvaddr.to_le_bytes());
        ph[32..40].copy_from_slice(&filesz.to_le_bytes());
        ph[40..48].copy_from_slice(&memsz.to_le_bytes());
        // p_align 1: this synthetic image does not pad the data to a page boundary, and the judge
        // only demands offset/address congruence when a segment claims an alignment.
        ph[48..56].copy_from_slice(&1u64.to_le_bytes());
        img.splice(phoff + 56..phoff + 56, ph);
        let data_off = img.len() as u64;
        img.extend(core::iter::repeat_n(0xab, filesz as usize));
        img[56..58].copy_from_slice(&2u16.to_le_bytes()); // e_phnum
        let second = phoff + 56;
        img[second + 8..second + 16].copy_from_slice(&data_off.to_le_bytes()); // p_offset
        let first = phoff;
        let text = data_off;
        img[first + 32..first + 40].copy_from_slice(&text.to_le_bytes()); // p_filesz
        img[first + 40..first + 48].copy_from_slice(&text.to_le_bytes()); // p_memsz
        img[24..32].copy_from_slice(&(t.code_va + 176).to_le_bytes()); // e_entry, past both headers
        img
    }
    let code = hello_code(Machine::Aarch64);
    let good = with_data(A64, code, A64.data_va, 8, 64, 6);
    let p = judge(&good, A64).expect("data segment judged");
    assert_eq!(p.data, &[0xab; 8]);
    assert_eq!(p.data_memsz, 64, "the .bss past the file bytes is declared");
    assert_eq!(p.vaddr, A64.code_va);

    for (dvaddr, filesz, memsz, flags, want) in [
        (
            A64.data_va,
            8u64,
            64u64,
            7u32,
            Refusal::WritableAndExecutable,
        ),
        (A64.code_va, 8, 64, 6, Refusal::WrongAddress),
        (A64.data_va + 8, 8, 64, 6, Refusal::WrongAddress),
        (A64.data_va, 8, 8192, 6, Refusal::TooLarge),
        (A64.data_va, 64, 8, 6, Refusal::TooLarge),
    ] {
        assert_eq!(
            judge(&with_data(A64, code, dvaddr, filesz, memsz, flags), A64).map(|_| ()),
            Err(want),
            "{dvaddr:#x} {filesz} {memsz} {flags:#x}"
        );
    }
    let bare = build(A64, code);
    let plain = judge(&bare, A64).unwrap();
    assert!(plain.data.is_empty() && plain.data_memsz == 0);
}

/// Seeded mutation campaign in the shape of `hostile_page.rs`: flip, truncate and splice the
/// seeded image thousands of times. The judgement never panics, and whatever it accepts is placed
/// inside one page at the code address, entered inside its own bytes, and never writable.
#[test]
fn hostile_images_never_panic_and_accepted_ones_stay_in_bounds() {
    let seed: u64 = std::env::var("ELF_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xE1F);
    let mut x = seed | 1;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut accepted = 0;
    for t in [A64, RV, X86] {
        let good = build(t, hello_code(t.machine));
        for _ in 0..20_000 {
            let mut b = good.clone();
            for _ in 0..(1 + next() % 4) {
                match next() % 3 {
                    0 if !b.is_empty() => {
                        let i = (next() as usize) % b.len();
                        b[i] = next() as u8;
                    }
                    0 => {}
                    1 => b.truncate((next() as usize) % (b.len() + 1)),
                    _ => {
                        let i = (next() as usize) % (b.len() + 1);
                        let v = next().to_le_bytes();
                        b.splice(i..i, v);
                    }
                }
            }
            if let Ok(p) = judge(&b, t) {
                accepted += 1;
                assert_eq!(p.vaddr, t.code_va, "seed {seed}");
                assert!(p.code.len() <= 4096, "seed {seed}");
                assert!(
                    p.entry >= p.vaddr && p.entry < p.vaddr + p.code.len() as u64,
                    "seed {seed}"
                );
            }
        }
    }
    assert!(
        accepted > 0,
        "the campaign never exercised the accepting path"
    );
}
