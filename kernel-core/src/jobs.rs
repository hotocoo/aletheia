//! Programs left running in the background (ADR-213): the table every target keeps them in.
//!
//! A target places a program in its own address space (ADR-201..212) and hands the result - its
//! `S`, whatever that target needs to resume it - to this table under a name. The console's idle
//! loop then asks for the next job's turn, runs one slice of it, and takes it out when it ends.
//! Fixed slots and a rotating cursor: starting a job is the only thing here that can fail, and
//! nothing here allocates, because a turn is taken on every idle pass of the console.
//!
//! Under memory pressure the table is also what a running machine reclaims from (ADR-238): each
//! job keeps the vector it was admitted with, and [`Jobs::reclaim`] hands the eviction forest's
//! ranking (`reclaim.rs`, ADR-082) the jobs as candidates and takes out the ones it chooses.

use alloc::vec::Vec;

use crate::frameown::Owner;
use crate::mlrisk_contract::N_FEATURES;
use crate::mlsched::MemoryMeter;
use crate::priosched::Priority;
use crate::reclaim::{Candidate, ReclaimOps, ReclaimOutcome, ReclaimRefusal, Reclaimer};
use crate::sched::TaskId;

/// Most programs left running at once.
pub const MAX_JOBS: usize = 4;

/// A job's name, kept by value so a job outlives the line that started it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobName {
    bytes: [u8; crate::fs::MAX_NAME],
    len: usize,
}

impl JobName {
    /// The name, cut at [`crate::fs::MAX_NAME`] bytes (a namespace name is never longer) and at a
    /// character boundary.
    pub fn new(name: &str) -> Self {
        let mut len = name.len().min(crate::fs::MAX_NAME);
        while !name.is_char_boundary(len) {
            len -= 1;
        }
        let mut bytes = [0u8; crate::fs::MAX_NAME];
        bytes[..len].copy_from_slice(&name.as_bytes()[..len]);
        JobName { bytes, len }
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

/// One program left running.
pub struct Job<S> {
    /// What the console calls it: `kill ID`. Never reused while the machine runs.
    pub id: u32,
    pub name: JobName,
    /// The target's own state for it (address space, saved registers, output).
    pub slot: S,
    /// What it was admitted with (ADR-238).
    pub admission: Admission,
}

/// What the resident advisor was told when a job was admitted: the vector the eviction forest is
/// asked about if the machine later runs short, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admission {
    pub features: [i32; N_FEATURES],
    pub submitted_secs: u64,
}

impl Admission {
    /// No vector was recorded (no advisor installed): the forest abstains about a constant row.
    pub const NONE: Admission = Admission {
        features: [0; N_FEATURES],
        submitted_secs: 0,
    };
}

/// What `jobs` shows of a job, copied out of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobFacts {
    pub id: u32,
    pub name: JobName,
    /// Slices it has been given so far.
    pub slices: u32,
}

/// The table.
pub struct Jobs<S> {
    jobs: [Option<Job<S>>; MAX_JOBS],
    /// The id the next job gets.
    next_id: u32,
    /// Where the next turn starts looking.
    turn: usize,
}

impl<S> Default for Jobs<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Jobs<S> {
    pub const fn new() -> Self {
        Jobs {
            jobs: [const { None }; MAX_JOBS],
            next_id: 1,
            turn: 0,
        }
    }

    /// Leave `slot` running as `name`; its id, or the slot handed back when every place is taken
    /// (the caller then gives back what the slot holds).
    pub fn add(&mut self, name: &str, slot: S, admission: Admission) -> Result<u32, S> {
        let Some(free) = self.jobs.iter().position(Option::is_none) else {
            return Err(slot);
        };
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.jobs[free] = Some(Job {
            id,
            name: JobName::new(name),
            slot,
            admission,
        });
        Ok(id)
    }

    /// The job whose turn it is: each live job in place order, round and round.
    pub fn next_turn(&mut self) -> Option<&mut Job<S>> {
        let at = (0..MAX_JOBS)
            .map(|i| (self.turn + i) % MAX_JOBS)
            .find(|&i| self.jobs[i].is_some())?;
        self.turn = (at + 1) % MAX_JOBS;
        self.jobs[at].as_mut()
    }

    /// Take a job out (it ended, or the operator killed it).
    pub fn take(&mut self, id: u32) -> Option<Job<S>> {
        self.jobs
            .iter_mut()
            .find(|j| j.as_ref().is_some_and(|j| j.id == id))
            .and_then(Option::take)
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.iter().all(Option::is_none)
    }

    /// Every live job, in place order.
    pub fn iter(&self) -> impl Iterator<Item = &Job<S>> {
        self.jobs.iter().flatten()
    }

    /// One reclaim round over the running jobs (ADR-238). `meter` is the allocator's reading;
    /// `pages` says how many frames a job's slot holds. Refused by name, with the table untouched
    /// and nothing allocated, when the meter is not under pressure; otherwise the reclaimer ranks
    /// the jobs (forest tier, then largest footprint, then oldest) and the chosen ones are taken
    /// out of the table and handed back, for the caller to give their frames back and report.
    pub fn reclaim(
        &mut self,
        reclaimer: &mut Reclaimer,
        meter: MemoryMeter,
        pages: impl Fn(&S) -> u64,
    ) -> Result<(ReclaimOutcome, Vec<Job<S>>), ReclaimRefusal> {
        // Asked after every slice: a machine that is not short is answered here, before anything
        // is built or counted, so the idle loop's common case costs one comparison.
        if !meter.under_pressure() {
            return Err(ReclaimRefusal::NotUnderPressure {
                free_pages: meter.free_pages,
                total_pages: meter.total_pages,
            });
        }
        let candidates: Vec<Candidate> = self
            .iter()
            .map(|j| Candidate {
                task: TaskId(j.id as u64),
                // A tag naming the job; the jobs are taken out by id, never by owner.
                owner: Owner::address_space(j.id % 64).unwrap_or(Owner::KERNEL),
                footprint_pages: pages(&j.slot),
                // Every background program is admitted at the console's one priority.
                priority: Priority(5),
                submitted_secs: j.admission.submitted_secs,
                protected: false,
                features: j.admission.features,
            })
            .collect();
        let mut ops = TakeOps {
            jobs: self,
            pages: &pages,
            taken: Vec::new(),
        };
        let outcome = reclaimer.reclaim(meter, &candidates, &mut ops)?;
        Ok((outcome, ops.taken))
    }
}

