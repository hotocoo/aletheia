//! Console accounts (ADR-244): who may use the console once the machine has said someone must.
//!
//! Until now the console was a privileged root policy: whoever reached the serial line or the
//! desktop held every console capability. This module is the smallest account system that ends
//! that without inventing a new trust root:
//!
//! * **Opt-in, then mandatory.** A machine with no account record runs as before. The first
//!   `passwd NAME` writes the record; from then on every console session on that machine starts
//!   LOCKED and answers nothing but `login NAME` until a password matches.
//! * **Stored as a salted, slow hash.** One line per account in the namespace object [`RECORD`]:
//!   `name:iterations:salt:hash`, PBKDF2-HMAC-SHA256 (RFC 8018) over a 16-byte salt drawn from the
//!   machine's entropy device, [`ITERATIONS`] rounds. No entropy device, no record: a salt made
//!   from a clock is one an attacker can make too.
//! * **Compared in constant time, unknown names included.** A name with no record is checked
//!   against a fixed dummy hash, so how long a refusal takes says nothing about which names exist.
//! * **Failures cost time.** After [`FREE_FAILURES`] consecutive failures each further attempt is
//!   refused until a back-off doubling from 2 s (capped at 5 minutes) has passed, by the machine's
//!   own clock; a success clears it.
//! * **The record is not readable from the console.** The dispatcher refuses every command naming
//!   [`RECORD`] except `passwd`, so a stolen console session cannot copy the hashes off for an
//!   offline guess.

use alloc::string::String;
use alloc::vec::Vec;

use crate::crypto::hmac_sha256;

/// The namespace object that holds the accounts.
pub const RECORD: &str = ".users";
/// PBKDF2 rounds for a new record. Stored per line, so a later raise does not invalidate old ones.
pub const ITERATIONS: u32 = 20_000;
/// Salt bytes per record.
pub const SALT_LEN: usize = 16;
/// Longest account name: printable, no `:`.
pub const MAX_NAME: usize = 32;
/// Failures allowed before the back-off starts.
pub const FREE_FAILURES: u32 = 3;
const MAX_BACKOFF_SECS: u64 = 300;

/// What an account may do at the console (ADR-247). The first account is always `Admin`, and a
/// machine with no accounts runs as `Admin` (as before ADR-244).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Role {
    /// Everything, including other accounts and the machine's power and clock.
    #[default]
    Admin,
    /// Uses the machine: reads, writes, runs programs, changes the display. Not reboot, halt,
    /// overclock, nor anyone else's account.
    Operator,
    /// Reads only.
    Viewer,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Operator => "operator",
            Role::Viewer => "viewer",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "admin" => Some(Role::Admin),
            "operator" => Some(Role::Operator),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }
}

/// The namespace object that records who owns which object (ADR-250): `name:owner` lines.
pub const OWNERS: &str = ".owners";

/// Whether `name` is one of the machine's private records, which no console command reads,
/// lists or changes except through the commands that own them.
pub fn is_private(name: &str) -> bool {
    name == RECORD || name == OWNERS
}

/// The account that owns `name` in an ownership record, if any.
pub fn owner_of<'a>(record: &'a str, name: &str) -> Option<&'a str> {
    record.lines().find_map(|l| {
        let (n, o) = l.split_once(':')?;
        (n == name && valid_name(o)).then_some(o)
    })
}

/// The ownership record with `name` owned by `owner` (`None`: owned by nobody).
pub fn with_owner(record: &str, name: &str, owner: Option<&str>) -> String {
    let mut out: String = record
        .lines()
        .filter(|l| l.split_once(':').map(|(n, _)| n) != Some(name) && !l.trim().is_empty())
        .flat_map(|l| [l, "\n"])
        .collect();
    if let Some(o) = owner {
        out.push_str(name);
        out.push(':');
        out.push_str(o);
        out.push('\n');
    }
    out
}

/// PBKDF2-HMAC-SHA256 with one 32-byte output block (RFC 8018 section 5.2).
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut msg = [0u8; SALT_LEN + 4];
    let n = salt.len().min(SALT_LEN);
    msg[..n].copy_from_slice(&salt[..n]);
    msg[n..n + 4].copy_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &msg[..n + 4]);
    let mut t = u;
    for _ in 1..iterations.max(1) {
        u = hmac_sha256(password, &u);
        for (a, b) in t.iter_mut().zip(u.iter()) {
            *a ^= b;
        }
    }
    t
}

