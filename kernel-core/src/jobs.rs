//! Programs left running in the background (ADR-213): the table every target keeps them in.
//!
//! A target places a program in its own address space (ADR-201..212) and hands the result - its
//! `S`, whatever that target needs to resume it - to this table under a name. The console's idle
//! loop then asks for the next job's turn, runs one slice of it, and takes it out when it ends.
//! Fixed slots and a rotating cursor: starting a job is the only thing here that can fail, and
//! nothing here allocates, because a turn is taken on every idle pass of the console.

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
    pub fn add(&mut self, name: &str, slot: S) -> Result<u32, S> {
        let Some(free) = self.jobs.iter().position(Option::is_none) else {
            return Err(slot);
        };
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.jobs[free] = Some(Job {
            id,
            name: JobName::new(name),
            slot,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn jobs_take_turns_in_place_order_and_a_taken_one_leaves() {
        let mut t: Jobs<u8> = Jobs::new();
        let a = t.add("a", 10).unwrap();
        let b = t.add("b", 20).unwrap();
        let c = t.add("c", 30).unwrap();
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
            t.add("x", i).unwrap();
        }
        assert_eq!(t.add("y", 99), Err(99));
        t.take(2).unwrap();
        assert_eq!(t.add("y", 99), Ok(MAX_JOBS as u32 + 1));
        let ids: Vec<u32> = t.iter().map(|j| j.id).collect();
        for id in ids {
            t.take(id);
        }
        assert!(t.is_empty());
        assert!(t.next_turn().is_none());
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
