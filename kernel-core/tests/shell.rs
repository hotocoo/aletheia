//! Host proof of the interactive console (REQ-CON-001, ADR-044).
//!
//! The live per-target suite (`shell::console_suite`) proves the console behaves on each CPU. These
//! tests attack it instead: the editor is the boundary a human — or anything pretending to be one —
//! reaches first, so the whole byte space is swept rather than sampled, and the dispatcher is fed
//! the inputs a careless user actually produces (a bare verb, a missing argument, a name that is all
//! whitespace, a paste far past the line bound).
use kernel_core::fs::{Filesystem, MAX_FILE_BYTES};
use kernel_core::shell::{
    self, Edit, LineEditor, Outcome, Session, ShellAction, ShellHost, COMMANDS, MAX_LINE,
};
use kernel_core::spine::{CapEngine, Constraints, Scope};
use kernel_core::storage::MemBlockDevice;

/// A host stand-in for a target's facts. Fixed values, so an assertion about output is about the
/// dispatcher's formatting rather than about the machine it ran on.
struct TestHost;

impl ShellHost for TestHost {
    fn arch(&self) -> &str {
        "test-host"
    }
    fn uptime_ns(&self) -> u64 {
        1_234_567
    }
    fn free_frames(&self) -> usize {
        900
    }
    fn total_frames(&self) -> usize {
        1024
    }
    fn privilege(&self) -> u64 {
        1
    }
    fn supervisor_terminated(&self) -> usize {
        3
    }
    fn supervisor_escalations(&self) -> usize {
        2
    }
    fn authorize(&self, _action: ShellAction) -> bool {
        true
    }
}

struct DenyWritesHost;

impl ShellHost for DenyWritesHost {
    fn arch(&self) -> &str {
        "test-host"
    }
    fn uptime_ns(&self) -> u64 {
        0
    }
    fn free_frames(&self) -> usize {
        1
    }
    fn total_frames(&self) -> usize {
        1
    }
    fn privilege(&self) -> u64 {
        1
    }
    fn authorize(&self, action: ShellAction) -> bool {
        !matches!(action, ShellAction::Write | ShellAction::Halt)
    }
}

fn device() -> MemBlockDevice {
    MemBlockDevice::new(kernel_core::fs::FILE_DATA_START + 64)
}

/// Run one command line against a freshly formatted namespace and return everything printed.
fn run(input: &str) -> String {
    let mut dev = device();
    Filesystem::format(&mut dev).unwrap();
    let mut fs = Filesystem::mount(&mut dev).unwrap();
    drive(input, &mut fs, &mut dev)
}

fn drive(input: &str, fs: &mut Filesystem, dev: &mut MemBlockDevice) -> String {
    let host = TestHost;
    let mut session = Session::new();
    let mut log = String::new();
    for b in input.bytes() {
        if session.feed(b, &host, fs, dev, &mut |s| log.push_str(s)) == Outcome::Halt {
            break;
        }
    }
    log
}

// ---------------------------------------------------------------------------------------------
// The editor: what may become part of a command
// ---------------------------------------------------------------------------------------------

#[test]
fn every_byte_outside_printable_ascii_is_refused_entry_to_the_line() {
    // The whole space, not a sample: the only bytes that may end up in a command are the printable
    // ones, and the only bytes that may END a line are CR and LF.
    for byte in 0u8..=255 {
        let mut ed = LineEditor::new();
        let edit = ed.feed(byte, &mut |_| {});
        match byte {
            b'\r' | b'\n' => {
                assert_eq!(edit, Edit::Line(String::new()), "byte {byte:#04x}");
                assert!(ed.is_empty());
            }
            0x03 => assert_eq!(edit, Edit::Cancelled),
            // Tab asks the SESSION to complete: the editor owns the line, the session owns the
            // namespace. Nothing enters the line here either.
            shell::TAB => {
                assert_eq!(edit, Edit::Complete);
                assert!(ed.is_empty());
            }
            0x20..=0x7e => {
                assert_eq!(edit, Edit::Pending);
                assert_eq!(ed.len(), 1, "printable byte {byte:#04x} must be admitted");
            }
            _ => {
                assert_eq!(edit, Edit::Pending);
                assert!(ed.is_empty(), "byte {byte:#04x} must not enter the line");
            }
        }
    }
}

