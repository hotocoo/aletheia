//! Console planning, from the outside (REQ-AI-006, ADR-053).
//!
//! The unit tests in `console_ops` and `ai::console` check the pieces. This checks the properties a
//! reader of the ADR would want held against the crate's public surface, with no model, no server
//! and no VM — so it runs everywhere and fails for reasons that are about Aletheia rather than about
//! whatever was resident on the machine that day.

use aletheia::ai::console::{self, DeterministicConsole, CASES};
use aletheia::console_ops::{self, Refusal};
use aletheia::tools::Risk;
use serde_json::json;

/// The menu the model is shown is the table the kernel dispatches on. Not a copy of it.
#[test]
fn every_kernel_command_is_offered_and_nothing_else_is() {
    let tools = console::tool_definitions();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    for (usage, _doc) in kernel_core::shell::COMMANDS {
        let verb = usage.split_whitespace().next().unwrap();
        assert!(names.contains(&verb), "the model is not offered `{verb}`");
    }
    assert_eq!(names.len(), kernel_core::shell::COMMANDS.len());
}

/// The classification is exhaustive: everything that can destroy bytes or stop the machine is
/// destructive, and the read-only commands are not.
#[test]
fn risk_is_assigned_to_every_command_and_matches_the_rule() {
    for verb in [
        "write", "append", "touch", "cp", "mv", "rm", "reboot", "halt",
    ] {
        assert!(
            matches!(console_ops::lookup(verb).unwrap().risk, Risk::Destructive),
            "{verb} should require approval"
        );
    }
    for verb in [
        "ls", "cat", "grep", "wc", "head", "stat", "df", "arch", "uptime",
    ] {
        assert!(
            matches!(console_ops::lookup(verb).unwrap().risk, Risk::Safe),
            "{verb} should not require approval"
        );
    }
}

/// The security property of the whole wave: a model-supplied argument becomes part of one typed
/// line, and cannot become a second command. Every one of these would be a console injection if the
/// step were rendered.
#[test]
fn no_model_argument_can_become_a_second_console_line() {
    for poison in [
        "beta\rrm manifesto",
        "beta\nrm manifesto",
        "beta\u{1b}[2Jrm manifesto",
        "beta\u{0}rm",
        "beta\u{7f}",
    ] {
        let e = console_ops::render("grep", &json!({"text": poison, "name": "poem"}), true)
            .expect_err("a control byte must refuse the plan");
        assert!(
            matches!(e, Refusal::ControlByte { .. }),
            "{poison:?} -> {e}"
        );
    }
}

/// Approval is not advisory. A destructive command is refused before a line exists at all, so there
/// is nothing for a caller to type by accident.
#[test]
fn a_destructive_plan_is_refused_before_it_is_rendered() {
    for (op, args) in [
        ("rm", json!({"name": "manifesto"})),
        ("write", json!({"name": "manifesto", "text": "clobbered"})),
        ("mv", json!({"src": "a", "dst": "b"})),
        ("halt", json!({})),
    ] {
        assert!(
            matches!(
                console_ops::render(op, &args, false),
                Err(Refusal::Approval { .. })
            ),
            "{op} rendered without approval"
        );
        assert!(
            console_ops::render(op, &args, true).is_ok(),
            "{op} with approval"
        );
    }
}

/// A plan is all-or-nothing: one bad step types none of it.
#[test]
fn a_plan_with_one_bad_step_renders_nothing() {
    let plan: aletheia::intent_action::Plan = serde_json::from_value(json!({"steps": [
        {"op": "cat", "args": {"name": "manifesto"}},
        {"op": "cat", "args": {"name": "poem\rhalt"}}
    ]}))
    .unwrap();
    assert!(console_ops::render_plan(&plan, false).is_err());
}

/// The wire shape the model actually returns: a tool call whose arguments are a JSON *string*.
#[test]
fn a_tool_call_becomes_a_validated_console_line() {
    let msg = json!({"tool_calls": [
        {"function": {"name": "grep", "arguments": "{\"text\":\"front\",\"name\":\"manifesto\"}"}}
    ]});
    let raw = console::plan_from_tool_calls(&msg).unwrap();
    let plan: aletheia::intent_action::Plan = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        console_ops::render_plan(&plan, false).unwrap(),
        vec!["grep front manifesto"]
    );
}

/// A response with no tool call is an error naming a cause, never an empty plan that reads as a
/// model with nothing to say.
#[test]
fn a_response_without_a_tool_call_names_the_likely_cause() {
    let e = console::plan_lines(
        &NoToolCalls,
        "human:operator",
        "print the contents of manifesto",
        "",
        false,
    )
    .unwrap_err();
    assert!(e.contains("--jinja"), "{e}");
}

struct NoToolCalls;
impl aletheia::ai::provider::ModelProvider for NoToolCalls {
    fn name(&self) -> &str {
        "no-tool-calls"
    }
    fn healthy(&self) -> bool {
        true
    }
    fn interpret(
        &self,
        _i: &aletheia::intent_action::Intent,
    ) -> Result<String, aletheia::ai::provider::ModelError> {
        Err(aletheia::ai::provider::ModelError::InvalidOutput)
    }
}

/// The control arm is the oracle for everything downstream of interpretation, so it must be perfect
/// on the same cases the model arm is measured against — and it must be perfect with no model, no
/// server and no VM, which is what makes this gate runnable anywhere.
#[test]
fn the_control_arm_plans_every_benchmark_case_to_its_exact_line() {
    for case in CASES {
        let (lines, _) = console::plan_lines(
            &DeterministicConsole,
            "human:operator",
            case.literal,
            case.context,
            case.approved,
        )
        .unwrap_or_else(|e| panic!("{}: {e}", case.literal));
        assert_eq!(lines, vec![case.expect.to_string()], "{}", case.literal);
    }
}

/// Every case's declared expectation is a line the registry itself would accept — so a case cannot
/// be written that the validator would refuse, which would make the gate untestable in the one
/// direction that matters.
#[test]
fn every_case_expects_a_line_the_registry_can_produce() {
    for case in CASES {
        let verb = case.expect.split_whitespace().next().unwrap();
        assert!(
            console_ops::lookup(verb).is_some(),
            "{} is not a console command",
            verb
        );
        assert!(case.expect.len() <= kernel_core::shell::MAX_LINE);
        assert!(
            !case.console_says.is_empty(),
            "{} asserts nothing",
            case.expect
        );
    }
}

/// The prompt names no command. It cost 3/8 to learn that a prohibition mentioning `ls` is still a
/// mention of `ls`; this is what stops it being relearned by someone adding a helpful sentence.
#[test]
fn the_system_prompt_names_no_command() {
    let p = console::system_prompt().to_ascii_lowercase();
    for op in console_ops::all() {
        assert!(
            !p.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w == op.name),
            "the system prompt names `{}`",
            op.name
        );
    }
}