/// The reclaimer's execution seam over the job table: evicting a job takes it out.
struct TakeOps<'j, S, F> {
    jobs: &'j mut Jobs<S>,
    pages: &'j F,
    taken: Vec<Job<S>>,
}

impl<S, F: Fn(&S) -> u64> ReclaimOps for TakeOps<'_, S, F> {
    fn evict(&mut self, task: TaskId, _owner: Owner) -> u64 {
        match self.jobs.take(task.0 as u32) {
            Some(job) => {
                let n = (self.pages)(&job.slot);
                self.taken.push(job);
                n
            }
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn jobs_take_turns_in_place_order_and_a_taken_one_leaves() {
        let mut t: Jobs<u8> = Jobs::new();
        let a = t.add("a", 10, Admission::NONE).unwrap();
        let b = t.add("b", 20, Admission::NONE).unwrap();
        let c = t.add("c", 30, Admission::NONE).unwrap();
        assert_eq!((a, b, c), (1, 2, 3));
        let turns: Vec<u32> = (0..6).map(|_| t.next_turn().unwrap().id).collect();
        assert_eq!(turns, [1, 2, 3, 1, 2, 3]);
        assert_eq!(t.take(b).map(|j| j.slot), Some(20));
        let turns: Vec<u32> = (0..4).map(|_| t.next_turn().unwrap().id).collect();
        assert_eq!(turns, [1, 3, 1, 3]);
        assert!(t.take(b).is_none());
    }

    #[test]
    fn a_full_table_hands_the_slot_back_and_ids_are_never_reused() {
        let mut t: Jobs<u8> = Jobs::new();
        for i in 0..MAX_JOBS as u8 {
            t.add("x", i, Admission::NONE).unwrap();
        }
        assert_eq!(t.add("y", 99, Admission::NONE), Err(99));
        t.take(2).unwrap();
        assert_eq!(t.add("y", 99, Admission::NONE), Ok(MAX_JOBS as u32 + 1));
        let ids: Vec<u32> = t.iter().map(|j| j.id).collect();
        for id in ids {
            t.take(id);
        }
        assert!(t.is_empty());
        assert!(t.next_turn().is_none());
    }

    fn meter(free: u64) -> MemoryMeter {
        MemoryMeter {
            total_pages: 1000,
            free_pages: free,
        }
    }

    #[test]
    fn no_pressure_no_round_and_the_table_is_untouched() {
        let mut t: Jobs<u64> = Jobs::new();
        t.add("a", 40, Admission::NONE).unwrap();
        let mut r = Reclaimer::without_model();
        let got = t.reclaim(&mut r, meter(500), |s| *s);
        assert!(matches!(got, Err(ReclaimRefusal::NotUnderPressure { .. })));
        assert_eq!(t.iter().count(), 1);
        assert_eq!(r.ledger().evictions, 0);
    }

    #[test]
    fn pressure_takes_the_largest_job_first_until_the_need_is_met() {
        let mut t: Jobs<u64> = Jobs::new();
        let small = t.add("small", 30, Admission::NONE).unwrap();
        let big = t.add("big", 150, Admission::NONE).unwrap();
        let mid = t.add("mid", 60, Admission::NONE).unwrap();
        let mut r = Reclaimer::without_model();
        // 50 free of 1000: the need is back to twice the 10 % watermark, 200 - 50 = 150 frames.
        let (out, gone) = t.reclaim(&mut r, meter(50), |s| *s).unwrap();
        assert_eq!(out.need, 150);
        assert_eq!(out.frames_reclaimed, 150);
        assert_eq!(out.shortfall, 0);
        assert_eq!(gone.iter().map(|j| j.id).collect::<Vec<_>>(), [big]);
        let left: Vec<u32> = t.iter().map(|j| j.id).collect();
        assert_eq!(left, [small, mid]);
        assert_eq!(r.ledger().evictions, 1);
    }

    #[test]
    fn pressure_with_too_little_to_take_takes_everything_and_names_the_shortfall() {
        let mut t: Jobs<u64> = Jobs::new();
        t.add("a", 10, Admission::NONE).unwrap();
        t.add("b", 20, Admission::NONE).unwrap();
        let mut r = Reclaimer::without_model();
        let (out, gone) = t.reclaim(&mut r, meter(0), |s| *s).unwrap();
        assert_eq!(gone.len(), 2);
        assert_eq!(out.frames_reclaimed, 30);
        assert_eq!(out.shortfall, 200 - 30);
        assert!(t.is_empty());
        let empty = t.reclaim(&mut r, meter(0), |s| *s);
        assert!(matches!(
            empty,
            Err(ReclaimRefusal::NothingEvictable { .. })
        ));
    }

    #[test]
    fn a_name_is_kept_whole_or_cut_at_a_character() {
        assert_eq!(JobName::new("spin").as_str(), "spin");
        let long = "é".repeat(30); // 60 bytes
        let kept = JobName::new(&long);
        assert!(kept.as_str().len() <= crate::fs::MAX_NAME);
        assert!(long.starts_with(kept.as_str()));
    }
}
