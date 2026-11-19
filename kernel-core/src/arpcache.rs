//! A bounded ARP cache — the answer to "who has this address?" you already asked once (REQ-NET-003, ADR-060).
//!
//! The first networking slice resolved the gateway by broadcasting an ARP request EVERY time, because
//! the register row honestly listed "no ARP cache" among what was missing. Repeating a broadcast for
//! an address you resolved microseconds ago is not humility, it is traffic: every resolve costs a
//! wire round trip, and a stack that cannot remember answers forces the network to answer the same
//! question forever. This module is the memory — written as a FIXED-SIZE table because a cache that
//! grows with the number of peers is an allocation disguised as an optimization, and the kernel
//! refuses to allocate on the resolve path.
//!
//! ## The policy, stated once
//!
//! * **Exact keys.** An IP is four bytes and they are compared as four bytes; 10.0.2.2 and
//!   10.0.2.20 share no prefix logic here, because an ARP cache that "closely" matches is a spoof.
//! * **Refresh, not duplicate.** Re-inserting a known key UPDATES that slot (address bindings change,
//!   e.g. after a peer rebooted onto a new NIC) — it never consumes a second slot.
//! * **LRU eviction, by construction.** When the table is full the entry unused longest is replaced.
//!   Every operation carries a monotonic tick, so "least recently used" is a fact about the recorded
//!   order, not a heuristic.
//! * **Bounded is provable.** len() can never exceed N; the suite asserts it after more than N
//!   distinct inserts, which is the whole point of choosing N deliberately.

/// How many address bindings one cache remembers. The gates talk to one gateway; eight leaves room
/// for DNS, a second host and a future default route without ever allocating.
pub const DEFAULT_ENTRIES: usize = 8;

/// How long a binding is believed without being learned again (ADR-265). A binding that is never
/// refreshed outlives the host that answered: an address reassigned, or a forged answer that got
/// in, would otherwise be trusted until the machine reboots. Sixty seconds is RFC 1122's suggested
/// order (section 2.3.2.1) and costs one broadcast a minute per peer in use.
pub const MAX_AGE_NS: u64 = 60_000_000_000;

/// A fixed-capacity IP-to-MAC cache with refresh-in-place and LRU eviction.
pub struct ArpCache<const N: usize = DEFAULT_ENTRIES> {
    /// None marks a free slot; occupancy is derivable from this array alone.
    slots: [Option<Entry>; N],
    /// Monotonic usage clock; saturates far beyond any realistic access count.
    tick: u64,
}

struct Entry {
    ip: [u8; 4],
    mac: [u8; 6],
    last_use: u64,
    /// When the wire last told us this binding, on the caller's nanosecond clock.
    learned_ns: u64,
}

impl<const N: usize> ArpCache<N> {
    /// An empty cache.
    pub fn new() -> Self {
        ArpCache {
            slots: [const { None }; N],
            tick: 0,
        }
    }

    /// Remember that ip is reachable at mac. Refreshes an existing binding in place;
    /// otherwise fills a free slot; otherwise replaces the least-recently-used entry.
    /// `now_ns` is when the wire said so; the binding expires [`MAX_AGE_NS`] later.
    pub fn insert(&mut self, ip: [u8; 4], mac: [u8; 6], now_ns: u64) {
        if N == 0 {
            // A cache with no slots remembers nothing. The constant condition compiles away for
            // every real N; without it, the eviction scan below would index an empty array.
            return;
        }
        self.tick += 1;
        let now = self.tick;
        // Refresh in place: a known key never costs a second slot.
        for slot in self.slots.iter_mut().flatten() {
            if slot.ip == ip {
                slot.mac = mac;
                slot.last_use = now;
                slot.learned_ns = now_ns;
                return;
            }
        }
        // Free slot first, so a cache that never fills pays nothing for eviction.
        if let Some(free) = self.slots.iter_mut().find(|s| s.is_none()) {
            *free = Some(Entry {
                ip,
                mac,
                last_use: now,
                learned_ns: now_ns,
            });
            return;
        }
        // Full: replace the entry unused longest. With N >= 1 some slot exists, so the scan
        // always finds a victim.
        let mut oldest = 0usize;
        for (i, slot) in self.slots.iter().enumerate() {
            let used = slot.as_ref().map(|e| e.last_use).unwrap_or(u64::MAX);
            let best = self.slots[oldest]
                .as_ref()
                .map(|e| e.last_use)
                .unwrap_or(u64::MAX);
            if used < best {
                oldest = i;
            }
        }
        self.slots[oldest] = Some(Entry {
            ip,
            mac,
            last_use: now,
            learned_ns: now_ns,
        });
    }

