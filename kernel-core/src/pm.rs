//! The power/performance contract: frequency is AUTHORITY, heat is a HARD CEILING (ALET-P2-022,
//! ADR-076).
//!
//! "Overclocking straight to the components" is, in an Aletheia-shaped OS, exactly the same
//! question as every other privileged act: who may push a clock past the platform's nominal top,
//! and what stops that push from melting the silicon. This module defines, once, the contract a
//! power-management subsystem must satisfy and a complete SOFTWARE MODEL of it that every proof
//! can run against today — the same posture ADR-071 took before VT-d and SMMUv3 were programmed.
//!
//! The contract, in one breath: every CPU core belongs to a frequency DOMAIN with a discrete
//! ladder of operating points; the top of the GOVERNOR range is `nominal` — reachable by anyone,
//! including the demand governor; points above nominal are the OVERCLOCK band, reachable only
//! through a live, per-domain ELEVATION GRANT — an unforgeable token attenuated on delegation
//! (a child's ceiling is never above its parent's), revoked with cascade, and clamping the
//! domain back to nominal the moment its grant dies; the thermal ENVELOPE is absolute — no
//! ladder point above it may be registered and no grant past it may be minted, so no reachable
//! state exceeds it, whatever authority says; a thermal TRIP clamps every domain to its lowest
//! point and latches a cooldown during which elevation is refused BY NAME even with a grant;
//! a zero-demand domain is PARKED at its lowest point (the idle machine costs nothing,
//! ADR-056) and a demanded domain is never parked; device power states move only along legal
//! arcs with every illegal arc a named refusal; and every accepted transition and every refusal
//! lands in a bounded audit ledger under a monotonic sequence number — nothing about a clock
//! changes silently.
//!
//! # Proof posture
//!
//! Host-exhaustive in `tests/pm.rs` (the full decision table over the OC band, attenuation
//! monotonicity by sweep, revocation clamps, envelope absoluteness, cooldown tick exactness,
//! idle accounting exactness, device-arc table), plus a compact in-kernel suite so every target
//! proves the core promises at boot. Named non-claim: this is the CONTRACT — a hardware rung
//! (MSR/CPPC/ACPI programming into real silicon, battery and system sleep/wake) stays scoped in
//! the gap register, exactly as the IOMMU contract preceded its silicon (ADR-073/074/075).

use alloc::vec;
use alloc::vec::Vec;

/// Live elevation grants the model tracks. Bounded so a boot cannot grow it without bound on a
/// never-freeing heap (ADR-063); generous for every caller the kernel has today.
pub const MAX_GRANTS: usize = 64;
/// Registered frequency domains. One per CPU cluster in every current target.
pub const MAX_DOMAINS: usize = 16;
/// Registered power-manageable devices.
pub const MAX_DEVICES: usize = 32;
/// Audit ledger capacity. The ledger WRAPS (the oldest record falls off) but its sequence
/// number never does: the count of everything that ever happened stays exact, the memory
/// stays bounded.
pub const AUDIT_CAP: usize = 128;
/// Cooldown length after a thermal trip, in ticks — the same logical clock every other
/// kernel contract uses (the kernel has no wall clock; monotonicity is the caller's duty).
pub const COOLDOWN_TICKS: u64 = 1_000;

/// One discrete operating point of a frequency domain. Real DVFS selects points, not
/// arbitrary frequencies, so every request below names an exact point or is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperatingPoint {
    /// Clock in kHz.
    pub khz: u32,
    /// Voltage in mV — recorded so a ladder cannot silently pair a high clock with a low rail.
    pub mv: u16,
}

/// Why the power manager refused. Every variant names what was involved — the shape every
/// other boundary in this kernel reports by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PmFault {
    /// The domain was never registered.
    UnknownDomain(u32),
    /// The device was never registered.
    UnknownDevice(u32),
    /// The domain (or device) was already registered.
    AlreadyRegistered(u32),
    /// The ladder is not an honest ladder: empty, non-ascending, duplicate clocks, a nominal
    /// that is not a ladder point, or any point above the envelope.
    MalformedLadder(u32),
    /// The requested frequency is not an operating point of this domain.
    NotAnOperatingPoint { domain: u32, khz: u32 },
    /// Elevation above nominal was requested with NO live grant for this domain — the
    /// fail-closed default. Covers absent, spent and revoked tokens alike: offering a dead
    /// token is offering nothing.
    NoAuthority { domain: u32 },
    /// A live grant exists but its ceiling is below the request. Names both sides.
    NotGranted {
        domain: u32,
        requested_khz: u32,
        granted_khz: u32,
    },
    /// Minting a grant past the thermal envelope is refused AT MINT — the envelope is
    /// absolute and no authority, not even the root's, reaches past it.
    AboveEnvelope {
        domain: u32,
        requested_khz: u32,
        envelope_khz: u32,
    },
    /// The domain tripped and its cooldown has not expired. Elevation stays refused even
    /// with a grant until the exact tick; the remaining ticks are named.
    Cooldown { domain: u32, remaining_ticks: u64 },
    /// Delegation tried to widen a parent grant (a child ceiling above the parent's).
    Amplification {
        domain: u32,
        parent_max_khz: u32,
        child_max_khz: u32,
    },
    /// A grant for one domain cannot be delegated onto another.
    CrossDomain {
        grant_domain: u32,
        target_domain: u32,
    },
    /// The grant table is full (or the domain/device table is).
    NoSpace,
    /// The demand register was set above 100.
    BadDemand { domain: u32, pct: u8 },
    /// The domain is already parked in an idle state.
    AlreadyIdle(u32),
    /// A governor never parks a demanded domain.
    DomainBusy { domain: u32, pct: u8 },
    /// C0 is "running" — it is not a state you park in.
    NotAnIdleState(u32),
    /// Wake requested for a domain that is not parked.
    NotIdle(u32),
    /// The device power transition is not a legal arc (e.g. D3 -> D1: wake through D0).
    IllegalDState {
        device: u32,
        from: DState,
        to: DState,
    },
}

