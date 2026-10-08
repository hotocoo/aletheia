//! Stable user-mode syscall numbers, capability mapping, and fail-closed decoding.
//!
//! Targets still own trap-frame mechanics and syscall effects. This module owns the ABI names and
//! numbers so aarch64 `svc`, RISC-V `ecall`, and x86-64 `int 0x80` cannot silently drift apart.
//! Unknown numbers decode to `None`; callers must return their architecture's failure value.

/// Syscall number used by the live user-mode suites to authorize one audited event.
pub const SYS_EMIT: u64 = 1;
/// Cooperative yield.
pub const SYS_YIELD: u64 = 2;
/// Task exit.
pub const SYS_EXIT: u64 = 3;
/// Send one bounded IPC body.
pub const SYS_SEND: u64 = 4;
/// Receive one bounded IPC body.
pub const SYS_RECV: u64 = 5;
/// x86-64 register-frame diagnostic. Other targets may reject it as unsupported.
pub const SYS_REGCHECK: u64 = 6;
/// Read-only process/supervisor counters. Returns packed `(terminated << 32) | escalations`.
pub const SYS_PROCESS_INFO: u64 = 7;
/// Read an object from the namespace that started the program (ADR-207): `(name, name_len, buf,
/// buf_len)` in the first four argument registers (x0..x3, a0..a3, rdi/rsi/rdx/r10); returns the
/// object's full length (copied: at most `buf_len`), or `u64::MAX` when refused.
pub const SYS_FS_READ: u64 = 8;
/// Write bytes to a capability-bound filesystem object. Reserved until user-memory copying lands.
pub const SYS_FS_WRITE: u64 = 9;
/// List capability-visible filesystem objects. Reserved until user-memory copying lands.
pub const SYS_FS_LIST: u64 = 10;
/// Terminate a task owned by the caller's capability domain. Reserved until task handles are public.
pub const SYS_PROCESS_KILL: u64 = 11;

/// Write bytes to the console that started the program (ADR-204): pointer and length, bounded by
/// the run's output sink. Returns the bytes kept, or `u64::MAX` when refused.
pub const SYS_WRITE_CONSOLE: u64 = 12;
/// Show a frame in the program's own desktop window (ADR-215): a packed one-bit bitmap in the
/// program's data pages, its width and height. Returns 0, or `u64::MAX` when refused.
pub const SYS_PRESENT: u64 = 13;
/// Take the next input event the operator gave the program's window (ADR-216): 0 when none,
/// `appwin::KEY | byte` for a key, `appwin::FOCUS_LOST`, or `u64::MAX` when the program holds no
/// open window.
pub const SYS_POLL_INPUT: u64 = 14;
/// Nanoseconds since boot on the machine's monotonic clock (ADR-222): what a program paces frames
/// and animations by, instead of busy loops whose speed is the emulator's.
pub const SYS_CLOCK: u64 = 15;
/// Grow the program's writable memory to `top` (ADR-230): zeroed pages mapped contiguously above
/// what it holds, up to `progout::DATA_CEILING_PAGES` above its stack. `0` asks for the current
/// top; returns the top after the call, or `u64::MAX` when refused (memory never shrinks).
pub const SYS_BRK: u64 = 16;
/// Upper 32 bits of [`SYS_PROCESS_INFO`] response.
pub const PROCESS_INFO_TERMINATED_SHIFT: u32 = 32;

/// Pack bounded supervisor counters into the process-info syscall response.
pub fn pack_process_info(terminated: usize, escalations: usize) -> u64 {
    ((terminated as u32 as u64) << PROCESS_INFO_TERMINATED_SHIFT) | escalations as u32 as u64
}

/// Unpack a process-info response into `(terminated, escalations)`.
pub fn unpack_process_info(value: u64) -> (u32, u32) {
    (
        (value >> PROCESS_INFO_TERMINATED_SHIFT) as u32,
        value as u32,
    )
}

/// Shared syscall identity. Effects remain target-owned until the syscall surface is widened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syscall {
    Emit,
    Yield,
    Exit,
    Send,
    Recv,
    Regcheck,
    ProcessInfo,
    FsRead,
    FsWrite,
    FsList,
    ProcessKill,
    WriteConsole,
    Present,
    PollInput,
    Clock,
    Brk,
}