/// Whether two byte strings are equal, in time that depends only on their length.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Why a name or password was refused before anything was hashed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    BadName,
    EmptyPassword,
}

impl Refusal {
    pub fn name(self) -> &'static str {
        match self {
            Refusal::BadName => "a name is 1 to 32 letters, digits, '-', '_' or '.'",
            Refusal::EmptyPassword => "an empty password is refused",
        }
    }
}

/// A valid account name: 1..=32 of `[A-Za-z0-9._-]`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let b = s.as_bytes();
    if b.len() != 2 * N {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (nib(b[2 * i])? << 4) | nib(b[2 * i + 1])?;
    }
    Some(out)
}

/// One account line for `name` with `role`, hashing `password` with `salt`.
pub fn line(
    name: &str,
    password: &str,
    salt: &[u8; SALT_LEN],
    role: Role,
) -> Result<String, Refusal> {
    if !valid_name(name) {
        return Err(Refusal::BadName);
    }
    if password.is_empty() {
        return Err(Refusal::EmptyPassword);
    }
    let h = pbkdf2_sha256(password.as_bytes(), salt, ITERATIONS);
    Ok(alloc::format!(
        "{name}:{ITERATIONS}:{}:{}:{}\n",
        hex(salt),
        hex(&h),
        role.name()
    ))
}

/// The record with `name`'s line replaced by `new_line` (or added).
pub fn upsert(record: &str, name: &str, new_line: &str) -> String {
    let mut out: String = record
        .lines()
        .filter(|l| l.split(':').next() != Some(name) && !l.trim().is_empty())
        .flat_map(|l| [l, "\n"])
        .collect();
    out.push_str(new_line);
    out
}

/// `name`'s line in `record`: rounds, salt, hash and role. A line without a role field predates
/// roles (ADR-244) and was its machine's only account, so it reads as `Admin`.
fn parse_line(record: &str, name: &str) -> Option<(u32, [u8; SALT_LEN], [u8; 32], Role)> {
    record.lines().find_map(|l| {
        let mut f = l.split(':');
        let (n, it, salt, hash) = (f.next()?, f.next()?, f.next()?, f.next()?);
        if n != name {
            return None;
        }
        let role = match f.next() {
            None => Role::Admin,
            Some(r) => Role::parse(r)?,
        };
        if f.next().is_some() {
            return None;
        }
        Some((
            it.parse::<u32>().ok()?,
            unhex::<SALT_LEN>(salt)?,
            unhex::<32>(hash)?,
            role,
        ))
    })
}

/// The role `name` holds in `record`, if it has a well-formed line.
pub fn role_of(record: &str, name: &str) -> Option<Role> {
    parse_line(record, name).map(|(_, _, _, r)| r)
}

/// The role `password` opens `name` with in `record`, or `None`. An unknown name, a malformed
/// line or a wrong password are all `None`, and all cost the same hashing work.
pub fn verify(record: &str, name: &str, password: &str) -> Option<Role> {
    let found = parse_line(record, name);
    // The dummy is never a real hash of anything: a name with no record always fails, after the
    // same work a real check costs.
    let (iterations, salt, want, role) = match found {
        Some((it, s, h, r)) if it >= 1 => (it, s, h, Some(r)),
        _ => (ITERATIONS, [0u8; SALT_LEN], [0xA5u8; 32], None),
    };
    let got = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
    if same(&got, &want) && !password.is_empty() {
        role
    } else {
        None
    }
}

/// The names in `record`, for `whoami`/listing without exposing anything else.
pub fn names(record: &str) -> Vec<&str> {
    record
        .lines()
        .filter_map(|l| l.split(':').next())
        .filter(|n| valid_name(n))
        .collect()
}

/// Consecutive failures and the time before which the next attempt is refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Guard {
    failures: u32,
    wait_until: u64,
}

impl Guard {
    /// Seconds the caller must still wait at `now`, or `None` when an attempt may be made.
    pub fn wait(&self, now_secs: u64) -> Option<u64> {
        (now_secs < self.wait_until).then(|| self.wait_until - now_secs)
    }

