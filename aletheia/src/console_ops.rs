//! The kernel console as a planning surface (REQ-AI-006, ADR-053).
//!
//! `tools.rs` registers the hosted Core's six entity operations. This registers a SECOND, disjoint
//! operation family: the commands a person types at the live kernel console. They are not two halves
//! of one surface — `entity.derive` and `grep` share no vocabulary, no arguments and no executor —
//! so they get two registries and one shape.
//!
//! **The list is not here.** It is `kernel_core::shell::COMMANDS`, the table the kernel's own
//! dispatcher and its `help` are generated from, and this module derives every operation from it.
//! Retyping the twenty-seven commands would have been three lines shorter and would have created the
//! exact defect `tools.rs` names out loud: a second list drifts, silently, and the drift presents as
//! the model proposing a command that does not exist — which reads as the model being wrong.
//!
//! What this module adds that the kernel table cannot know:
//!
//! * **Risk.** The console dispatcher does not classify its own commands; approval is a hosted-side
//!   policy question. The rule here is conservative and stated once: anything that writes to the
//!   medium, or stops the machine, is `Destructive`. Whether a particular write happens to be
//!   additive (`append`, `touch`) is not knowable before it runs, and a planner that guesses
//!   "additive" is a planner that eventually guesses wrong about somebody's bytes.
//! * **A rendering contract.** A validated step becomes EXACTLY ONE console line. That is the
//!   security property of this file: a model that emits `"beta\rrm notes"` as an argument cannot get
//!   a second command executed, because a control byte in any argument is a rejected plan, not a
//!   typed keystroke.

use crate::tools::Risk;
use kernel_core::shell::{COMMANDS, MAX_LINE};

/// One console command, as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleOp {
    /// The verb the dispatcher matches on (`grep`, `write`, …).
    pub name: &'static str,
    /// The kernel's own usage string, verbatim (`grep TEXT NAME`). Kept because it is the interface
    /// as the system states it, and a measurement showed that showing it to the model is worth
    /// several operations: a lowercased argument LIST is a description of the interface, and the
    /// usage line IS the interface.
    pub usage: &'static str,
    /// Argument names in the order the dispatcher parses them, lowercased from the kernel's own
    /// usage string (`grep TEXT NAME` → `["text", "name"]`).
    pub args: Vec<String>,
    /// How many leading arguments are required; the remainder are optional (`head NAME [N]` → 1).
    pub required: usize,
    /// What `help` says the command does — handed to the model verbatim, so the menu the model reads
    /// and the menu a human reads are the same sentence.
    pub doc: &'static str,
    pub risk: Risk,
}

impl ConsoleOp {
    /// True when `arg` may contain spaces: only a trailing free-form `text`, which the dispatcher
    /// takes as the rest of the line. Every other argument is read with `split_first`, so a space
    /// inside it would silently become the NEXT argument — `grep "no space" f` would search for `no`
    /// in an object called `space`, execute happily, and report a wrong answer as a right one.
    pub fn is_free_form(&self, index: usize) -> bool {
        index + 1 == self.args.len() && self.args[index] == "text"
    }
}

/// Every console operation, derived from the kernel's command table.
pub fn all() -> Vec<ConsoleOp> {
    COMMANDS
        .iter()
        .map(|(usage, doc)| parse(usage, doc))
        .collect()
}

/// Look one up by verb.
pub fn lookup(name: &str) -> Option<ConsoleOp> {
    all().into_iter().find(|c| c.name == name)
}

fn parse(usage: &'static str, doc: &'static str) -> ConsoleOp {
    let mut it = usage.split_whitespace();
    let name = it.next().unwrap_or(usage);
    let mut args = Vec::new();
    let mut required = 0usize;
    for tok in it {
        let optional = tok.starts_with('[');
        let cleaned = tok
            .trim_matches(|c| c == '[' || c == ']')
            .to_ascii_lowercase();
        if !optional {
            required += 1;
        }
        args.push(cleaned);
    }
    ConsoleOp {
        name,
        usage,
        args,
        required,
        doc,
        risk: risk_of(name),
    }
}

/// The classification rule, in one place: writing to the medium or stopping the machine is
/// destructive. Exhaustive on purpose — a command added to the kernel table and not named here
/// fails `every_command_is_classified`, rather than defaulting to `Safe` and being planned without
/// approval on the day someone adds `format`.
fn risk_of(name: &str) -> Risk {
    match name {
        "write" | "append" | "touch" | "cp" | "mv" | "rm" | "reboot" | "halt" => Risk::Destructive,
        "help" | "ver" | "arch" | "uptime" | "mem" | "faults" | "mlstat" | "lsblk" | "df"
        | "ls" | "find" | "stat" | "cat" | "head" | "wc" | "grep" | "hexdump" | "sync"
        | "history" | "echo" | "clear" | "input" => Risk::Safe,
        // Unknown to this file: fail closed. An unclassified command is refused by the validator
        // below rather than being planned as harmless.
        _ => Risk::Destructive,
    }
}

