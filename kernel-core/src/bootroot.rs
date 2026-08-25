//! The custody anchor crosses the platform boundary (ALET-P1-034, ADR-072).
//!
//! ADR-070 built the vault's lifecycle and left two questions open. This module closes both.
//!
//! **Delivery.** The 32-byte root arrives over the firmware's own configuration channel (the
//! fwcfg module, QEMU fw_cfg) instead of being handed in by whoever compiled the boot. The
//! contract is deliberately narrow and fail-closed:
//!
//! * exactly-32 bytes delivered — the ONLY shape that opens a vault;
//! * item absent on a live fw_cfg bus — named refusal, machine continues without the vault;
//! * no fw_cfg signature at all (VirtualBox, real hardware) — a DIFFERENT named refusal;
//! * any other size — refused before a byte is wanted.
//!
//! The root is consumed exactly as ADR-070 demands: only its derived sealing subkey is retained,
//! so delivery transfers CUSTODY, not a working key.
//!
//! **The combined transaction, decided.** The capability image and the entity store stay TWO
//! commits — merging the AEAD image into the entity record would put authority bytes under the
//! record checksum's regime and give up independent rotation for no integrity gain. What was
//! missing was MUTUAL DETECTION: ADR-070 pinned "a consistent older pair rolls back undetected"
//! as needing an external anchor. The anchor is the entity store itself: every paired commit
//! records the vault's keystore generation INSIDE the durable entity record (under its trailing
//! checksum), and every custody open checks the monotone rule
//! "witnessed_generation <= keystore_counter", refusing BY NAME when the medium remembers newer
//! authority than the vault can show. Crash positions are safe by ORDER — the witness is written
//! LAST — so an interrupted pair always leaves witnessed <= found. What remains undetectable is
//! rolling back ALL THREE objects at once, which is strictly stronger than ADR-070's guarantee
//! and still pinned (and documented) in the host proofs.
//!
//! Proofs live in kernel-core/tests/bootroot.rs (host-exhaustive: directory liars, truncations,
//! wrong sizes, foreign roots, rollback positions) and in the in-kernel suite below, which runs
//! against the REAL firmware channel and REAL persistent medium on all three targets.

use crate::capvault::{self, CapVault, VaultError};
use crate::fs::Filesystem;
use crate::fwcfg::{self, FwCfgBus};
use crate::persist::{self, PersistError};
use crate::spine::{CapEngine, Decision, EntityType, Store, Target};
use crate::storage::BlockDevice;

/// The fw_cfg item this platform delivers the vault root through.
pub const ROOT_ITEM: &[u8] = b"opt/org.aletheia/capvault-root";
/// The same name, printable, for one honest boot-log line about where custody came from.
pub const ROOT_ITEM_DISPLAY: &str = "opt/org.aletheia/capvault-root";
/// The only root length custody accepts.
pub const ROOT_LEN: usize = 32;

/// What the platform handed us, if anything. Every non-Delivered variant is a NAMED fact the
/// boot log can print and a gate can require.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootDelivery {
    /// The platform delivered exactly a 32-byte root. The only variant that opens a vault.
    Delivered([u8; ROOT_LEN]),
    /// Live fw_cfg firmware answered, but carries no root item: the platform chose not to.
    RootNotProvided,
    /// Nothing answered the fw_cfg signature probe: there is no platform channel here at all.
    FirmwareAbsent,
    /// The item exists but its declared size is not exactly 32 bytes.
    Malformed(u64),
}

/// Walk the firmware channel once and classify what was delivered. Reads each item exactly once;
/// a second call re-reads from the hardware (proved byte-stable by the suite).
pub fn deliver(bus: &mut impl FwCfgBus) -> RootDelivery {
    if !fwcfg::signature_matches(bus) {
        return RootDelivery::FirmwareAbsent;
    }
    let entry = match fwcfg::find_file(bus, ROOT_ITEM) {
        Some(e) => e,
        None => return RootDelivery::RootNotProvided,
    };
    if entry.size != ROOT_LEN as u32 {
        return RootDelivery::Malformed(entry.size as u64);
    }
    let mut root = [0u8; ROOT_LEN];
    let _filled = fwcfg::read_entry(bus, &entry, &mut root);
    // A short read cannot happen against a declared size of 32 on a live bus, and every other
    // shape was refused above by its DECLARED size; what remains IS our 32 bytes to consume.
    RootDelivery::Delivered(root)
}

