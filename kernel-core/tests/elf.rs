//! The ELF judgement (ADR-201): the seeded program is accepted on its own CPU and every other
//! shape is refused by name.

use kernel_core::elf::{
    build, hello_code, judge, trap_code, Machine, Refusal, Target, HELLO_STATUS,
};

const A64: Target = Target {
    machine: Machine::Aarch64,
    code_va: 0x5000_0000,
};
const RV: Target = Target {
    machine: Machine::Riscv64,
    code_va: 0x5000_0000,
};
const X86: Target = Target {
    machine: Machine::X86_64,
    code_va: 0x4000_0000,
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
    assert_eq!(
        with(&|b| put(b, 68, &6u32.to_le_bytes())),
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