#[test]
fn a_refused_byte_is_never_echoed_so_the_screen_matches_the_buffer() {
    for byte in 0u8..=255 {
        if matches!(
            byte,
            b'\r' | b'\n' | 0x03 | 0x08 | 0x15 | 0x7f | 0x20..=0x7e
        ) {
            continue;
        }
        let mut ed = LineEditor::new();
        let mut echoed = String::new();
        ed.feed(byte, &mut |s| echoed.push_str(s));
        assert!(echoed.is_empty(), "byte {byte:#04x} echoed {echoed:?}");
    }
}

#[test]
fn the_line_is_bounded_and_the_bound_holds_under_a_paste_far_past_it() {
    let mut ed = LineEditor::new();
    for _ in 0..(MAX_LINE * 100) {
        ed.feed(b'x', &mut |_| {});
    }
    assert_eq!(ed.len(), MAX_LINE);

    // And the truncated line is still a valid, runnable line — dropping the tail must not corrupt
    // what was already typed.
    let mut completed = None;
    if let Edit::Line(l) = ed.feed(b'\r', &mut |_| {}) {
        completed = Some(l);
    }
    let line = completed.expect("return completes even a truncated line");
    assert_eq!(line.len(), MAX_LINE);
    assert!(line.bytes().all(|b| b == b'x'));
}

#[test]
fn backspace_cannot_walk_past_the_start_of_the_line() {
    let mut ed = LineEditor::new();
    for _ in 0..50 {
        ed.feed(0x7f, &mut |_| {});
    }
    assert!(ed.is_empty());
    ed.feed(b'a', &mut |_| {});
    ed.feed(0x08, &mut |_| {});
    ed.feed(0x08, &mut |_| {});
    assert!(ed.is_empty());
}

#[test]
fn ctrl_u_kills_the_line_without_ending_it() {
    let mut ed = LineEditor::new();
    for b in b"halt" {
        ed.feed(*b, &mut |_| {});
    }
    assert_eq!(ed.feed(0x15, &mut |_| {}), Edit::Pending);
    assert!(ed.is_empty());
}

#[test]
fn ctrl_c_discards_whatever_was_typed_and_runs_nothing() {
    let log = run("write doomed contents\x03\r");
    assert!(log.contains("^C"));
    assert!(!log.contains("wrote"));
}

// ---------------------------------------------------------------------------------------------
// The dispatcher: every refusal is a printed reason, never a panic and never a silent no-op
// ---------------------------------------------------------------------------------------------

#[test]
fn an_unknown_command_is_refused_by_name_and_the_session_continues() {
    let log = run("nope\rhelp\r");
    assert!(log.contains("unknown command 'nope'"));
    assert!(log.contains("commands:"), "the session kept going");
}

#[test]
fn faults_reports_supervisor_counters_through_inspect_authority() {
    let log = run("faults\r");
    assert!(log.contains("3 user task(s) contained, 2 fault(s) escalated"));
}

#[test]
fn denied_write_is_refused_before_filesystem_io() {
    let host = DenyWritesHost;
    let mut dev = device();
    Filesystem::format(&mut dev).unwrap();
    let mut fs = Filesystem::mount(&mut dev).unwrap();
    let mut log = String::new();
    assert_eq!(
        shell::execute(
            "write secret value",
            &host,
            &mut fs,
            &mut dev,
            &[],
            &mut |s| log.push_str(s),
        ),
        Outcome::Continue
    );
    assert!(log.contains("permission denied: console.write"));
    assert_eq!(fs.list(&dev).unwrap().len(), 0);
}

#[test]
fn denied_halt_cannot_stop_session() {
    let host = DenyWritesHost;
    let mut dev = device();
    Filesystem::format(&mut dev).unwrap();
    let mut fs = Filesystem::mount(&mut dev).unwrap();
    let mut log = String::new();
    let outcome = shell::execute("halt", &host, &mut fs, &mut dev, &[], &mut |s| {
        log.push_str(s)
    });
    assert_eq!(outcome, Outcome::Continue);
    assert!(log.contains("permission denied: system.halt"));
    assert!(!log.contains("halting."));
}

