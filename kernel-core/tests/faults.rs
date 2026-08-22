//! Fault-injection proofs for the durability protocols (ALET-P2-008, ADR-062).
//!
//! The journal's contract is all-or-nothing across a crash, and "crash" is not one scenario -- it
//! is a device refusing at ANY position of the protocol. These proofs place a refusal at EVERY
//! position of a commit and of a recovery, exhaustively for the swept transaction sizes, and hold
//! the protocol to exactly its promised outcomes. A fault the protocol cannot survive, or an error
//! it swallows, fails here on the host -- no QEMU required.
extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use kernel_core::faultdev::{FaultInject, Op};
use kernel_core::storage::{
    BlockDevice, Journal, MemBlockDevice, StorageError, BLOCK_SIZE, DATA_START,
};

const BLOCKS: usize = DATA_START + 8;

fn block(seed: u8) -> [u8; BLOCK_SIZE] {
    let mut b = [0u8; BLOCK_SIZE];
    for (i, x) in b.iter_mut().enumerate() {
        *x = seed.wrapping_add(i as u8);
    }
    b
}

/// The two-op transaction every sweep uses: homes at DATA_START and DATA_START+1.
fn two_updates() -> Vec<(usize, [u8; BLOCK_SIZE])> {
    vec![(DATA_START, block(0xAA)), (DATA_START + 1, block(0xBB))]
}

/// The commit protocol's op sequence for a 2-update transaction, as storage.rs orders it:
/// 2 journal writes, flush, commit-record write, flush (the pivot), 2 home writes, flush.
const COMMIT_OPS: usize = 2 + 1 + 1 + 1 + 2 + 1;

/// A script of length COMMIT_OPS with exactly ONE refusal at position 'at' (0-based, protocol
/// order) -- the exhaustive way to say "the device died here".
fn die_at(at: usize) -> Vec<Op> {
    let mut s = Vec::new();
    for i in 0..COMMIT_OPS {
        let op = match i {
            0 | 1 => Op::WriteOk, // journal payload writes
            2 => Op::FlushOk,     // journal flush
            3 => Op::WriteOk,     // commit-record write
            4 => Op::FlushOk,     // THE PIVOT flush
            5 | 6 => Op::WriteOk, // home writes
            _ => Op::FlushOk,     // final flush
        };
        if i == at {
            s.push(match op {
                Op::WriteOk => Op::WriteFail,
                Op::FlushOk => Op::FlushFail,
                other => other,
            });
        } else {
            s.push(op);
        }
    }
    s
}

#[test]
fn a_refusal_at_every_commit_position_leaves_old_or_new_never_a_mixture() {
    let old = [block(0x11), block(0x22)];
    let new = [block(0xAA), block(0xBB)];

    for at in 0..COMMIT_OPS {
        let mut dev = MemBlockDevice::new(BLOCKS);
        // The OLD world is durable: both homes written and flushed before the adversary acts.
        dev.write_block(DATA_START, &old[0]).unwrap();
        dev.write_block(DATA_START + 1, &old[1]).unwrap();
        dev.flush().unwrap();

        let mut j = Journal::new();
        let mut f = FaultInject::new(dev, die_at(at));
        assert_eq!(
            f.remaining(),
            COMMIT_OPS,
            "pos {at}: the script must be armed"
        );
        let res = j.commit(&mut f, &two_updates());
        assert!(
            res.is_err(),
            "pos {at}: exactly one op was refused, so Err(Device)"
        );
        // The protocol ABORTS at the refusal, so the script advanced exactly to it: everything
        // before position 'at' played out, the refusal fired, and nothing after was consumed.
        assert_eq!(
            f.remaining(),
            COMMIT_OPS - (at + 1),
            "pos {at}: the refusal must be the LAST op consumed"
        );

        // Whatever the protocol did, RECOVERY must now bring the device to a consistent world:
        // every home block either fully OLD or fully NEW -- and both blocks agree with each other.
        let mut dev = f.into_inner();
        let mut j2 = Journal::new();
        let replayed = j2.recover(&mut dev).expect("recovery itself must not fail");
        let mut b0 = [0u8; BLOCK_SIZE];
        let mut b1 = [0u8; BLOCK_SIZE];
        dev.read_block(DATA_START, &mut b0).unwrap();
        dev.read_block(DATA_START + 1, &mut b1).unwrap();

        let all_old = b0 == old[0] && b1 == old[1];
        let all_new = b0 == new[0] && b1 == new[1];
        assert!(
            all_old || all_new,
            "pos {at}: TORN STATE -- homes disagree (replayed={replayed})"
        );
        // The outcome must MATCH the pivot rule: committed iff the pivot flush (position 4)
        // happened -- before it, uncommitted; from it on, recoverable-new.
        if at < 4 {
            assert!(
                all_old,
                "pos {at}: pre-pivot refusal must end in the OLD world"
            );
            assert!(
                !replayed,
                "pos {at}: nothing was committed, so nothing replays"
            );
        } else {
            assert!(
                replayed,
                "pos {at}: post-pivot refusal must be recoverable to NEW"
            );
            assert!(
                all_new,
                "pos {at}: post-pivot refusal must end in the NEW world"
            );
        }
    }
}