/// Why a proposed console step was refused. Every variant renders to a line an operator can act on;
/// none of them can be produced by a step that would have been safe to type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    UnknownCommand(String),
    MissingArgument { op: String, arg: String },
    UnknownArgument { op: String, arg: String },
    EmptyArgument { op: String, arg: String },
    ControlByte { op: String, arg: String },
    SpaceInWordArgument { op: String, arg: String },
    LineTooLong { op: String, len: usize },
    NotAnObject,
    Approval { op: String },
}

impl Refusal {
    /// Can a caller usefully hand this refusal back to whoever produced the step and ask again?
    ///
    /// The split is between MALFORMING and OVERREACHING, and it is the whole safety content of the
    /// agent loop's retry (ADR-054). Everything true here says "you wrote it wrong", the refusal
    /// text says how, and a second attempt is a different attempt. `Approval` is the one that is
    /// not: the step was well-formed and the authority was absent, so asking again changes nothing
    /// except how many times the model was invited to try to do it.
    pub fn is_recoverable(&self) -> bool {
        !matches!(self, Refusal::Approval { .. })
    }
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Refusal::UnknownCommand(op) => {
                write!(f, "no such console command: {}", op.escape_debug())
            }
            Refusal::MissingArgument { op, arg } => write!(f, "{op} needs {arg}"),
            Refusal::UnknownArgument { op, arg } => {
                write!(f, "{op} takes no argument called {}", arg.escape_debug())
            }
            Refusal::EmptyArgument { op, arg } => write!(f, "{op}: {arg} is empty"),
            Refusal::ControlByte { op, arg } => write!(
                f,
                "{op}: {arg} contains a control byte — one step is one line"
            ),
            Refusal::SpaceInWordArgument { op, arg } => {
                write!(f, "{op}: {arg} must be one word")
            }
            Refusal::LineTooLong { op, len } => {
                write!(
                    f,
                    "{op}: {len} bytes exceeds the console line bound of {MAX_LINE}"
                )
            }
            Refusal::NotAnObject => write!(f, "a step's args must be a JSON object"),
            Refusal::Approval { op } => {
                write!(f, "{op} changes the machine and was not approved")
            }
        }
    }
}

/// Validate ONE proposed step and render the console line it becomes.
///
/// `approved` is the operator's answer to the destructive-risk question, carried in rather than read
/// from anywhere: this function decides nothing about policy, it only refuses to render a
/// destructive line that policy did not authorize.
pub fn render(op: &str, args: &serde_json::Value, approved: bool) -> Result<String, Refusal> {
    let meta = lookup(op).ok_or_else(|| Refusal::UnknownCommand(op.to_string()))?;
    if matches!(meta.risk, Risk::Destructive) && !approved {
        return Err(Refusal::Approval { op: op.to_string() });
    }
    let obj = args.as_object().ok_or(Refusal::NotAnObject)?;
    for key in obj.keys() {
        if !meta.args.iter().any(|a| a == key) {
            return Err(Refusal::UnknownArgument {
                op: op.to_string(),
                arg: key.clone(),
            });
        }
    }
    let mut line = String::from(meta.name);
    for (i, arg) in meta.args.iter().enumerate() {
        let raw = obj.get(arg);
        let value = match raw {
            None | Some(serde_json::Value::Null) => {
                if i < meta.required {
                    return Err(Refusal::MissingArgument {
                        op: op.to_string(),
                        arg: arg.clone(),
                    });
                }
                continue;
            }
            // A number is what a model produces for `head NAME [N]`; rendering it through
            // `to_string` rather than demanding a string keeps a correct plan from being refused for
            // its JSON type.
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(other) => other.to_string(),
        };
        if value.is_empty() {
            if i < meta.required {
                return Err(Refusal::EmptyArgument {
                    op: op.to_string(),
                    arg: arg.clone(),
                });
            }
            continue;
        }
        if value.chars().any(|c| c.is_control() || c == '\u{7f}') {
            return Err(Refusal::ControlByte {
                op: op.to_string(),
                arg: arg.clone(),
            });
        }
        if value.contains(char::is_whitespace) && !meta.is_free_form(i) {
            return Err(Refusal::SpaceInWordArgument {
                op: op.to_string(),
                arg: arg.clone(),
            });
        }
        line.push(' ');
        line.push_str(&value);
    }
    if line.len() > MAX_LINE {
        return Err(Refusal::LineTooLong {
            op: op.to_string(),
            len: line.len(),
        });
    }
    Ok(line)
}