/// Device power state. Three rungs are modeled: on, low, off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DState {
    D0,
    D1,
    D3,
}

/// Idle (sleep) state of a domain, with the latency a wake from it costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleState {
    /// Running.
    C0,
    /// Shallow idle — cheap to wake.
    C1,
    /// Deep idle — expensive to wake.
    C2,
}

impl IdleState {
    /// Wake latency in nanoseconds. C0 costs nothing; deeper states cost more — the
    /// accounting that makes "the idle machine should cost nothing" MEASURABLE (ADR-056).
    pub fn wake_latency_ns(self) -> u64 {
        match self {
            IdleState::C0 => 0,
            IdleState::C1 => 1_000,
            IdleState::C2 => 10_000,
        }
    }

    fn slot(self) -> usize {
        match self {
            IdleState::C0 => 0,
            IdleState::C1 => 1,
            IdleState::C2 => 2,
        }
    }
}

/// One audit record: everything the boundary did, accepted or refused, in order, with the
/// holder a grant act was performed for ("" when the record is not about a grant).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditRecord {
    pub seq: u64,
    pub accepted: bool,
    pub domain: u32,
    /// The operating point involved (demand pct on demand records; 0 when not applicable).
    pub khz: u32,
    /// Refusal class or act name: "set", "clamp", "mint", "cooldown", "no-authority", …
    pub kind: &'static str,
    /// Who the grant act named as holder ("" when not a grant act).
    pub holder: &'static str,
}

/// One live (or once-live) elevation grant. Tokens are possession-based like the spine's
/// (`next_serial ^ secret`); revocation, not secrecy, is what retires them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Grant {
    token: u64,
    domain: u32,
    max_khz: u32,
    parent: Option<u64>,
}

struct Domain {
    id: u32,
    ladder: Vec<OperatingPoint>,
    /// Index into `ladder` of the top of the governor range.
    nominal_idx: usize,
    envelope_khz: u32,
    trip_temp_mc: i32,
    /// Index into `ladder` of the current point.
    current_idx: usize,
    demand_pct: u8,
    /// Cooldown expiry tick; 0 means never tripped.
    cooldown_until: u64,
    /// Open idle span: the state parked in and the tick it opened (None = running).
    idle_enter: Option<(IdleState, u64)>,
    /// Closed idle residency per state, in ticks: [C0, C1, C2].
    idle_residency_ticks: [u64; 3],
    wake_latency_ns_total: u64,
}

/// The software power manager: register domains, mint/delegate/revoke elevation grants,
/// request operating points, govern demand, trip thermals, park cores, move devices.
pub struct PmEngine {
    secret: u64,
    next_serial: u64,
    domains: Vec<Domain>,
    devices: Vec<(u32, DState)>,
    grants: Vec<Grant>,
    revoked: Vec<u64>,
    audit: Vec<AuditRecord>,
    audit_next_seq: u64,
    accepted: usize,
    refusals: usize,
}

impl PmEngine {
    pub fn new(secret: u64) -> Self {
        PmEngine {
            secret,
            next_serial: 0x00C0_FFEE,
            domains: Vec::new(),
            devices: Vec::new(),
            grants: Vec::new(),
            revoked: Vec::new(),
            audit: Vec::with_capacity(AUDIT_CAP),
            audit_next_seq: 1,
            accepted: 0,
            refusals: 0,
        }
    }

    // -- registration --------------------------------------------------------