    /// Record an attempt's result at `now`.
    pub fn record(&mut self, ok: bool, now_secs: u64) {
        if ok {
            *self = Guard::default();
            return;
        }
        self.failures = self.failures.saturating_add(1);
        if self.failures > FREE_FAILURES {
            let shift = (self.failures - FREE_FAILURES).min(16);
            let back = (1u64 << shift).min(MAX_BACKOFF_SECS);
            self.wait_until = now_secs + back;
        }
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbkdf2_matches_rfc7914_vector() {
        // RFC 7914 section 11: PBKDF2-HMAC-SHA256, P="passwd", S="salt", c=1, first 32 bytes.
        let h = pbkdf2_sha256(b"passwd", b"salt", 1);
        assert_eq!(
            hex(&h),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
    }

    #[test]
    fn a_record_opens_for_its_password_only() {
        let salt = [7u8; SALT_LEN];
        let rec = line("ada", "correct horse", &salt, Role::Operator).unwrap();
        assert_eq!(verify(&rec, "ada", "correct horse"), Some(Role::Operator));
        assert_eq!(verify(&rec, "ada", "correct hors"), None);
        assert_eq!(verify(&rec, "bob", "correct horse"), None);
        assert_eq!(verify(&rec, "ada", ""), None);
        assert_eq!(verify("ada:x:y:z\n", "ada", "anything"), None);
        // A line from before roles (ADR-244) reads as the admin it was.
        let old = rec.replace(":operator", "");
        assert_eq!(verify(&old, "ada", "correct horse"), Some(Role::Admin));
        assert_eq!(
            verify(&rec.replace(":operator", ":root"), "ada", "correct horse"),
            None
        );
        assert!(!rec.contains("correct"));
    }

    #[test]
    fn upsert_replaces_one_account_and_keeps_the_others() {
        let a = line("ada", "one", &[1; SALT_LEN], Role::Admin).unwrap();
        let b = line("bob", "two", &[2; SALT_LEN], Role::Viewer).unwrap();
        let rec = upsert(&upsert("", "ada", &a), "bob", &b);
        let a2 = line("ada", "three", &[3; SALT_LEN], Role::Admin).unwrap();
        let rec = upsert(&rec, "ada", &a2);
        assert_eq!(names(&rec), ["bob", "ada"]);
        assert!(verify(&rec, "ada", "three").is_some() && verify(&rec, "ada", "one").is_none());
        assert_eq!(verify(&rec, "bob", "two"), Some(Role::Viewer));
        assert_eq!(role_of(&rec, "bob"), Some(Role::Viewer));
    }

    #[test]
    fn names_and_passwords_are_refused_before_hashing() {
        let s = [0; SALT_LEN];
        assert_eq!(line("", "p", &s, Role::Admin), Err(Refusal::BadName));
        assert_eq!(line("a:b", "p", &s, Role::Admin), Err(Refusal::BadName));
        assert_eq!(
            line(&"x".repeat(33), "p", &s, Role::Admin),
            Err(Refusal::BadName)
        );
        assert_eq!(line("ok", "", &s, Role::Admin), Err(Refusal::EmptyPassword));
    }

    #[test]
    fn ownership_lines_are_set_moved_and_dropped() {
        let r = with_owner("", "notes", Some("ada"));
        let r = with_owner(&r, "plan", Some("bob"));
        assert_eq!(owner_of(&r, "notes"), Some("ada"));
        assert_eq!(owner_of(&r, "plan"), Some("bob"));
        assert_eq!(owner_of(&r, "other"), None);
        let r = with_owner(&r, "notes", None);
        assert_eq!(owner_of(&r, "notes"), None);
        assert_eq!(owner_of(&r, "plan"), Some("bob"));
        assert!(is_private(".owners") && is_private(".users") && !is_private("plan"));
    }

    #[test]
    fn failures_back_off_and_a_success_clears_them() {
        let mut g = Guard::default();
        for t in 0..FREE_FAILURES as u64 {
            assert_eq!(g.wait(t), None);
            g.record(false, t);
        }
        assert_eq!(g.wait(10), None);
        g.record(false, 10);
        assert_eq!(g.wait(10), Some(2));
        assert_eq!(g.wait(12), None);
        g.record(false, 12);
        assert_eq!(g.wait(12), Some(4));
        for _ in 0..20 {
            g.record(false, 100);
        }
        assert_eq!(g.wait(100), Some(MAX_BACKOFF_SECS));
        g.record(true, 500);
        assert_eq!((g.wait(500), g.failures()), (None, 0));
    }
}