#[test]
fn console_actions_use_real_capabilities_and_revoke_fails_closed() {
    let mut engine = CapEngine::new(0xCAFE, 0);
    let console = engine.mint(
        "human:console",
        "console.*",
        Scope::All,
        Constraints::none(),
    );
    let system = engine.mint("human:console", "system.*", Scope::All, Constraints::none());
    let offered = [console, system];

    assert!(shell::authorize_with_capabilities(
        &engine,
        &offered,
        ShellAction::Inspect
    ));
    assert!(shell::authorize_with_capabilities(
        &engine,
        &offered,
        ShellAction::Write
    ));
    assert!(shell::authorize_with_capabilities(
        &engine,
        &offered,
        ShellAction::Halt
    ));

    engine.revoke(console);
    assert!(!shell::authorize_with_capabilities(
        &engine,
        &offered,
        ShellAction::Inspect
    ));
    assert!(!shell::authorize_with_capabilities(
        &engine,
        &offered,
        ShellAction::Write
    ));
    assert!(shell::authorize_with_capabilities(
        &engine,
        &offered,
        ShellAction::Halt
    ));

    engine.revoke(system);
    assert!(!shell::authorize_with_capabilities(
        &engine,
        &offered,
        ShellAction::Halt
    ));
}

#[test]
fn a_command_that_needs_an_argument_says_so_rather_than_guessing() {
    for (verb, usage) in [
        ("cat", "usage: cat NAME"),
        ("stat", "usage: stat NAME"),
        ("rm", "usage: rm NAME"),
        ("write", "usage: write NAME TEXT"),
    ] {
        let log = run(&format!("{verb}\r"));
        assert!(log.contains(usage), "{verb} printed: {log}");
    }
}

#[test]
fn a_line_of_only_whitespace_does_nothing_at_all() {
    let log = run("    \r");
    assert!(!log.contains("unknown command"));
    // Two prompts: the first, and the one after the empty line.
    assert_eq!(log.matches(shell::PROMPT).count(), 2);
}

#[test]
fn every_listed_command_is_reachable_and_every_reachable_command_is_listed() {
    let help = run("help\r");
    for (name, doc) in COMMANDS {
        let verb = name.split_whitespace().next().unwrap();
        assert!(help.contains(verb), "help omits {verb}");
        assert!(help.contains(doc), "help omits the doc for {verb}");
        // Reachable: running it must not produce the unknown-command refusal.
        let log = run(&format!("{verb}\r"));
        assert!(
            !log.contains("unknown command"),
            "{verb} is listed but not accepted"
        );
    }
}

#[test]
fn a_verb_is_matched_exactly_not_by_prefix() {
    // `hel` is not `help`, and `catx` is not `cat` — a prefix match would make typos destructive.
    for verb in ["hel", "helpp", "catx", "rmm", "writeX"] {
        let log = run(&format!("{verb} something\r"));
        assert!(log.contains("unknown command"), "{verb} was matched");
    }
}

// ---------------------------------------------------------------------------------------------
// The namespace: the console drives the real filesystem, and its refusals are the fs's own
// ---------------------------------------------------------------------------------------------

#[test]
fn write_then_cat_returns_the_bytes_that_were_typed_including_interior_spaces() {
    // The separator between the name and the body is consumed; INTERIOR spacing is not.
    let log = run("write note  two  spaces\rcat note\r");
    assert!(log.contains("wrote 11 bytes to note"), "{log}");
    assert!(log.contains("two  spaces"));
}

#[test]
fn a_second_write_replaces_rather_than_duplicating_the_name() {
    let mut dev = device();
    Filesystem::format(&mut dev).unwrap();
    let mut fs = Filesystem::mount(&mut dev).unwrap();
    drive("write k first\r", &mut fs, &mut dev);
    drive("write k second\r", &mut fs, &mut dev);
    let listing = drive("ls\r", &mut fs, &mut dev);
    assert_eq!(listing.matches(" k").count(), 1, "duplicated: {listing}");
    assert_eq!(fs.read(&dev, "k").unwrap(), b"second");
}

#[test]
fn a_console_write_is_durable_across_a_remount_of_the_same_device() {
    let mut dev = device();
    Filesystem::format(&mut dev).unwrap();
    {
        let mut fs = Filesystem::mount(&mut dev).unwrap();
        drive("write persisted still here\r", &mut fs, &mut dev);
    }

    // A fresh mount is the closest a host test gets to a reboot: only committed state survives it.
    let fresh = Filesystem::mount(&mut dev).unwrap();
    assert_eq!(fresh.read(&dev, "persisted").unwrap(), b"still here");
}