    /// Register a frequency domain. `ladder` must be strictly ascending in kHz, `nominal_khz`
    /// must be one of its points, and NO point may sit above `envelope_khz` — the envelope is
    /// structural: if the hardware cannot be asked for it, nothing can reach it.
    pub fn register_domain(
        &mut self,
        id: u32,
        ladder: &[OperatingPoint],
        nominal_khz: u32,
        envelope_khz: u32,
        trip_temp_mc: i32,
    ) -> Result<(), PmFault> {
        if self.domains.iter().any(|d| d.id == id) {
            self.record(false, id, 0, "already-registered", "");
            return Err(PmFault::AlreadyRegistered(id));
        }
        if self.domains.len() >= MAX_DOMAINS {
            self.record(false, id, 0, "no-space", "");
            return Err(PmFault::NoSpace);
        }
        let malformed = ladder.is_empty()
            || ladder.windows(2).any(|w| w[0].khz >= w[1].khz)
            || ladder.iter().any(|p| p.khz > envelope_khz)
            || !ladder.iter().any(|p| p.khz == nominal_khz);
        if malformed {
            self.record(false, id, 0, "malformed-ladder", "");
            return Err(PmFault::MalformedLadder(id));
        }
        let nominal_idx = ladder.iter().position(|p| p.khz == nominal_khz).unwrap();
        self.domains.push(Domain {
            id,
            ladder: ladder.to_vec(),
            nominal_idx,
            envelope_khz,
            trip_temp_mc,
            current_idx: 0,
            demand_pct: 0,
            cooldown_until: 0,
            idle_enter: None,
            idle_residency_ticks: [0; 3],
            wake_latency_ns_total: 0,
        });
        self.record(true, id, ladder[0].khz, "register", "");
        Ok(())
    }

    /// Register a power-manageable device. Fresh devices are ON (D0) — fail-usable, and
    /// powering them down is a separate, audited act.
    pub fn register_device(&mut self, id: u32) -> Result<(), PmFault> {
        if self.devices.iter().any(|(d, _)| *d == id) {
            self.record(false, id, 0, "already-registered", "");
            return Err(PmFault::AlreadyRegistered(id));
        }
        if self.devices.len() >= MAX_DEVICES {
            self.record(false, id, 0, "no-space", "");
            return Err(PmFault::NoSpace);
        }
        self.devices.push((id, DState::D0));
        self.record(true, id, 0, "register-device", "");
        Ok(())
    }

    // -- authority ------------------------------------------------------------

    /// Mint a ROOT elevation grant for `domain` up to `max_khz`. The root grant is the only
    /// door into the overclock band; its ceiling is refused at mint if it is not a ladder
    /// point or reaches past the envelope.
    pub fn mint_grant(
        &mut self,
        domain: u32,
        max_khz: u32,
        holder: &'static str,
    ) -> Result<u64, PmFault> {
        let (envelope, is_point) = match self.domain(domain) {
            Some(d) => (d.envelope_khz, d.ladder.iter().any(|p| p.khz == max_khz)),
            None => {
                self.record(false, domain, max_khz, "unknown-domain", holder);
                return Err(PmFault::UnknownDomain(domain));
            }
        };
        if max_khz > envelope {
            self.record(false, domain, max_khz, "above-envelope", holder);
            return Err(PmFault::AboveEnvelope {
                domain,
                requested_khz: max_khz,
                envelope_khz: envelope,
            });
        }
        if !is_point {
            self.record(false, domain, max_khz, "not-an-operating-point", holder);
            return Err(PmFault::NotAnOperatingPoint {
                domain,
                khz: max_khz,
            });
        }
        if self.grants.len() >= MAX_GRANTS {
            self.record(false, domain, max_khz, "no-space", holder);
            return Err(PmFault::NoSpace);
        }
        self.next_serial += 1;
        let token = self.next_serial ^ self.secret;
        self.grants.push(Grant {
            token,
            domain,
            max_khz,
            parent: None,
        });
        self.record(true, domain, max_khz, "mint", holder);
        Ok(token)
    }

    /// Delegate a grant: same domain only, ceiling equal-or-narrower — the same attenuation
    /// law every other authority in this kernel obeys (ADR-003, ADR-048).
    pub fn delegate(
        &mut self,
        parent_token: u64,
        domain: u32,
        max_khz: u32,
        holder: &'static str,
    ) -> Result<u64, PmFault> {
        let parent = match self.grants.iter().find(|g| g.token == parent_token) {
            Some(g) if !self.is_revoked(g.token) => *g,
            _ => {
                self.record(false, domain, max_khz, "no-authority", holder);
                return Err(PmFault::NoAuthority { domain });
            }
        };
        if parent.domain != domain {
            self.record(false, domain, max_khz, "cross-domain", holder);
            return Err(PmFault::CrossDomain {
                grant_domain: parent.domain,
                target_domain: domain,
            });
        }
        if max_khz > parent.max_khz {
            self.record(false, domain, max_khz, "amplification", holder);
            return Err(PmFault::Amplification {
                domain,
                parent_max_khz: parent.max_khz,
                child_max_khz: max_khz,
            });
        }
        let is_point = self
            .domain(domain)
            .map(|d| d.ladder.iter().any(|p| p.khz == max_khz))
            .unwrap_or(false);
        if !is_point {
            self.record(false, domain, max_khz, "not-an-operating-point", holder);
            return Err(PmFault::NotAnOperatingPoint {
                domain,
                khz: max_khz,
            });
        }
        if self.grants.len() >= MAX_GRANTS {
            self.record(false, domain, max_khz, "no-space", holder);
            return Err(PmFault::NoSpace);
        }
        self.next_serial += 1;
        let token = self.next_serial ^ self.secret;
        self.grants.push(Grant {
            token,
            domain,
            max_khz,
            parent: Some(parent_token),
        });
        self.record(true, domain, max_khz, "delegate", holder);
        Ok(token)
    }

