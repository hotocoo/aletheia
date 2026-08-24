//! The custody anchor crosses the platform boundary, attacked rather than exercised
//! (ALET-P1-034 delivery half, ADR-072, docs/INVARIANT-CONTRACTS.md section INV-CAP-DELIVERY).
//!
//! The suite inside bootroot.rs proves the contract on its happy path inside the kernel; this
//! file attacks it from the host where memory is unbounded and adversaries are cheap:
//!
//! * the fw_cfg directory walker against LYING firmware - counts past the data, truncated
//!   entries, prefix-name lookalikes, dead buses - every one resolved to a NAMED outcome,
//!   never a panic and never garbage accepted as custody;
//! * every wrong item size refused before a byte is wanted;
//! * the whole twelve-invariant boot suite re-proved on the host over a modeled medium,
//!   including the SECOND boot against the same medium;
//! * THE residual ADR-070 pinned: rolling the vault's two objects back together used to be
//!   undetectable. With the entity-store witness it is caught BY NAME - this file constructs
//!   exactly that rollback and holds the refusal up to the light;
//! * a crash-position sweep through the paired commit: at EVERY device-op position, whatever
//!   lands leaves a forward-safe world (witnessed <= found), never a refusal trap.

use std::collections::BTreeMap;

use kernel_core::bootroot::{boot_suite, commit_pair, deliver, open_custody, RootDelivery, VaultGateError, ROOT_ITEM};
use kernel_core::capvault::{CapVault, KEYSTORE_OBJECT, VAULT_OBJECT};
use kernel_core::faultdev::{FaultInject, Op};
use kernel_core::fwcfg::{self, FwCfgBus};
use kernel_core::fs::Filesystem;
use kernel_core::persist::{self, PersistError};
use kernel_core::spine::{Constraints, EntityType, Scope, Store};
use kernel_core::storage::{BlockDevice, MemBlockDevice, StorageError};

const ROOT_A: [u8; 32] = [0x3A; 32];
const DELIVERY: RootDelivery = RootDelivery::Delivered([0x3A; 32]);

// ---------------------------------------------------------------------------
// The modeled firmware bus
// ---------------------------------------------------------------------------

/// fw_cfg over plain memory: items keyed by selector, reads past an item's end return 0xFF -
/// exactly the dead-bus rule the target transports hide behind the same two-method trait.
struct MemBus {
    items: BTreeMap<u16, Vec<u8>>,
    cur: Option<(u16, usize)>,
}

impl MemBus {
    fn with(items: Vec<(u16, Vec<u8>)>) -> Self {
        MemBus { items: items.into_iter().collect(), cur: None }
    }

    /// No firmware at all: every read is dead.
    fn dead() -> Self {
        MemBus::with(vec![])
    }
}

impl FwCfgBus for MemBus {
    fn select(&mut self, selector: u16) {
        self.cur = Some((selector, 0));
    }
    fn read_byte(&mut self) -> u8 {
        let (sel, pos) = match self.cur {
            Some((s, p)) => (s, p),
            None => return 0xFF,
        };
        let b = self.items.get(&sel).and_then(|v| v.get(pos)).copied().unwrap_or(0xFF);
        self.cur = Some((sel, pos + 1));
        b
    }
}

const SIG_SEL: u16 = 0x00;
const DIR_SEL: u16 = 0x19;
const ITEM_SEL: u16 = 0x1000;

/// A live QEMU-looking bus: real signature, a directory naming one bystander item and - when
/// root is Some - ours at the given declared size with the given bytes.
fn qemu_bus(root: Option<(u32, Vec<u8>)>) -> MemBus {
    let mut entries: Vec<(u32, u16, Vec<u8>)> = vec![(4, 0x0002, b"opt/bystander/item".to_vec())];
    let mut items: Vec<(u16, Vec<u8>)> =
        vec![(SIG_SEL, b"QEMU".to_vec()), (0x0002, b"ABCD".to_vec())];
    if let Some((size, bytes)) = root {
        entries.push((size, ITEM_SEL, ROOT_ITEM.to_vec()));
        items.push((ITEM_SEL, bytes));
    }
    let mut dir = Vec::new();
    dir.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (size, sel, name) in entries {
        dir.extend_from_slice(&size.to_be_bytes());
        dir.extend_from_slice(&sel.to_be_bytes());
        dir.extend_from_slice(&0u16.to_be_bytes());
        let mut nm = [0u8; 56];
        nm[..name.len()].copy_from_slice(&name);
        dir.extend_from_slice(&nm);
    }
    items.push((DIR_SEL, dir));
    MemBus::with(items)
}

/// A bare directory head declaring COUNT entries, followed by nothing.
fn lying_dir(count: u32) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend_from_slice(&count.to_be_bytes());
    d
}