impl RootDelivery {
    /// One honest line about the delivery outcome, for the boot log and the gates. The Malformed
    /// size itself is printed by the caller next to this fact.
    pub fn describe(&self) -> &'static str {
        match self {
            RootDelivery::Delivered(_) => {
                "platform custody: root DELIVERED over firmware configuration"
            }
            RootDelivery::RootNotProvided => {
                "platform custody: PLATFORM ROOT ABSENT (RootNotProvided) - the vault stays sealed, the machine continues"
            }
            RootDelivery::FirmwareAbsent => {
                "platform custody: NO PLATFORM CHANNEL (FirmwareAbsent) - no fw_cfg answered, the vault stays sealed"
            }
            RootDelivery::Malformed(_) => {
                "platform custody: MALFORMED ROOT SIZE (MalformedRoot) - refused fail-closed"
            }
        }
    }

    /// Why custody refuses to open this delivery, as a value — the SAME refusal open_custody
    /// would return, provable without touching the medium.
    pub fn as_gate_error(&self) -> Option<VaultGateError> {
        match self {
            RootDelivery::Delivered(_) => None,
            RootDelivery::RootNotProvided => Some(VaultGateError::RootNotProvided),
            RootDelivery::FirmwareAbsent => Some(VaultGateError::FirmwareAbsent),
            RootDelivery::Malformed(n) => Some(VaultGateError::MalformedRoot(*n)),
        }
    }
}

/// Why custody refused to open. Every variant names the exact fact; none is recoverable by
/// retrying — they are properties of the platform or the medium, not transient faults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultGateError {
    /// No fw_cfg firmware answered.
    FirmwareAbsent,
    /// Firmware answered but provided no root item.
    RootNotProvided,
    /// The root item exists with a size other than 32.
    MalformedRoot(u64),
    /// The vault itself refused (forwarded BY NAME from capvault).
    Vault(VaultError),
    /// The entity store remembers NEWER authority than the vault can show: the sealed objects
    /// were rolled back behind the other subsystem's back.
    RolledBack { remembered: u64, found: u64 },
}

/// Open the custody vault under a platform-delivered root, enforcing the monotone witness rule
/// against the entity store's recorded generation. This is THE door: nothing else opens a vault.
pub fn open_custody<D: BlockDevice>(
    fs: &mut Filesystem,
    dev: &mut D,
    delivery: &RootDelivery,
    witnessed_gen: u64,
) -> Result<CapVault, VaultGateError> {
    let root = match delivery {
        RootDelivery::Delivered(r) => *r,
        RootDelivery::RootNotProvided => return Err(VaultGateError::RootNotProvided),
        RootDelivery::FirmwareAbsent => return Err(VaultGateError::FirmwareAbsent),
        RootDelivery::Malformed(n) => return Err(VaultGateError::MalformedRoot(*n)),
    };
    let vault = CapVault::open(fs, dev, &root).map_err(VaultGateError::Vault)?;
    let found = vault.keystore_nonce_counter();
    if witnessed_gen > found {
        return Err(VaultGateError::RolledBack {
            remembered: witnessed_gen,
            found,
        });
    }
    Ok(vault)
}

/// Why a paired commit aborted. Both halves name their layer; a crash between the commits is NOT
/// an error case here — the caller simply retries the whole commit on the next boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairError {
    /// Sealing the capability image failed (forwarded by name).
    Seal(VaultError),
    /// Writing the witnessing entity-store record failed (forwarded by name).
    Witness(PersistError),
}

/// The PAIRED COMMIT, in its crash-safe order:
///
/// 1. seal the capability image (keystore reserve-commit, then image replace — ADR-070);
/// 2. write the entity-store record carrying the vault's NEW keystore generation.
///
/// Because the witness goes last, every interruption leaves witnessed <= found: an interrupted
/// pair is forward-safe, never a refusal trap. Only a COMPLETE pair raises the bar the next
/// boot enforces.
pub fn commit_pair<D: BlockDevice>(
    fs: &mut Filesystem,
    dev: &mut D,
    vault: &mut CapVault,
    engine: &CapEngine,
    store: &Store,
) -> Result<u64, PairError> {
    vault
        .save_sealed(fs, dev, engine)
        .map_err(PairError::Seal)?;
    let gen = vault.keystore_nonce_counter();
    persist::save_compressed_with_generation(fs, dev, store, gen).map_err(PairError::Witness)?;
    Ok(gen)
}