    /// Revoke a grant with cascade. A clock raised INTO the overclock band under a now-dead
    /// grant comes back to nominal IMMEDIATELY — authority that dies takes its effects with
    /// it, the same law an unmapped DMA window obeys (ADR-071). Revoking an unknown or
    /// already-revoked token is a no-op: idempotent, never a distinguishable error.
    pub fn revoke(&mut self, token: u64, now: u64) {
        if self.is_revoked(token) || !self.grants.iter().any(|g| g.token == token) {
            return;
        }
        self.revoked.push(token);
        // The cascade: every descendant of the revoked token dies too.
        let mut frontier = vec![token];
        while let Some(t) = frontier.pop() {
            let children: Vec<u64> = self
                .grants
                .iter()
                .filter(|g| g.parent == Some(t))
                .map(|g| g.token)
                .collect();
            for c in children {
                if !self.revoked.contains(&c) {
                    self.revoked.push(c);
                    frontier.push(c);
                }
            }
        }
        // Clamp every domain the dead grants had lifted above nominal.
        let dead_domains: Vec<u32> = self
            .grants
            .iter()
            .filter(|g| self.revoked.contains(&g.token))
            .map(|g| g.domain)
            .collect();
        for did in dead_domains {
            if let Some(d) = self.domain_mut(did) {
                if d.current_idx > d.nominal_idx {
                    d.current_idx = d.nominal_idx;
                    d.idle_enter = None;
                    let khz = d.ladder[d.current_idx].khz;
                    self.record(true, did, khz, "clamp", "");
                }
            }
        }
        let _ = now;
        self.record(true, 0, 0, "revoke", "");
    }

    fn is_revoked(&self, token: u64) -> bool {
        self.revoked.contains(&token)
    }

    /// Is this offered token LIVE authority for `domain` up to `khz`?
    fn authorizes(&self, token: u64, domain: u32, khz: u32) -> bool {
        !self.is_revoked(token)
            && self
                .grants
                .iter()
                .any(|g| g.token == token && g.domain == domain && g.max_khz >= khz)
    }

    // -- operating points -----------------------------------------------------

    /// Request an exact operating point. Points at or below nominal are the governor range —
    /// reachable by any caller. Points above nominal are the overclock band: refused with
    /// `NoAuthority` unless one of the offered tokens is a LIVE grant for this domain whose
    /// ceiling reaches the point, and refused with `Cooldown` while the domain is cooling
    /// even with a grant.
    pub fn request_point(
        &mut self,
        domain: u32,
        khz: u32,
        offered: &[u64],
        now: u64,
    ) -> Result<(), PmFault> {
        let (idx, nominal_khz) = match self.domain(domain) {
            Some(d) => match d.ladder.iter().position(|p| p.khz == khz) {
                Some(i) => (i, d.ladder[d.nominal_idx].khz),
                None => {
                    self.record(false, domain, khz, "not-an-operating-point", "");
                    return Err(PmFault::NotAnOperatingPoint { domain, khz });
                }
            },
            None => {
                self.record(false, domain, khz, "unknown-domain", "");
                return Err(PmFault::UnknownDomain(domain));
            }
        };
        if khz > nominal_khz {
            // Cooldown gates the band FIRST: even a valid grant waits out the trip.
            if let Some(remaining) = self.cooldown_remaining(domain, now) {
                self.record(false, domain, khz, "cooldown", "");
                return Err(PmFault::Cooldown {
                    domain,
                    remaining_ticks: remaining,
                });
            }
            let authorized = offered.iter().any(|t| self.authorizes(*t, domain, khz));
            if !authorized {
                // Distinguish "a live grant exists but does not reach" from "nothing live
                // was offered" — both refuse, the names differ, fail-closed either way.
                let best = offered
                    .iter()
                    .filter_map(|t| {
                        self.grants
                            .iter()
                            .find(|g| g.token == *t && g.domain == domain && !self.is_revoked(*t))
                    })
                    .map(|g| g.max_khz)
                    .max();
                match best {
                    Some(granted_khz) => {
                        self.record(false, domain, khz, "not-granted", "");
                        return Err(PmFault::NotGranted {
                            domain,
                            requested_khz: khz,
                            granted_khz,
                        });
                    }
                    None => {
                        self.record(false, domain, khz, "no-authority", "");
                        return Err(PmFault::NoAuthority { domain });
                    }
                }
            }
        }
        if idx != self.domain(domain).unwrap().current_idx {
            self.close_idle(domain, now);
            let d = self.domain_mut(domain).unwrap();
            d.current_idx = idx;
            let got = d.ladder[idx].khz;
            self.record(true, domain, got, "set", "");
        }
        Ok(())
    }

    // -- demand governor --------------------------------------------------------