    /// The MAC ip was last bound to, refreshing its recency on the way out (this IS a use).
    /// A binding older than [`MAX_AGE_NS`] at `now_ns` is forgotten and misses, so the caller asks
    /// the wire again. A clock that went backwards (`now_ns` before `learned_ns`) counts as expired
    /// too: an age that cannot be measured cannot be trusted.
    pub fn lookup(&mut self, ip: [u8; 4], now_ns: u64) -> Option<[u8; 6]> {
        self.tick += 1;
        let now = self.tick;
        let slot = self
            .slots
            .iter_mut()
            .find(|s| s.as_ref().is_some_and(|e| e.ip == ip))?;
        let e = slot.as_mut()?;
        match now_ns.checked_sub(e.learned_ns) {
            Some(age) if age <= MAX_AGE_NS => {
                e.last_use = now;
                Some(e.mac)
            }
            _ => {
                *slot = None;
                None
            }
        }
    }

    /// Live bindings currently held — never more than N, by construction and by proof.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Whether nothing is cached. A cache reports emptiness rather than making callers guess.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<const N: usize> Default for ArpCache<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> [u8; 4] {
        [a, b, c, d]
    }

    fn mac(seed: u8) -> [u8; 6] {
        [0x52, 0x54, 0x00, 0x00, 0x00, seed]
    }

    #[test]
    fn a_looked_up_binding_returns_the_mac_it_was_inserted_with() {
        let mut c = ArpCache::<4>::new();
        assert!(
            c.lookup(ip(10, 0, 2, 2), 0).is_none(),
            "an empty cache answers nothing"
        );
        c.insert(ip(10, 0, 2, 2), mac(2), 0);
        assert_eq!(c.lookup(ip(10, 0, 2, 2), 0), Some(mac(2)));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn addresses_match_byte_exactly_no_prefix_no_closeness() {
        let mut c = ArpCache::<4>::new();
        c.insert(ip(10, 0, 2, 2), mac(2), 0);
        // Every one-byte variation misses: a cache that "nearly" matches would hand a frame
        // addressed to one host to another host's link address.
        for d in 0..=255u8 {
            if d == 2 {
                continue;
            }
            assert_eq!(
                c.lookup(ip(10, 0, 2, d), 0),
                None,
                "byte {d} must not alias .2"
            );
        }
    }

    #[test]
    fn reinserting_a_known_key_refreshes_in_place_and_evicts_nothing() {
        let mut c = ArpCache::<4>::new();
        c.insert(ip(1, 0, 0, 1), mac(1), 0);
        c.insert(ip(1, 0, 0, 2), mac(2), 0);
        c.insert(ip(1, 0, 0, 3), mac(3), 0);
        c.insert(ip(1, 0, 0, 4), mac(4), 0);
        // The binding CHANGED for .1 (peer rebooted onto a new NIC) — update, not duplicate.
        c.insert(ip(1, 0, 0, 1), mac(9), 0);
        assert_eq!(c.len(), 4, "a refresh must not consume a second slot");
        assert_eq!(c.lookup(ip(1, 0, 0, 1), 0), Some(mac(9)));
        // The untouched siblings are all still resident.
        for s in 2..=4u8 {
            assert_eq!(c.lookup(ip(1, 0, 0, s), 0), Some(mac(s)));
        }
    }

    #[test]
    fn the_table_never_holds_more_than_its_bound_and_the_unused_longest_is_replaced() {
        let mut c = ArpCache::<4>::new();
        for s in 0..4u8 {
            c.insert(ip(10, 0, 0, s), mac(s), 0);
        }
        assert_eq!(c.len(), 4);
        // Touch .0 so .1 becomes the entry unused longest…
        assert_eq!(c.lookup(ip(10, 0, 0, 0), 0), Some(mac(0)));
        // …then overflow: the victim must be .1, and only .1.
        c.insert(ip(10, 0, 0, 7), mac(7), 0);
        assert_eq!(c.len(), 4, "bounded means bounded");
        assert_eq!(
            c.lookup(ip(10, 0, 0, 1), 0),
            None,
            "the LRU entry was replaced"
        );
        for s in [0u8, 2, 3] {
            assert_eq!(c.lookup(ip(10, 0, 0, s), 0), Some(mac(s)), ".{s} survived");
        }
        assert_eq!(c.lookup(ip(10, 0, 0, 7), 0), Some(mac(7)));
    }

    #[test]
    fn a_random_operation_stream_agrees_with_a_reference_model() {
        // A tiny xorshift stream (deterministic across hosts) drives inserts and lookups; a
        // straightforward Vec model states what the observable behavior must be after every step.
        struct X(u64);
        impl X {
            fn next(&mut self) -> u64 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                self.0
            }
        }
        let mut rng = X(0x9E3779B97F4A7C15);
        let mut c = ArpCache::<8>::new();
        let mut order: alloc::vec::Vec<([u8; 4], [u8; 6])> = alloc::vec![]; // LRU order, front = oldest
        for step in 0..4000u64 {
            let r = rng.next();
            let s = (r % 12) as u8; // more keys than slots, so eviction is exercised constantly
            let ip = ip(192, 168, 0, s);
            let m = mac((r >> 16) as u8);
            if r & 1 == 0 {
                c.insert(ip, m, 0);
                match order.iter().position(|(k, _)| *k == ip) {
                    Some(p) => {
                        order.remove(p);
                        order.push((ip, m)); // refresh moves to the most-recent end
                    }
                    None => {
                        if order.len() == 8 {
                            order.remove(0);
                        }
                        order.push((ip, m));
                    }
                }
            } else {
                let got = c.lookup(ip, 0);
                let want = order.iter().find(|(k, _)| *k == ip).map(|(_, m)| *m);
                assert_eq!(
                    got, want,
                    "step {step}: cache diverged from the model for {ip:?}"
                );
            }
            assert_eq!(c.len(), order.len(), "step {step}: occupancy diverged");
        }
    }

    #[test]
    fn a_binding_expires_after_its_max_age_and_a_backwards_clock_expires_it_too() {
        let mut c = ArpCache::<4>::new();
        c.insert(ip(10, 0, 2, 2), mac(2), 1_000);
        assert_eq!(c.lookup(ip(10, 0, 2, 2), 1_000 + MAX_AGE_NS), Some(mac(2)));
        assert_eq!(c.lookup(ip(10, 0, 2, 2), 1_001 + MAX_AGE_NS), None);
        assert_eq!(c.len(), 0, "an expired binding frees its slot");
        c.insert(ip(10, 0, 2, 2), mac(3), 5_000);
        assert_eq!(
            c.lookup(ip(10, 0, 2, 2), 4_999),
            None,
            "an age that cannot be measured"
        );
        // Learning the binding again restarts its age.
        c.insert(ip(10, 0, 2, 2), mac(4), 0);
        c.insert(ip(10, 0, 2, 2), mac(4), MAX_AGE_NS);
        assert_eq!(c.lookup(ip(10, 0, 2, 2), 2 * MAX_AGE_NS), Some(mac(4)));
    }
}