// ---------------------------------------------------------------------------
// The in-kernel invariant suite

/// One authority world to seal: a root, an attenuated child that must stay live, and a revoked
/// sibling that must stay dead — the same shape the capability-lifetime suite uses, so both
/// suites reason about the SAME persisted object.
fn build_engine() -> (CapEngine, crate::spine::CapToken, crate::spine::CapToken) {
    use crate::spine::{Constraints, Scope};
    let mut e = CapEngine::new(0x5EED, 1000);
    let root_tok = e.mint("user", "entity.*", Scope::All, Constraints::none());
    let child = e
        .delegate(
            root_tok,
            "agent",
            "entity.derive",
            Scope::Type(EntityType::Document),
            Constraints::none(),
        )
        .expect("attenuation of a live root always delegates");
    let doomed = e
        .delegate(
            root_tok,
            "agent",
            "entity.delete",
            Scope::All,
            Constraints::none(),
        )
        .expect("equal-scope delegation of a live root always delegates");
    e.revoke(doomed);
    (e, child, doomed)
}

fn doc_target() -> Target {
    Target {
        id: None,
        etype: Some(EntityType::Document),
    }
}

/// The custody-delivery invariants, proved against the REAL persistent medium under the REAL
/// firmware-delivered root. Arch-independent and deliberately SMALL (ADR-063: the boot heap
/// never frees, so sweep churn here would starve later suites — rotation/rekey/retirement
/// depth stays host-side, already exhaustive in capvault.rs and tests/capvault.rs). Requires a
/// Delivered root; callers print the named absence facts for every other variant.
pub fn boot_suite<D: BlockDevice>(
    dev: &mut D,
    delivery: &RootDelivery,
    mut log: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            log(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }

    let root = match delivery {
        RootDelivery::Delivered(r) => *r,
        _ => {
            return Err((
                0,
                "platform custody: suite requires a delivered platform root",
            ))
        }
    };

    // Mount exactly like the persistence witness does: format once on a blank medium, never wipe.
    let mut fs = match Filesystem::mount(dev) {
        Ok(f) => f,
        Err(crate::fs::FsError::NotFormatted) => {
            Filesystem::format(dev).map_err(|_| (0, "platform custody: medium refused format"))?;
            Filesystem::mount(dev).map_err(|_| (0, "platform custody: medium refused mount"))?
        }
        Err(_) => return Err((0, "platform custody: medium refused mount")),
    };
    let (store_after_witness, witnessed_gen) =
        match persist::load_compressed_with_generation(&fs, dev) {
            Ok(sg) => sg,
            Err(PersistError::Absent) => (Store::new(), 0),
            Err(_) => return Err((0, "platform custody: entity store unreadable")),
        };

    // 1 - the DELIVERED root opens the vault (first boot creates the keystore, later boots reopen).
    let opened = open_custody(&mut fs, dev, delivery, witnessed_gen);
    check!(
        opened.is_ok(),
        "platform custody: the firmware-delivered root opens the vault"
    );
    let mut vault = match opened {
        Ok(v) => v,
        Err(_) => return Err((n, "platform custody: open failed mid-suite")),
    };

    // 2 - a FOREIGN root is refused by name on the same medium: the disk alone is nobody's.
    {
        let mut foreign = root;
        foreign[0] ^= 0x5A;
        let refused = CapVault::open(&mut fs, dev, &foreign);
        check!(
            matches!(refused, Err(VaultError::KeystoreAuth)),
            "platform custody: the sealed registry refuses a foreign root"
        );
    }

    // 3+4+5 - seal the world, reopen it cold from the medium: authority intact, revocation dead,
    // and the reserve-first keystore commit visibly advanced the constructed-nonce counter.
    let counter_before_save = vault.keystore_nonce_counter();
    let (engine, child, doomed) = build_engine();
    let sealed = vault.save_sealed(&mut fs, dev, &engine);
    check!(
        sealed.is_ok(),
        "platform custody: the registry seals under the delivered root"
    );
    drop(vault);
    let reopened = open_custody(&mut fs, dev, delivery, witnessed_gen);
    check!(
        reopened.is_ok(),
        "platform custody: the vault reopens cold after sealing"
    );
    let mut vault = match reopened {
        Ok(v) => v,
        Err(_) => return Err((n, "platform custody: reopen failed mid-suite")),
    };
    let loaded = vault.load_sealed(&fs, dev, 1000);
    check!(
        loaded
            .as_ref()
            .map(|e| e.evaluate("entity.derive", &doc_target(), &[child]) == Decision::Allow)
            .unwrap_or(false),
        "platform custody: authority sealed under custody authorizes after reopening"
    );
    let still_dead = loaded.map(|e| e.evaluate("entity.delete", &doc_target(), &[doomed]));
    check!(
        matches!(still_dead, Ok(Decision::Deny(_))),
        "platform custody: a revoked capability stays dead across the seal"
    );
    check!(
        vault.keystore_nonce_counter() > counter_before_save,
        "platform custody: the keystore nonce counter advanced with this boot's seal"
    );

    // 6+7 - the PAIRED COMMIT records this generation inside the entity store, under its
    // checksum, and ROLLED-BACK custody is refused BY NAME against that witness: a medium
    // remembering gen G refuses a vault whose newest keystore counter sits below G.
    let paired = commit_pair(&mut fs, dev, &mut vault, &engine, &store_after_witness);
    let gen_now = match paired {
        Ok(g) => g,
        Err(_) => return Err((n, "platform custody: paired commit failed")),
    };
    check!(
        gen_now == vault.keystore_nonce_counter() && gen_now > witnessed_gen,
        "platform custody: the entity store witnesses this boot's custody generation"
    );
    {
        let rolled = open_custody(&mut fs, dev, delivery, gen_now.wrapping_add(1));
        check!(
            matches!(rolled, Err(VaultGateError::RolledBack { .. })),
            "platform custody: rolled-back custody is refused by name against the witness"
        );
    }

    // 8+9+10 - the impostor shapes are NAMED refusals, provable without hardware lying: a root
    // of the wrong size, an undelivered root, and a vault that would not authenticate.
    check!(
        matches!(
            open_custody(&mut fs, dev, &RootDelivery::Malformed(31), 0),
            Err(VaultGateError::MalformedRoot(31))
        ),
        "platform custody: a wrong-size root is refused by name"
    );
    check!(
        matches!(
            open_custody(&mut fs, dev, &RootDelivery::RootNotProvided, 0),
            Err(VaultGateError::RootNotProvided)
        ) && matches!(
            open_custody(&mut fs, dev, &RootDelivery::FirmwareAbsent, 0),
            Err(VaultGateError::FirmwareAbsent)
        ),
        "platform custody: an undelivered root refuses fail-closed by name"
    );
    {
        // A TAMPERED keystore object refuses WHOLE: flip one byte of cap.keys on the medium,
        // demand the named refusal, then restore - a refusal that mutated the medium would be
        // an attacker-visible side effect.
        let pristine = fs.read(dev, capvault::KEYSTORE_OBJECT).map_err(|_| {
            (
                n,
                "platform custody: keystore object vanished before tamper",
            )
        })?;
        let mut evil = pristine.clone();
        evil[10] ^= 0x80;
        fs.replace(dev, capvault::KEYSTORE_OBJECT, &evil)
            .map_err(|_| (n, "platform custody: tamper write refused"))?;
        let tampered = CapVault::open(&mut fs, dev, &root);
        let refused_tamper = matches!(tampered, Err(VaultError::KeystoreAuth));
        fs.replace(dev, capvault::KEYSTORE_OBJECT, &pristine)
            .map_err(|_| (n, "platform custody: restore write refused"))?;
        check!(
            refused_tamper,
            "platform custody: a tampered keystore object refuses whole"
        );
    }

    // 11+12 - stability: one more cold reopen admits the final world with identical verdicts,
    // for the SAME tokens the earlier checks used (a fresh engine mints different ids).
    let final_world = open_custody(&mut fs, dev, delivery, gen_now).and_then(|v| {
        v.load_sealed(&fs, dev, 1000)
            .map_err(VaultGateError::Vault)
            .map(|e| e.evaluate("entity.derive", &doc_target(), &[child]))
    });
    check!(
        matches!(final_world, Ok(Decision::Allow)),
        "platform custody: the final world reopens and authorizes identically"
    );
    let counters_stable = open_custody(&mut fs, dev, delivery, gen_now)
        .map(|v| v.keystore_nonce_counter() == vault.keystore_nonce_counter());
    check!(
        matches!(counters_stable, Ok(true)),
        "platform custody: repeated opens move no counter (custody is read-only when idle)"
    );

    Ok(n)
}