    /// Record a demand hint (0..=100) for a domain.
    pub fn set_demand(&mut self, domain: u32, pct: u8) -> Result<(), PmFault> {
        if self.domain(domain).is_none() {
            self.record(false, domain, 0, "unknown-domain", "");
            return Err(PmFault::UnknownDomain(domain));
        }
        if pct > 100 {
            self.record(false, domain, 0, "bad-demand", "");
            return Err(PmFault::BadDemand { domain, pct });
        }
        self.domain_mut(domain).unwrap().demand_pct = pct;
        self.record(true, domain, pct as u32, "demand", "");
        Ok(())
    }

    /// One governor step over every domain: a zero-demand domain is PARKED at its lowest
    /// point (the idle machine costs nothing, ADR-056); a demanded domain is raised toward
    /// the demand-mapped point of the GOVERNOR range — never into the overclock band, which
    /// only an authority holder may enter. Returns the number of transitions performed.
    pub fn govern(&mut self, now: u64) -> usize {
        let ids: Vec<u32> = self.domains.iter().map(|d| d.id).collect();
        let mut moves = 0;
        for id in ids {
            let target_idx = {
                let d = self.domain(id).unwrap();
                if d.demand_pct == 0 {
                    0
                } else {
                    // Deterministic demand -> point map over the governor range only.
                    let span = d.nominal_idx + 1;
                    let t = ((d.demand_pct as usize) * span).div_ceil(100);
                    t.max(1) - 1
                }
            };
            if target_idx != self.domain(id).unwrap().current_idx {
                self.close_idle(id, now);
                let d = self.domain_mut(id).unwrap();
                d.current_idx = target_idx;
                moves += 1;
            }
        }
        if moves > 0 {
            self.record(true, 0, 0, "govern", "");
        }
        moves
    }

    // -- thermal ---------------------------------------------------------------

    /// Report a die temperature (milli-degrees C). At or above the domain's trip point the
    /// whole machine clamps: EVERY domain returns to its lowest point and EVERY domain
    /// latches a cooldown during which elevation is refused even with a grant. Heat is the
    /// one authority the hardware holds over the software.
    pub fn report_temperature(&mut self, domain: u32, temp_mc: i32, now: u64) {
        let trip = match self.domain(domain) {
            Some(d) => d.trip_temp_mc,
            None => {
                self.record(false, domain, 0, "unknown-domain", "");
                return;
            }
        };
        if temp_mc >= trip {
            let until = now + COOLDOWN_TICKS;
            let ids: Vec<u32> = self.domains.iter().map(|d| d.id).collect();
            for id in ids {
                self.close_idle(id, now);
                let d = self.domain_mut(id).unwrap();
                d.current_idx = 0;
                d.cooldown_until = until;
            }
            self.record(true, domain, temp_mc.max(0) as u32, "trip", "");
        }
    }

    fn cooldown_remaining(&self, domain: u32, now: u64) -> Option<u64> {
        let d = self.domain(domain)?;
        if d.cooldown_until > now {
            Some(d.cooldown_until - now)
        } else {
            None
        }
    }

    // -- idle --------------------------------------------------------------------

    /// Park a domain in an idle state. A demanded domain refuses (a governor never parks
    /// demanded silicon), an already-parked domain refuses too, and C0 is not a parking
    /// state.
    pub fn enter_idle(&mut self, domain: u32, state: IdleState, now: u64) -> Result<(), PmFault> {
        let (already, demand) = match self.domain(domain) {
            Some(d) => (d.idle_enter.is_some(), d.demand_pct),
            None => {
                self.record(false, domain, 0, "unknown-domain", "");
                return Err(PmFault::UnknownDomain(domain));
            }
        };
        if already {
            self.record(false, domain, 0, "already-idle", "");
            return Err(PmFault::AlreadyIdle(domain));
        }
        if demand > 0 {
            self.record(false, domain, 0, "domain-busy", "");
            return Err(PmFault::DomainBusy {
                domain,
                pct: demand,
            });
        }
        if state == IdleState::C0 {
            self.record(false, domain, 0, "not-an-idle-state", "");
            return Err(PmFault::NotAnIdleState(domain));
        }
        self.domain_mut(domain).unwrap().idle_enter = Some((state, now));
        self.record(true, domain, 0, "idle-enter", "");
        Ok(())
    }

    /// Wake a parked domain: closes its residency accounting and books the wake latency.
    pub fn wake(&mut self, domain: u32, now: u64) -> Result<(), PmFault> {
        let opened = match self.domain_mut(domain) {
            Some(d) => d.idle_enter.take(),
            None => {
                self.record(false, domain, 0, "unknown-domain", "");
                return Err(PmFault::UnknownDomain(domain));
            }
        };
        match opened {
            Some((state, enter)) => {
                let residency = now.saturating_sub(enter);
                let slot = state.slot();
                let d = self.domain_mut(domain).unwrap();
                d.idle_residency_ticks[slot] += residency;
                d.wake_latency_ns_total += state.wake_latency_ns();
                self.record(
                    true,
                    domain,
                    residency.min(u32::MAX as u64) as u32,
                    "wake",
                    "",
                );
                Ok(())
            }
            None => {
                self.record(false, domain, 0, "not-idle", "");
                Err(PmFault::NotIdle(domain))
            }
        }
    }