/// Render a whole plan: every step, in order, each one line. All-or-nothing — a plan with one bad
/// step types none of it, because a half-executed plan at a console is a state nobody described.
pub fn render_plan(
    plan: &crate::intent_action::Plan,
    approved: bool,
) -> Result<Vec<String>, Refusal> {
    plan.steps
        .iter()
        .map(|s| render(&s.op, &s.args, approved))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_registry_is_the_kernel_table() {
        let ops = all();
        assert_eq!(ops.len(), COMMANDS.len());
        for (usage, _) in COMMANDS {
            let verb = usage.split_whitespace().next().unwrap();
            assert!(ops.iter().any(|o| o.name == verb), "{verb} missing");
        }
    }

    #[test]
    fn usage_strings_become_argument_names() {
        let grep = lookup("grep").unwrap();
        assert_eq!(grep.args, vec!["text", "name"]);
        assert_eq!(grep.required, 2);
        let head = lookup("head").unwrap();
        assert_eq!(head.args, vec!["name", "n"]);
        assert_eq!(head.required, 1);
        assert!(lookup("help").unwrap().args.is_empty());
    }

    /// Every command in the kernel table is deliberately classified. The fallback arm of `risk_of`
    /// exists to fail closed, not to be used — if this fails, a command was added upstream and the
    /// classification was not.
    #[test]
    fn every_command_is_classified() {
        for (usage, _) in COMMANDS {
            let verb = usage.split_whitespace().next().unwrap();
            let named = matches!(
                verb,
                "write"
                    | "append"
                    | "touch"
                    | "cp"
                    | "mv"
                    | "rm"
                    | "reboot"
                    | "halt"
                    | "help"
                    | "ver"
                    | "arch"
                    | "uptime"
                    | "mem"
                    | "faults"
                    | "mlstat"
                    | "lsblk"
                    | "df"
                    | "ls"
                    | "find"
                    | "stat"
                    | "cat"
                    | "head"
                    | "wc"
                    | "grep"
                    | "hexdump"
                    | "sync"
                    | "history"
                    | "echo"
                    | "clear"
                    | "input"
            );
            assert!(named, "{verb} has no explicit risk classification");
        }
    }

    #[test]
    fn a_safe_step_renders_to_one_line() {
        let line = render("grep", &json!({"text": "beta", "name": "poem"}), false).unwrap();
        assert_eq!(line, "grep beta poem");
    }

    #[test]
    fn a_free_form_tail_may_hold_spaces() {
        let line = render(
            "write",
            &json!({"name": "manifesto", "text": "the OS you can sit in front of"}),
            true,
        )
        .unwrap();
        assert_eq!(line, "write manifesto the OS you can sit in front of");
    }

    /// The injection case. A carriage return inside an argument would end the line at the console
    /// and run whatever followed as a second command — with the authority of the first.
    #[test]
    fn a_control_byte_in_an_argument_is_refused_not_typed() {
        let e = render(
            "grep",
            &json!({"text": "beta\rrm notes", "name": "poem"}),
            false,
        )
        .unwrap_err();
        assert!(matches!(e, Refusal::ControlByte { .. }), "{e}");
        let e = render("cat", &json!({"name": "poem\x1b[2J"}), false).unwrap_err();
        assert!(matches!(e, Refusal::ControlByte { .. }), "{e}");
    }

    #[test]
    fn a_space_in_a_word_argument_is_refused_not_silently_resplit() {
        let e = render("grep", &json!({"text": "two words", "name": "poem"}), false).unwrap_err();
        assert!(matches!(e, Refusal::SpaceInWordArgument { .. }), "{e}");
    }

    #[test]
    fn a_destructive_command_needs_approval() {
        let e = render("rm", &json!({"name": "notes"}), false).unwrap_err();
        assert!(matches!(e, Refusal::Approval { .. }), "{e}");
        assert_eq!(
            render("rm", &json!({"name": "notes"}), true).unwrap(),
            "rm notes"
        );
    }

    #[test]
    fn an_optional_argument_may_be_absent_or_a_number() {
        assert_eq!(
            render("head", &json!({"name": "poem"}), false).unwrap(),
            "head poem"
        );
        assert_eq!(
            render("head", &json!({"name": "poem", "n": 3}), false).unwrap(),
            "head poem 3"
        );
    }

    #[test]
    fn a_missing_required_argument_is_refused() {
        let e = render("grep", &json!({"name": "poem"}), false).unwrap_err();
        assert!(matches!(e, Refusal::MissingArgument { .. }), "{e}");
    }

    #[test]
    fn an_invented_argument_is_refused() {
        let e = render("cat", &json!({"file": "poem"}), false).unwrap_err();
        assert!(matches!(e, Refusal::UnknownArgument { .. }), "{e}");
    }

    #[test]
    fn an_invented_command_is_refused() {
        let e = render("format", &json!({}), true).unwrap_err();
        assert!(matches!(e, Refusal::UnknownCommand(_)), "{e}");
    }

    #[test]
    fn a_line_longer_than_the_console_bound_is_refused() {
        let long = "x".repeat(MAX_LINE);
        let e = render("write", &json!({"name": "n", "text": long}), true).unwrap_err();
        assert!(matches!(e, Refusal::LineTooLong { .. }), "{e}");
    }
}