// ---------------------------------------------------------------------------
// The walker vs lying firmware
// ---------------------------------------------------------------------------

#[test]
fn a_dead_bus_is_named_firmware_absent_never_a_root() {
    let mut bus = MemBus::dead();
    assert_eq!(deliver(&mut bus), RootDelivery::FirmwareAbsent);
    // And a bus that answers SOMETHING that is not QEMU is the same fact.
    let mut liar = MemBus::with(vec![(SIG_SEL, b"VBOX".to_vec())]);
    assert_eq!(deliver(&mut liar), RootDelivery::FirmwareAbsent);
}

#[test]
fn a_live_platform_delivering_32_bytes_hands_exactly_those_bytes_to_custody() {
    let payload: Vec<u8> = (0..32u8).collect();
    let mut bus = qemu_bus(Some((32, payload.clone())));
    let want: [u8; 32] = payload.try_into().unwrap();
    assert_eq!(deliver(&mut bus), RootDelivery::Delivered(want));
    // Redelivery is byte-stable: reading twice hands custody the same anchor.
    assert_eq!(deliver(&mut bus), RootDelivery::Delivered(want));
}

#[test]
fn every_wrong_size_is_refused_before_a_byte_is_wanted() {
    for bad in [0u32, 1, 31, 33, 64, 4096] {
        let mut bus = qemu_bus(Some((bad, vec![0xEE; bad as usize])));
        match deliver(&mut bus) {
            RootDelivery::Malformed(n) => assert_eq!(n, bad as u64),
            other => panic!("size {} delivered as {:?}, wanted Malformed", bad, other),
        }
    }
}

#[test]
fn an_absent_item_on_a_live_bus_is_named_not_provided() {
    let mut bus = qemu_bus(None);
    assert_eq!(deliver(&mut bus), RootDelivery::RootNotProvided);
    // An EMPTY directory says the same thing.
    let mut empty = MemBus::with(vec![(SIG_SEL, b"QEMU".to_vec()), (DIR_SEL, lying_dir(0))]);
    assert_eq!(deliver(&mut empty), RootDelivery::RootNotProvided);
}

#[test]
fn a_directory_count_past_the_data_ends_the_walk_fail_closed() {
    // The head LIES: seven entries, zero present. Every read past the lie is 0xFF; names can
    // never match; the walk ends with not-provided instead of looping or panicking.
    let mut liar = MemBus::with(vec![(SIG_SEL, b"QEMU".to_vec()), (DIR_SEL, lying_dir(7))]);
    assert_eq!(deliver(&mut liar), RootDelivery::RootNotProvided);
}

#[test]
fn a_truncated_entry_never_matches_and_never_panics() {
    // Real head (one entry), but the entry itself is cut in half on the wire.
    let full = fwcfg::encode_directory_for_test(&[(32, ITEM_SEL, ROOT_ITEM)]);
    let mut cut = MemBus::with(vec![
        (SIG_SEL, b"QEMU".to_vec()),
        (DIR_SEL, full[..full.len() - 20].to_vec()),
        (ITEM_SEL, vec![0x11; 32]),
    ]);
    assert_eq!(deliver(&mut cut), RootDelivery::RootNotProvided);
}

#[test]
fn a_prefix_lookalike_name_must_not_match() {
    // The firmware exposes opt/org.aletheia/capvault-root-longer: walking for OUR name must
    // skip it (exact match against the NUL-padded field), not hand us someone else's item.
    let mut lookalike = qemu_bus(None);
    {
        let dir = lookalike.items.get_mut(&DIR_SEL).unwrap();
        let name_at = dir.len() - 56;
        let longer = b"opt/org.aletheia/capvault-root-longer";
        let mut nm = [0u8; 56];
        nm[..longer.len()].copy_from_slice(longer);
        dir[name_at..].copy_from_slice(&nm);
        // Declared size fixed at 32 so ONLY the name could wrongly match.
        dir[8..12].copy_from_slice(&32u32.to_be_bytes());
    }
    lookalike.items.insert(ITEM_SEL, vec![0x22; 32]);
    assert_eq!(deliver(&mut lookalike), RootDelivery::RootNotProvided);
}

// ---------------------------------------------------------------------------
// The gate contract over deliveries
// ---------------------------------------------------------------------------

#[test]
fn open_custody_names_every_undelivered_shape_without_touching_the_medium() {
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();

    assert!(matches!(
        open_custody(&mut fs, &mut disk, &RootDelivery::FirmwareAbsent, 0),
        Err(VaultGateError::FirmwareAbsent)
    ));
    assert!(matches!(
        open_custody(&mut fs, &mut disk, &RootDelivery::RootNotProvided, 0),
        Err(VaultGateError::RootNotProvided)
    ));
    assert!(matches!(
        open_custody(&mut fs, &mut disk, &RootDelivery::Malformed(31), 0),
        Err(VaultGateError::MalformedRoot(31))
    ));
    // The medium never learned anything: still blank underneath the refusals.
    let gen = match persist::load_compressed_with_generation(&fs, &disk) {
        Ok((_s, g)) => g,
        Err(PersistError::Absent) => 0,
        Err(e) => panic!("unexpected store state: {:?}", e),
    };
    assert_eq!(gen, 0);
}