    /// Idle residency of a domain in ticks, per state — `[C0, C1, C2]`.
    pub fn idle_residency(&self, domain: u32) -> Option<[u64; 3]> {
        self.domain(domain).map(|d| d.idle_residency_ticks)
    }

    /// Total wake latency the domain has paid, in nanoseconds.
    pub fn wake_latency_ns(&self, domain: u32) -> Option<u64> {
        self.domain(domain).map(|d| d.wake_latency_ns_total)
    }

    // -- device power ------------------------------------------------------------

    /// Move a device along a legal power arc: D0 <-> D1 and anything -> D3 -> D0. D3 -> D1
    /// is refused — a device wakes through D0 or not at all.
    pub fn set_device_power(&mut self, device: u32, to: DState) -> Result<(), PmFault> {
        let slot = match self.devices.iter_mut().find(|(d, _)| *d == device) {
            Some(s) => s,
            None => {
                self.record(false, device, 0, "unknown-device", "");
                return Err(PmFault::UnknownDevice(device));
            }
        };
        let legal = matches!(
            (slot.1, to),
            (DState::D0, DState::D1)
                | (DState::D1, DState::D0)
                | (DState::D0, DState::D3)
                | (DState::D1, DState::D3)
                | (DState::D3, DState::D0)
        );
        if !legal {
            let from = slot.1;
            self.record(false, device, 0, "illegal-dstate", "");
            return Err(PmFault::IllegalDState { device, from, to });
        }
        slot.1 = to;
        self.record(true, device, 0, "dstate", "");
        Ok(())
    }

    pub fn device_state(&self, device: u32) -> Option<DState> {
        self.devices
            .iter()
            .find(|(d, _)| *d == device)
            .map(|(_, s)| *s)
    }

    // -- observation ---------------------------------------------------------

    /// Current operating point of a domain, in kHz.
    pub fn current_khz(&self, domain: u32) -> Option<u32> {
        self.domain(domain).map(|d| d.ladder[d.current_idx].khz)
    }

    pub fn nominal_khz(&self, domain: u32) -> Option<u32> {
        self.domain(domain).map(|d| d.ladder[d.nominal_idx].khz)
    }

    pub fn envelope_khz(&self, domain: u32) -> Option<u32> {
        self.domain(domain).map(|d| d.envelope_khz)
    }

    /// The current point of every domain, in registration order — the isolation invariant
    /// reads this to prove a change to one domain leaves the others untouched.
    pub fn all_current_khz(&self) -> Vec<(u32, u32)> {
        self.domains
            .iter()
            .map(|d| (d.id, d.ladder[d.current_idx].khz))
            .collect()
    }

    pub fn transitions(&self) -> usize {
        self.accepted
    }

    pub fn refusals(&self) -> usize {
        self.refusals
    }

    /// The audit ledger, oldest first, capacity-bounded (wraps). Sequence numbers are
    /// monotonic across wraps: the ledger forgets bytes, never events.
    pub fn audit(&self) -> &[AuditRecord] {
        &self.audit
    }

    /// Highest sequence number ever issued — the ledger's completeness witness.
    pub fn audit_sequence(&self) -> u64 {
        self.audit_next_seq - 1
    }

    // -- internals -------------------------------------------------------------

    fn domain(&self, id: u32) -> Option<&Domain> {
        self.domains.iter().find(|d| d.id == id)
    }

    fn domain_mut(&mut self, id: u32) -> Option<&mut Domain> {
        self.domains.iter_mut().find(|d| d.id == id)
    }

    /// Close an open idle span, booking its residency up to `now`. Called by any act that
    /// moves a parked domain's clock: the residency is real time and is not lost. Wake
    /// LATENCY is booked only by `wake` — a parked domain interrupted by a clock change
    /// paid ticks, not a wake.
    fn close_idle(&mut self, id: u32, now: u64) {
        let opened = match self.domain_mut(id) {
            Some(d) => d.idle_enter.take(),
            None => return,
        };
        if let Some((state, enter)) = opened {
            let residency = now.saturating_sub(enter);
            let slot = state.slot();
            let d = self.domain_mut(id).unwrap();
            d.idle_residency_ticks[slot] += residency;
        }
    }

    fn record(
        &mut self,
        accepted: bool,
        domain: u32,
        khz: u32,
        kind: &'static str,
        holder: &'static str,
    ) {
        if accepted {
            self.accepted += 1;
        } else {
            self.refusals += 1;
        }
        let rec = AuditRecord {
            seq: self.audit_next_seq,
            accepted,
            domain,
            khz,
            kind,
            holder,
        };
        self.audit_next_seq += 1;
        if self.audit.len() == AUDIT_CAP {
            self.audit.remove(0);
        }
        self.audit.push(rec);
    }
}