#[test]
fn a_failed_flush_is_surfaced_by_commit_never_swallowed() {
    // The durability barrier is the whole point of a flush; a protocol that treats a failed flush
    // as success is reporting durability that does not exist. The script refuses the FIRST flush
    // (the journal flush): commit must surface Err(Device), naming the device, not itself.
    let dev = MemBlockDevice::new(BLOCKS);
    let mut f = FaultInject::new(dev, vec![Op::WriteOk, Op::WriteOk, Op::FlushFail]);
    let mut j = Journal::new();
    let err = j.commit(&mut f, &two_updates()).unwrap_err();
    assert_eq!(err, StorageError::Device);
}

#[test]
fn recovery_survives_a_refusal_at_every_position_of_its_own_protocol() {
    // A COMMITTED device, then recovery run under a script that refuses each of its ops in turn:
    // commit-record read (R), journal reads (R x2), home writes (W x2), final flush (F).
    let mut committed = {
        let mut d = MemBlockDevice::new(BLOCKS);
        let mut j = Journal::new();
        j.commit(&mut d, &two_updates()).unwrap();
        d
    };
    // A snapshot of the committed world: every sweep iteration starts from EXACTLY this state
    // (MemBlockDevice deliberately implements no Clone -- restore() is the honest way back).
    let snap = committed.snapshot();
    // recovery ops: 1 read (record) + 2 reads (payload) + 2 writes (homes) + 1 flush = 6
    const REC_OPS: usize = 6;
    let rec_script = |at: usize| -> Vec<Op> {
        (0..REC_OPS)
            .map(|i| {
                if i == at {
                    match i {
                        0..=2 => Op::ReadFail,
                        3..=4 => Op::WriteFail,
                        _ => Op::FlushFail,
                    }
                } else if i <= 2 {
                    Op::ReadOk
                } else if i <= 4 {
                    Op::WriteOk
                } else {
                    Op::FlushOk
                }
            })
            .collect()
    };

    for at in 0..REC_OPS {
        committed.restore(&snap);
        let mut f = FaultInject::new(committed, rec_script(at));
        let mut j = Journal::new();
        let _ = j.recover(&mut f); // Err is an acceptable outcome -- surfacing IS the contract
                                   // Retry with health restored: recovery must COMPLETE (the protocol is idempotent), landing
                                   // the NEW world everywhere.
        let mut dev = f.into_inner();
        let mut j2 = Journal::new();
        let replayed = j2
            .recover(&mut dev)
            .expect("retry after a refusal must succeed");
        assert!(
            replayed,
            "pos {at}: the committed transaction must still be found"
        );
        let mut b0 = [0u8; BLOCK_SIZE];
        let mut b1 = [0u8; BLOCK_SIZE];
        dev.read_block(DATA_START, &mut b0).unwrap();
        dev.read_block(DATA_START + 1, &mut b1).unwrap();
        assert_eq!(b0, block(0xAA), "pos {at}");
        assert_eq!(b1, block(0xBB), "pos {at}");
        committed = dev;
    }
}

#[test]
fn a_refused_read_hands_back_no_bytes_at_all() {
    // A refused read must leave the caller's buffer UNWRITTEN -- a wrapper that zeroes it or fills
    // it with garbage turns "error" into "error plus fiction", and callers that check only the
    // error would never know.
    let mut dev = MemBlockDevice::new(BLOCKS);
    dev.write_block(DATA_START, &block(0x55)).unwrap();
    let f = FaultInject::new(dev, vec![Op::ReadFail]);
    let mut buf = [0x7Eu8; BLOCK_SIZE];
    let err = f.read_block(DATA_START, &mut buf).unwrap_err();
    assert_eq!(err, StorageError::Device);
    assert!(
        buf.iter().all(|&b| b == 0x7E),
        "a refused read must not touch the buffer"
    );
}

#[test]
fn the_layers_above_honor_the_device_contract_at_every_boundary() {
    // The journal must refuse to aim at the machinery: every reserved block (commit record +
    // journal slots) and both out-of-range edges refused BY NAME, every legal data block accepted.
    // Swept over the WHOLE index space, not sampled.
    let mut dev = MemBlockDevice::new(BLOCKS);
    let mut j = Journal::new();
    for idx in 0..BLOCKS {
        let res = j.commit(&mut dev, &[(idx, block(1))]);
        if !(DATA_START..BLOCKS).contains(&idx) {
            assert_eq!(res.unwrap_err(), StorageError::TooLarge, "idx {idx}");
        } else {
            res.expect("legal data block");
        }
    }
}

#[test]
fn passthrough_after_the_script_runs_out_is_untouched() {
    // The adversary must DISARM: once the script is spent the wrapper is indistinguishable from
    // the real device -- otherwise a proof's post-fault assertions would be about the adversary,
    // not the protocol.
    let mut dev = MemBlockDevice::new(BLOCKS);
    dev.write_block(DATA_START, &block(0x77)).unwrap();
    let mut f = FaultInject::new(dev, vec![Op::ReadFail, Op::WriteFail, Op::FlushFail]);
    assert_eq!(f.remaining(), 3);
    let mut buf = [0u8; BLOCK_SIZE];
    assert!(f.read_block(DATA_START, &mut buf).is_err());
    assert!(f.write_block(DATA_START + 1, &block(1)).is_err());
    assert!(f.flush().is_err());
    assert_eq!(f.remaining(), 0);
    // Spent: everything passes through, byte for byte.
    assert!(f.read_block(DATA_START, &mut buf).is_ok());
    assert_eq!(buf, block(0x77));
    f.write_block(DATA_START + 1, &block(0x88)).unwrap();
    let mut b2 = [0u8; BLOCK_SIZE];
    f.read_block(DATA_START + 1, &mut b2).unwrap();
    assert_eq!(b2, block(0x88));
}