impl Syscall {
    /// Decode one ABI number. Unknown values are refused rather than assigned a future meaning.
    pub const fn decode(number: u64) -> Option<Self> {
        match number {
            SYS_EMIT => Some(Self::Emit),
            SYS_YIELD => Some(Self::Yield),
            SYS_EXIT => Some(Self::Exit),
            SYS_SEND => Some(Self::Send),
            SYS_RECV => Some(Self::Recv),
            SYS_REGCHECK => Some(Self::Regcheck),
            SYS_PROCESS_INFO => Some(Self::ProcessInfo),
            SYS_FS_READ => Some(Self::FsRead),
            SYS_FS_WRITE => Some(Self::FsWrite),
            SYS_FS_LIST => Some(Self::FsList),
            SYS_PROCESS_KILL => Some(Self::ProcessKill),
            SYS_WRITE_CONSOLE => Some(Self::WriteConsole),
            SYS_PRESENT => Some(Self::Present),
            SYS_POLL_INPUT => Some(Self::PollInput),
            SYS_CLOCK => Some(Self::Clock),
            SYS_BRK => Some(Self::Brk),
            _ => None,
        }
    }

    /// Return stable ABI number for this syscall.
    pub const fn number(self) -> u64 {
        match self {
            Self::Emit => SYS_EMIT,
            Self::Yield => SYS_YIELD,
            Self::Exit => SYS_EXIT,
            Self::Send => SYS_SEND,
            Self::Recv => SYS_RECV,
            Self::Regcheck => SYS_REGCHECK,
            Self::ProcessInfo => SYS_PROCESS_INFO,
            Self::FsRead => SYS_FS_READ,
            Self::FsWrite => SYS_FS_WRITE,
            Self::FsList => SYS_FS_LIST,
            Self::ProcessKill => SYS_PROCESS_KILL,
            Self::WriteConsole => SYS_WRITE_CONSOLE,
            Self::Present => SYS_PRESENT,
            Self::PollInput => SYS_POLL_INPUT,
            Self::Clock => SYS_CLOCK,
            Self::Brk => SYS_BRK,
        }
    }

    /// Capability action required for syscall effects. Scheduler mechanics and diagnostics have no
    /// object authority; service calls always name their policy action explicitly.
    pub const fn capability(self) -> Option<&'static str> {
        match self {
            Self::Emit => Some("event.emit"),
            Self::Send | Self::Recv => Some("ipc.msg"),
            Self::ProcessInfo => Some("process.inspect"),
            Self::FsRead => Some("fs.read"),
            Self::FsWrite => Some("fs.write"),
            Self::FsList => Some("fs.inspect"),
            Self::ProcessKill => Some("process.kill"),
            Self::WriteConsole => Some("console.output"),
            Self::Present | Self::PollInput => Some("window.present"),
            // Reading the clock touches no object: like yielding, it needs no authority. Growing
            // memory is bounded by the program's own ceiling, as its stack is (ADR-230).
            Self::Yield | Self::Exit | Self::Regcheck | Self::Clock | Self::Brk => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_numbers_round_trip() {
        for syscall in [
            Syscall::Emit,
            Syscall::Yield,
            Syscall::Exit,
            Syscall::Send,
            Syscall::Recv,
            Syscall::Regcheck,
            Syscall::ProcessInfo,
            Syscall::FsRead,
            Syscall::FsWrite,
            Syscall::FsList,
            Syscall::ProcessKill,
            Syscall::WriteConsole,
            Syscall::Present,
            Syscall::PollInput,
            Syscall::Clock,
            Syscall::Brk,
        ] {
            assert_eq!(Syscall::decode(syscall.number()), Some(syscall));
        }
    }

    #[test]
    fn unknown_numbers_fail_closed() {
        for number in [0, 17, 99, u64::MAX] {
            assert_eq!(Syscall::decode(number), None);
        }
    }

    #[test]
    fn service_capability_mapping_is_explicit() {
        assert_eq!(Syscall::ProcessInfo.capability(), Some("process.inspect"));
        assert_eq!(Syscall::FsRead.capability(), Some("fs.read"));
        assert_eq!(Syscall::FsWrite.capability(), Some("fs.write"));
        assert_eq!(Syscall::FsList.capability(), Some("fs.inspect"));
        assert_eq!(Syscall::ProcessKill.capability(), Some("process.kill"));
        assert_eq!(Syscall::WriteConsole.capability(), Some("console.output"));
        assert_eq!(Syscall::Yield.capability(), None);
    }

    #[test]
    fn process_info_pack_is_bounded_and_round_trips() {
        let packed = pack_process_info(17, 3);
        assert_eq!(unpack_process_info(packed), (17, 3));
    }
}