// ---------------------------------------------------------------------------
// The in-kernel invariant suite. Kept SMALL by design: the boot heap never
// frees (ADR-063), so the boot proves the core contract while the exhaustive
// sweeps and the decision tables live in tests/pm.rs on the host.
// ---------------------------------------------------------------------------
pub fn pm_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            report(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }

    // A small two-domain platform: cluster A can be pushed past nominal (2.0 GHz) to 2.4/2.8
    // GHz; cluster B's ladder tops out AT nominal. Ladder points are kHz; the envelope tops
    // both ladders. Temperatures are milli-degrees C.
    let ladder_a = [
        OperatingPoint {
            khz: 800_000,
            mv: 700,
        },
        OperatingPoint {
            khz: 1_200_000,
            mv: 800,
        },
        OperatingPoint {
            khz: 2_000_000,
            mv: 900,
        }, // nominal
        OperatingPoint {
            khz: 2_400_000,
            mv: 1000,
        }, // OC band
        OperatingPoint {
            khz: 2_800_000,
            mv: 1100,
        }, // OC band, == envelope
    ];
    let ladder_b = [
        OperatingPoint {
            khz: 500_000,
            mv: 600,
        },
        OperatingPoint {
            khz: 1_000_000,
            mv: 700,
        },
    ];
    const A: u32 = 1;
    const B: u32 = 2;

    let mut pm = PmEngine::new(0x00C0_FFEE);
    pm.register_domain(A, &ladder_a, 2_000_000, 2_800_000, 95_000)
        .unwrap();
    pm.register_domain(B, &ladder_b, 1_000_000, 1_000_000, 95_000)
        .unwrap();

    // 1 - fail-closed default: a fresh machine sits at its LOWEST points with NO grants, and
    // every point above nominal is refused with nothing offered.
    let fresh_ok = pm.all_current_khz() == vec![(A, 800_000), (B, 500_000)];
    let no_auth = pm.request_point(A, 2_400_000, &[], 0);
    check!(
        fresh_ok && matches!(no_auth, Err(PmFault::NoAuthority { domain: A })),
        "pm: a fresh machine idles at its lowest points and elevation without authority is refused by name"
    );

    // 2 - the governor range is free: every point at or below nominal is reachable by anyone.
    let gov_ok = pm.request_point(A, 1_200_000, &[], 0).is_ok()
        && pm.current_khz(A) == Some(1_200_000)
        && pm.request_point(A, 2_000_000, &[], 0).is_ok()
        && pm.current_khz(A) == Some(2_000_000);
    check!(
        gov_ok,
        "pm: the governor range (at or below nominal) needs no authority"
    );

    // 3 - the overclock band is grants only: minted exactly to a ladder point, refused off it.
    let root = pm.mint_grant(A, 2_800_000, "platform-owner").unwrap();
    check!(
        pm.request_point(A, 2_400_000, &[root], 0).is_ok()
            && pm.current_khz(A) == Some(2_400_000)
            && matches!(
                pm.request_point(A, 2_799_999, &[root], 0),
                Err(PmFault::NotAnOperatingPoint { domain: A, .. })
            ),
        "pm: elevation into the overclock band works exactly at declared operating points"
    );

    // 4 - attenuation: a child grant never widens its parent, and cannot leak across domains.
    let child = pm.delegate(root, A, 2_400_000, "agent").unwrap();
    pm.request_point(A, 2_000_000, &[], 0).unwrap();
    let below_child = pm.request_point(A, 2_400_000, &[child], 0).is_ok();
    let above_child = pm.request_point(A, 2_800_000, &[child], 0);
    let cross = pm.delegate(root, B, 1_000_000, "agent");
    check!(
        below_child
            && matches!(
                above_child,
                Err(PmFault::NotGranted {
                    domain: A,
                    requested_khz: 2_800_000,
                    granted_khz: 2_400_000
                })
            )
            && matches!(cross, Err(PmFault::CrossDomain { .. })),
        "pm: delegation attenuates (never amplifies) and never crosses domains"
    );

    // 5 - a forged token is not authority.
    check!(
        matches!(
            pm.request_point(A, 2_800_000, &[0xDEAD_BEEF], 0),
            Err(PmFault::NoAuthority { domain: A })
        ),
        "pm: a fabricated token is not elevation authority"
    );

    // 6 - the envelope is absolute: no grant may be MINTED past it, so no reachable state
    // exceeds it — the ceiling is structural, not a policy anyone remembers to apply.
    check!(
        matches!(
            pm.mint_grant(A, 3_000_000, "owner"),
            Err(PmFault::AboveEnvelope {
                domain: A,
                requested_khz: 3_000_000,
                envelope_khz: 2_800_000
            })
        ),
        "pm: the thermal envelope is absolute - no grant is minted past it"
    );

    // 7 - revocation clamps immediately: a domain running in the OC band under a dead grant
    // is back at nominal before revoke returns; the cascade kills the child too.
    pm.request_point(A, 2_800_000, &[root], 0).unwrap();
    pm.revoke(root, 4_000);
    check!(
        pm.current_khz(A) == Some(2_000_000)
            && matches!(
                pm.request_point(A, 2_400_000, &[child], 0),
                Err(PmFault::NoAuthority { domain: A })
            ),
        "pm: a revoked grant clamps the domain back to nominal immediately and cascades"
    );

    // 8 - a thermal trip clamps EVERY domain and latches a cooldown: elevation refused BY
    // NAME (remaining ticks named) even with a fresh valid grant, while the governor range
    // keeps serving the machine, and elevation returns exactly at expiry.
    let root2 = pm.mint_grant(A, 2_800_000, "platform-owner").unwrap();
    pm.request_point(A, 2_400_000, &[root2], 0).unwrap();
    pm.request_point(B, 1_000_000, &[], 0).unwrap();
    pm.report_temperature(A, 96_000, 5_000); // 96.0 C >= the 95.0 C trip point
    let clamped = pm.current_khz(A) == Some(800_000) && pm.current_khz(B) == Some(500_000);
    let cooled = pm.request_point(A, 2_400_000, &[root2], 5_002);
    let still_cooling = pm.request_point(A, 2_400_000, &[root2], 5_998);
    let b_governor_ok = pm.request_point(B, 1_000_000, &[], 6_000).is_ok();
    let expired_ok = pm.request_point(A, 2_400_000, &[root2], 6_000).is_ok();
    check!(
        clamped
            && matches!(
                cooled,
                Err(PmFault::Cooldown {
                    domain: A,
                    remaining_ticks: 998
                })
            )
            && matches!(still_cooling, Err(PmFault::Cooldown { .. }))
            && b_governor_ok
            && expired_ok,
        "pm: a thermal trip clamps every domain and refuses elevation until the cooldown expires"
    );

    // 9 - the governor never enters the overclock band, whatever the demand says.
    pm.set_demand(A, 100).unwrap();
    pm.govern(20_000);
    check!(
        pm.current_khz(A) == Some(2_000_000),
        "pm: the demand governor never enters the overclock band"
    );

    // 10 - zero demand parks the domain at its lowest point (the idle machine costs nothing,
    // ADR-056).
    pm.set_demand(A, 0).unwrap();
    pm.govern(21_000);
    check!(
        pm.current_khz(A) == Some(800_000),
        "pm: a zero-demand domain is parked at its lowest point"
    );

    // 11 - a demanded domain is never parked, an already-parked one is not re-parked, and
    // the idle residency + wake latency are accounted exactly.
    pm.set_demand(A, 10).unwrap();
    let busy = pm.enter_idle(A, IdleState::C1, 22_000);
    pm.set_demand(A, 0).unwrap();
    pm.enter_idle(A, IdleState::C1, 22_000).unwrap();
    let double = pm.enter_idle(A, IdleState::C2, 22_100);
    pm.wake(A, 23_000).unwrap();
    check!(
        matches!(busy, Err(PmFault::DomainBusy { domain: A, pct: 10 }))
            && matches!(double, Err(PmFault::AlreadyIdle(A)))
            && pm.idle_residency(A) == Some([0, 1_000, 0])
            && pm.wake_latency_ns(A) == Some(1_000),
        "pm: demanded silicon is never parked; idle residency and wake latency are accounted exactly"
    );

    // 12 - device power arcs: D0->D1->D0->D3->D0 all legal, D3->D1 refused BY NAME (wake
    // through D0), unknown devices refused.
    pm.register_device(9).unwrap();
    let arcs = pm.set_device_power(9, DState::D1).is_ok()
        && pm.set_device_power(9, DState::D0).is_ok()
        && pm.set_device_power(9, DState::D3).is_ok()
        && matches!(
            pm.set_device_power(9, DState::D1),
            Err(PmFault::IllegalDState {
                device: 9,
                from: DState::D3,
                to: DState::D1
            })
        )
        && pm.set_device_power(9, DState::D0).is_ok();
    check!(
        arcs && matches!(
            pm.set_device_power(77, DState::D0),
            Err(PmFault::UnknownDevice(77))
        ),
        "pm: device power moves only along legal arcs and every refusal is named"
    );

    // 13 - domain isolation: raising domain A never moves domain B, and the ledger counted
    // every accepted transition and every refusal.
    let before = pm.all_current_khz();
    pm.request_point(A, 1_200_000, &[], 24_000).unwrap();
    let after = pm.all_current_khz();
    let b_untouched = before[1] == after[1] && before[0].0 == A && after[0].0 == A;
    let ledger_ok = pm.audit_sequence() as usize >= pm.transitions() + pm.refusals()
        && pm.transitions() > 10
        && pm.refusals() >= 6;
    check!(
        b_untouched && ledger_ok,
        "pm: domains are isolated from each other and the audit ledger counts everything"
    );

    // 14 - the ledger sequence never rewinds and every record is well-formed (accepted
    // records and refusals both carried, in order, with a class).
    let ledger = pm.audit();
    let ordered = ledger.windows(2).all(|w| w[0].seq < w[1].seq);
    let classified = ledger.iter().all(|r| !r.kind.is_empty());
    check!(
        ordered && classified && !ledger.is_empty(),
        "pm: the audit ledger is an ordered, classified record of everything the boundary did"
    );

    Ok(n)
}