#[test]
fn removing_an_object_erases_it_and_a_later_read_is_refused() {
    let mut dev = device();
    Filesystem::format(&mut dev).unwrap();
    let mut fs = Filesystem::mount(&mut dev).unwrap();
    drive("write gone secret\r", &mut fs, &mut dev);
    let start = fs.stat(&dev, "gone").unwrap().start;
    drive("rm gone\r", &mut fs, &mut dev);

    let log = drive("cat gone\r", &mut fs, &mut dev);
    assert!(log.contains("no such object"));

    // Erase on delete (ADR-033's storage twin) reaches the block the console released.
    let mut block = [0xffu8; kernel_core::storage::BLOCK_SIZE];
    kernel_core::storage::BlockDevice::read_block(&dev, start, &mut block).unwrap();
    assert!(
        block.iter().all(|&b| b == 0),
        "released block still holds data"
    );
}

#[test]
fn an_invalid_name_is_refused_with_the_reason_and_writes_nothing() {
    for name in ["bad/name", &"n".repeat(64)] {
        let mut dev = device();
        Filesystem::format(&mut dev).unwrap();
        let mut fs = Filesystem::mount(&mut dev).unwrap();
        let log = drive(&format!("write {name} x\r"), &mut fs, &mut dev);
        assert!(log.contains("bad name"), "{name} printed: {log}");
        assert!(
            fs.list(&dev).unwrap().is_empty(),
            "{name} created something"
        );
    }
}

#[test]
fn an_object_too_large_for_one_transaction_is_refused_not_truncated() {
    // The console cannot type this much, but the dispatcher is the thing under test, not the
    // keyboard: an oversized body must surface the fs's refusal rather than a partial write.
    let mut dev = device();
    Filesystem::format(&mut dev).unwrap();
    let mut fs = Filesystem::mount(&mut dev).unwrap();
    let body = "z".repeat(MAX_FILE_BYTES + 1);
    let mut log = String::new();
    let host = TestHost;
    shell::execute(
        &format!("write big {body}"),
        &host,
        &mut fs,
        &mut dev,
        &[],
        &mut |s| log.push_str(s),
    );
    assert!(
        log.contains("too large") || log.contains("no space"),
        "{log}"
    );
    assert!(
        fs.stat(&dev, "big").is_err(),
        "a refused write created the name"
    );
}

#[test]
fn cat_of_non_text_contents_reports_a_byte_count_instead_of_spraying_the_terminal() {
    let mut dev = device();
    Filesystem::format(&mut dev).unwrap();
    let mut fs = Filesystem::mount(&mut dev).unwrap();
    fs.create(&mut dev, "binary", &[0xff, 0xfe, 0x00, 0x01])
        .unwrap();
    let log = drive("cat binary\r", &mut fs, &mut dev);
    assert!(log.contains("<4 bytes, not text>"), "{log}");
}

#[test]
fn halt_is_the_only_command_that_ends_the_session() {
    let host = TestHost;
    let mut dev = device();
    Filesystem::format(&mut dev).unwrap();
    let mut fs = Filesystem::mount(&mut dev).unwrap();
    for (name, _) in COMMANDS {
        let verb = name.split_whitespace().next().unwrap();
        let outcome = shell::execute(verb, &host, &mut fs, &mut dev, &[], &mut |_| {});
        let expected = if verb == "halt" {
            Outcome::Halt
        } else {
            Outcome::Continue
        };
        assert_eq!(outcome, expected, "{verb}");
    }
}

// ---------------------------------------------------------------------------------------------
// The live suite the targets run must also pass on the host, so a per-target failure means the
// TARGET differs — not that the suite was never true anywhere.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_live_console_suite_passes_on_the_host_too() {
    let host = TestHost;
    let mut dev = device();
    let mut names = Vec::new();
    let n = shell::console_suite(&host, &mut dev, &mut |_, passed, name| {
        assert!(passed, "live invariant failed on the host: {name}");
        names.push(name.to_string());
    })
    .expect("the live console suite holds on the host");
    assert_eq!(n as usize, names.len());
    assert!(n >= 15, "the suite lost invariants: {n}");
}