#[test]
fn the_boot_suite_refuses_to_run_without_a_delivered_root() {
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    for d in [
        RootDelivery::FirmwareAbsent,
        RootDelivery::RootNotProvided,
        RootDelivery::Malformed(17),
    ] {
        let r = boot_suite(&mut disk, &d, |_, _, _| {});
        assert!(r.is_err(), "suite ran without a delivered root");
    }
}

// ---------------------------------------------------------------------------
// The twelve invariants, on the host, across two boots
// ---------------------------------------------------------------------------

#[test]
fn all_twelve_custody_invariants_hold_on_a_fresh_medium_and_again_on_reboot() {
    let mut disk = MemBlockDevice::new(512);
    let n1 = boot_suite(&mut disk, &DELIVERY, |i, ok, name| {
        assert!(ok, "first boot failed at invariant {}: {}", i, name);
    })
    .expect("first boot suite");
    assert_eq!(n1, 14);
    // Boot #2 against the SAME medium: reopen under the same platform anchor, everything holds.
    let n2 = boot_suite(&mut disk, &DELIVERY, |i, ok, name| {
        assert!(ok, "second boot failed at invariant {}: {}", i, name);
    })
    .expect("second boot suite");
    assert_eq!(n2, 14);
}

// ---------------------------------------------------------------------------
// ADR-070's residual, closed: consistent-PAIR rollback is now DETECTABLE
// ---------------------------------------------------------------------------

#[test]
fn rolling_both_vault_objects_back_is_caught_by_the_entity_store_witness() {
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();
    let store = Store::new();

    // Pair-commit #1: seal world one, witness its generation.
    let mut vault = open_custody(&mut fs, &mut disk, &DELIVERY, 0).unwrap();
    let (engine1, child1) = engine_with_child();
    commit_pair(&mut fs, &mut disk, &mut vault, &engine1, &store).unwrap();
    let keys_at_gen1 = fs.read(&disk, KEYSTORE_OBJECT).unwrap();
    let image_at_gen1 = fs.read(&disk, VAULT_OBJECT).unwrap();
    let (_s, witnessed1) = persist::load_compressed_with_generation(&fs, &disk).unwrap();

    // Pair-commit #2: newer world, newer generation.
    let (engine2, _child2) = engine_with_child();
    commit_pair(&mut fs, &mut disk, &mut vault, &engine2, &store).unwrap();
    let (_s, witnessed2) = persist::load_compressed_with_generation(&fs, &disk).unwrap();
    assert!(witnessed2 > witnessed1, "generations must advance");

    // THE ATTACK: restore BOTH vault objects to their generation-1 state - internally
    // consistent, authentic under the root: exactly what ADR-070 proved undetectable.
    fs.replace(&mut disk, KEYSTORE_OBJECT, &keys_at_gen1).unwrap();
    fs.replace(&mut disk, VAULT_OBJECT, &image_at_gen1).unwrap();

    // The pair IS authentic - a bare vault open admits it.
    let _pair_still_authentic = CapVault::open(&mut fs, &mut disk, &ROOT_A).unwrap();
    // ...but custody's door refuses BY NAME, naming both sides of the disagreement.
    match open_custody(&mut fs, &mut disk, &DELIVERY, witnessed2) {
        Err(VaultGateError::RolledBack { remembered, found }) => {
            assert_eq!(remembered, witnessed2);
            assert_eq!(found, witnessed1);
        }
        other => panic!("rolled-back pair admitted as {:?}", other.map(|_| ())),
    }

    // Recovery is honest too: at the generation the medium ACTUALLY pairs with (gen1), custody
    // opens and authority is intact - the refusal is a lockout pending a forward commit, never
    // a destruction of authority.
    let recovered = open_custody(&mut fs, &mut disk, &DELIVERY, witnessed1).unwrap();
    let world = recovered.load_sealed(&fs, &disk, 1000).unwrap();
    assert_eq!(
        world.evaluate("entity.derive", &doc(), &[child1]),
        kernel_core::spine::Decision::Allow
    );
}

// ---------------------------------------------------------------------------
// The paired commit under device failure: forward-safe at EVERY position
// ---------------------------------------------------------------------------

/// Build a seeded medium: one complete paired world, plus the generation it witnessed.
fn seeded_world() -> (MemBlockDevice, u64) {
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    {
        let mut fs = Filesystem::mount(&mut disk).unwrap();
        let mut vault = open_custody(&mut fs, &mut disk, &DELIVERY, 0).unwrap();
        let (engine, _) = engine_with_child();
        commit_pair(&mut fs, &mut disk, &mut vault, &engine, &Store::new()).unwrap();
    }
    let gen = {
        let mut fs = Filesystem::mount(&mut disk).unwrap();
        persist::load_compressed_with_generation(&fs, &disk).unwrap().1
    };
    (disk, gen)
}

#[test]
fn a_fault_at_every_pair_position_leaves_witnessed_never_ahead_of_found() {
    // Learn the exact op-kind sequence one clean paired commit executes, starting from a SEEDED
    // world (the sweep replays the SECOND commit, where both halves really write).
    let seq: Vec<Op> = {
        let (disk, _gen) = seeded_world();
        let mut rec = Recorder { inner: disk, log: std::cell::RefCell::new(Vec::new()) };
        let mut fs = Filesystem::mount(&mut rec).unwrap();
        let mut vault = open_custody(&mut fs, &mut rec, &DELIVERY, 0).unwrap();
        let (engine, _) = engine_with_child();
        rec.log.borrow_mut().clear();
        let _ = commit_pair(&mut fs, &mut rec, &mut vault, &engine, &Store::new());
        rec.log.into_inner()
    };
    assert!(
        seq.iter().any(|o| *o == Op::WriteOk),
        "a clean paired commit must actually write"
    );

    for at in 0..seq.len() {
        let (seeded, witnessed) = seeded_world();
        let snap = seeded.snapshot();
        let mut work = MemBlockDevice::new(512);
        work.restore(&snap);

        let mut dev = FaultInject::new(work, fail_script(&seq, at));
        let outcome: Option<bool> = (|| {
            let mut fs = Filesystem::mount(&mut dev).ok()?;
            let mut vault = open_custody(&mut fs, &mut dev, &DELIVERY, witnessed).ok()?;
            let (engine, _) = engine_with_child();
            Some(commit_pair(&mut fs, &mut dev, &mut vault, &engine, &Store::new()).is_ok())
        })();
        let mut base = dev.into_inner();

        // Post-state invariant: whatever landed, the durable witness never sits AHEAD of the
        // durable vault - the door always opens at what the medium itself remembers.
        let (_s, gen_now) = {
            let fs = Filesystem::mount(&mut base).unwrap();
            persist::load_compressed_with_generation(&fs, &base)
                .unwrap_or((Store::new(), 0))
        };
        let opens = {
            let mut fs = Filesystem::mount(&mut base).unwrap();
            open_custody(&mut fs, &mut base, &DELIVERY, gen_now).is_ok()
        };
        assert!(
            opens,
            "position {} ({:?}, commit ok={}): post-fault world refuses at its own witness",
            at, seq[at], outcome.unwrap_or(false)
        );
        let _ = snap;
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn doc() -> kernel_core::spine::Target {
    kernel_core::spine::Target {
        id: None,
        etype: Some(EntityType::Document),
    }
}

fn engine_with_child() -> (kernel_core::spine::CapEngine, kernel_core::spine::CapToken) {
    use kernel_core::spine::CapEngine;
    let mut e = CapEngine::new(0x5EED, 1000);
    let root_cap = e.mint("user", "entity.*", Scope::All, Constraints::none());
    let child = e
        .delegate(
            root_cap,
            "agent",
            "entity.derive",
            Scope::Type(EntityType::Document),
            Constraints::none(),
        )
        .unwrap();
    (e, child)
}

/// Records the exact op-kind sequence one clean run executes, so the sweep can aim a refusal
/// of the RIGHT kind at every protocol position (same harness discipline as capvault.rs).
struct Recorder {
    inner: MemBlockDevice,
    log: std::cell::RefCell<Vec<Op>>,
}

impl BlockDevice for Recorder {
    fn num_blocks(&self) -> usize {
        self.inner.num_blocks()
    }
    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        self.log.borrow_mut().push(Op::ReadOk);
        self.inner.read_block(idx, buf)
    }
    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        self.log.borrow_mut().push(Op::WriteOk);
        self.inner.write_block(idx, buf)
    }
    fn flush(&mut self) -> Result<(), StorageError> {
        self.log.borrow_mut().push(Op::FlushOk);
        self.inner.flush()
    }
}

fn fail_script(seq: &[Op], at: usize) -> Vec<Op> {
    seq.iter()
        .enumerate()
        .map(|(i, op)| match (i == at, op) {
            (true, Op::ReadOk) => Op::ReadFail,
            (true, Op::WriteOk) => Op::WriteFail,
            (true, Op::FlushOk) => Op::FlushFail,
            (_, other) => *other,
        })
        .collect()
}
