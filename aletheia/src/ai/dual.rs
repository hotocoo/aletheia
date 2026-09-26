//! The console's two thinking systems, asked in order (ADR-186).
//!
//! System 1 is asked first because it is cheap: a handful of typed questions — which command, which
//! object, which part of the request is the text — each answered with a calibrated confidence. When
//! EVERY answer clears the manifest's threshold and every argument the command needs was resolved,
//! System 1's plan is the plan. Otherwise the request goes, untouched, to System 2 (the language
//! model), and the reason is recorded. Either way the plan is untrusted output: the same validator,
//! the same control-byte check and the same human approval sit after both (INV-014).
//!
//! Nothing here names a model. The command options come from `console_ops` (the kernel's own
//! table), the objects from the context brief, the numbers from the request; the threshold from the
//! System-1 manifest.

use std::cell::RefCell;

use super::decision::{Answer, DecisionProvider, Question};
use super::provider::{ModelError, ModelProvider};
use crate::console_ops::{self, ConsoleOp};
use crate::intent_action::{Intent, Plan, Step, Verb};

/// Which system produced the last plan, and why.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// System 1 answered; the lowest confidence among its answers.
    System1 { confidence: f32 },
    /// System 2 answered, because System 1 could not or would not (the reason is named).
    System2 { because: String },
}

/// Argument names whose values System 1 picks from the objects the brief lists.
const OBJECT_ARGS: &[&str] = &["name", "src", "dst"];
/// Argument names whose values are numbers the request states.
const NUMBER_ARGS: &[&str] = &["n", "port", "khz", "domain"];

/// The objects a context brief names, in order: both the inline form
/// (`objects on this machine: a (3 bytes), b (4 bytes)`) and `ls`'s own lines (`      30  a`).
pub fn objects_in(context: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(at) = context.find("objects on this machine:") else {
        return out;
    };
    let body = &context[at + "objects on this machine:".len()..];
    for piece in body.split([',', '\n']) {
        let words: Vec<&str> = piece.split_whitespace().collect();
        let name = match words.as_slice() {
            [] => continue,
            [n, rest @ ..] if n.chars().all(|c| c.is_ascii_digit()) && !rest.is_empty() => rest[0],
            [first, ..] => first,
        };
        if name.starts_with('(') {
            continue;
        }
        if !out.iter().any(|o| o == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// Words of the request that are numbers, in order.
fn numbers_in(request: &str) -> Vec<&str> {
    request
        .split_whitespace()
        .filter(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_digit()))
        .collect()
}

/// A request-derived choice: every suffix of the request, as candidate free text.
fn suffixes(request: &str) -> Vec<(String, String)> {
    let words: Vec<&str> = request.split_whitespace().collect();
    (1..words.len())
        .map(|i| (words[i..].join(" "), String::new()))
        .collect()
}

fn words_of(request: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = Vec::new();
    for w in request.split_whitespace() {
        if !v.iter().any(|(l, _)| l == w) {
            v.push((w.to_string(), String::new()));
        }
    }
    v
}

/// Where a caller is told which system answered.
type RouteSink = Box<dyn Fn(&Route)>;

/// The dual-process interpreter.
pub struct DualProcess {
    system1: Box<dyn DecisionProvider>,
    system2: Box<dyn ModelProvider>,
    threshold: f32,
    label: String,
    last: RefCell<Option<Route>>,
    on_route: Option<RouteSink>,
}

impl DualProcess {
    pub fn new(
        system1: Box<dyn DecisionProvider>,
        system2: Box<dyn ModelProvider>,
        threshold: f32,
    ) -> Self {
        DualProcess {
            label: format!("dual({} -> {})", system1.name(), system2.name()),
            system1,
            system2,
            threshold,
            last: RefCell::new(None),
            on_route: None,
        }
    }

    /// Be told which system answered each request — so a caller can SAY it, never leave it implied.
    pub fn on_route(mut self, f: impl Fn(&Route) + 'static) -> Self {
        self.on_route = Some(Box::new(f));
        self
    }

    fn record(&self, r: Route) {
        if let Some(f) = &self.on_route {
            f(&r);
        }
        *self.last.borrow_mut() = Some(r);
    }

    /// Which system produced the most recent plan.
    pub fn last_route(&self) -> Option<Route> {
        self.last.borrow().clone()
    }

    fn ask(&self, state: &str, q: Question) -> Result<Answer, String> {
        let mut a = self
            .system1
            .decide(state, std::slice::from_ref(&q))
            .map_err(|e| format!("system1 failed: {e:?}"))?;
        let a = a.pop().ok_or("system1 gave no answer")?;
        if a.confidence() < self.threshold {
            return Err(format!(
                "system1 unsure ({:.2} < {:.2})",
                a.confidence(),
                self.threshold
            ));
        }
        Ok(a)
    }

    fn pick(
        &self,
        state: &str,
        instructions: String,
        options: Vec<(String, String)>,
    ) -> Result<(String, f32), String> {
        if options.is_empty() {
            return Err("nothing to choose from".into());
        }
        match self.ask(
            state,
            Question::Choice {
                instructions,
                options,
            },
        )? {
            Answer::Choice { label, confidence } => Ok((label, confidence)),
            Answer::YesNo { .. } => Err("system1 answered the wrong kind of question".into()),
        }
    }

    /// System 1's whole attempt: a plan and its weakest confidence, or why it declines.
    pub fn system1_plan(&self, request: &str, context: &str) -> Result<(Plan, f32), String> {
        let ops = console_ops::all();
        let state = request;
        let options = ops
            .iter()
            .map(|o| (o.name.to_string(), o.doc.to_string()))
            .collect();
        let (verb, mut weakest) = self.pick(
            state,
            "Which console command does the operator's request ask for?".into(),
            options,
        )?;
        let op: &ConsoleOp = ops
            .iter()
            .find(|o| o.name == verb)
            .ok_or("system1 chose a command the table does not have")?;
        let objects = objects_in(context);
        let numbers = numbers_in(request);
        let mut next_number = 0usize;
        let mut args = serde_json::Map::new();
        for (i, arg) in op.args.iter().enumerate() {
            let required = i < op.required;
            let value = if OBJECT_ARGS.contains(&arg.as_str()) {
                let mut opts: Vec<(String, String)> =
                    objects.iter().map(|o| (o.clone(), String::new())).collect();
                for w in words_of(request) {
                    if !opts.iter().any(|(l, _)| *l == w.0) {
                        opts.push(w);
                    }
                }
                let (v, c) = self.pick(
                    state,
                    format!("Which object does the request name as the {arg}?"),
                    opts,
                )?;
                weakest = weakest.min(c);
                Some(v)
            } else if NUMBER_ARGS.contains(&arg.as_str()) {
                let v = numbers.get(next_number).map(|s| s.to_string());
                if v.is_some() {
                    next_number += 1;
                }
                v
            } else if arg == "text" {
                let opts = if op.is_free_form(i) {
                    suffixes(request)
                } else {
                    words_of(request)
                };
                let (v, c) = self.pick(
                    state,
                    "Which part of the request is the text the command needs?".into(),
                    opts,
                )?;
                weakest = weakest.min(c);
                Some(v)
            } else {
                // Addresses, pins, URLs: System 1 does not invent them, it hands the request on.
                return Err(format!("`{arg}` is not a typed decision"));
            };
            match value {
                Some(v) => {
                    args.insert(arg.clone(), serde_json::json!(v));
                }
                None if required => return Err(format!("no value for required `{arg}`")),
                None => break,
            }
        }
        Ok((
            Plan {
                steps: vec![Step {
                    op: op.name.to_string(),
                    args: serde_json::Value::Object(args),
                }],
            },
            weakest,
        ))
    }
}

impl ModelProvider for DualProcess {
    fn name(&self) -> &str {
        &self.label
    }
    fn healthy(&self) -> bool {
        self.system1.healthy() || self.system2.healthy()
    }
    fn interpret(&self, intent: &Intent) -> Result<String, ModelError> {
        self.interpret_with_context(intent, "")
    }
    fn interpret_with_context(&self, intent: &Intent, context: &str) -> Result<String, ModelError> {
        let text = match &intent.verb {
            Verb::Raw { text } => text.clone(),
            _ => return self.system2.interpret_with_context(intent, context),
        };
        match self.system1_plan(&text, context) {
            Ok((plan, confidence)) => {
                self.record(Route::System1 { confidence });
                serde_json::to_string(&plan).map_err(|_| ModelError::InvalidOutput)
            }
            Err(because) => {
                self.record(Route::System2 { because });
                self.system2.interpret_with_context(intent, context)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::console::{plan_lines, DeterministicConsole};

    /// A System 1 that answers from a script: for each question, the label whose text the
    /// instructions or options match, at a fixed confidence.
    struct Scripted {
        confidence: f32,
        answers: Vec<&'static str>,
        asked: RefCell<usize>,
    }
    impl DecisionProvider for Scripted {
        fn name(&self) -> &str {
            "scripted"
        }
        fn healthy(&self) -> bool {
            true
        }
        fn decide(&self, _s: &str, q: &[Question]) -> Result<Vec<Answer>, ModelError> {
            let i = *self.asked.borrow();
            *self.asked.borrow_mut() += 1;
            let want = self.answers.get(i).ok_or(ModelError::InvalidOutput)?;
            match &q[0] {
                Question::Choice { options, .. } => {
                    assert!(options.iter().any(|(l, _)| l == want), "{want} offered");
                    Ok(vec![Answer::Choice {
                        label: want.to_string(),
                        confidence: self.confidence,
                    }])
                }
                Question::YesNo { .. } => Err(ModelError::InvalidOutput),
            }
        }
    }
    fn s1(confidence: f32, answers: &[&'static str]) -> Scripted {
        Scripted {
            confidence,
            answers: answers.to_vec(),
            asked: RefCell::new(0),
        }
    }

    const BRIEF: &str = "  objects on this machine: manifesto (30 bytes), poem (12 bytes)\n";

    #[test]
    fn a_confident_system1_plans_alone() {
        let one = s1(0.97, &["head", "manifesto"]);
        let d = DualProcess::new(Box::new(one), Box::new(DeterministicConsole), 0.9);
        let (lines, _) = plan_lines(
            &d,
            "op",
            "show me the first 1 line of manifesto",
            BRIEF,
            false,
        )
        .unwrap();
        assert_eq!(lines, vec!["head manifesto 1"]);
        assert_eq!(d.last_route(), Some(Route::System1 { confidence: 0.97 }));
    }

    #[test]
    fn free_text_is_a_choice_over_the_request_itself() {
        let one = s1(0.95, &["write", "notes", "hello from the model"]);
        let d = DualProcess::new(Box::new(one), Box::new(DeterministicConsole), 0.9);
        let (lines, _) = plan_lines(
            &d,
            "op",
            "create an object called notes whose contents are hello from the model",
            BRIEF,
            true,
        )
        .unwrap();
        assert_eq!(lines, vec!["write notes hello from the model"]);
    }

    #[test]
    fn an_unsure_system1_hands_the_request_to_system2_and_says_why() {
        let one = s1(0.4, &["ls"]);
        let d = DualProcess::new(Box::new(one), Box::new(DeterministicConsole), 0.9);
        // The deterministic control arm stands in for System 2: it plans the literal form.
        let (lines, _) = plan_lines(&d, "op", "ls", BRIEF, false).unwrap();
        assert_eq!(lines, vec!["ls"]);
        match d.last_route() {
            Some(Route::System2 { because }) => assert!(because.contains("unsure"), "{because}"),
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn an_argument_system1_cannot_type_escalates() {
        // `tcp ADDR PORT TEXT`: an address is not a typed decision.
        let one = s1(0.99, &["tcp"]);
        let d = DualProcess::new(Box::new(one), Box::new(DeterministicConsole), 0.9);
        let _ = plan_lines(&d, "op", "tcp 10.0.2.2 80 hi", BRIEF, true);
        match d.last_route() {
            Some(Route::System2 { because }) => assert!(because.contains("addr"), "{because}"),
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn a_missing_required_number_escalates_and_an_optional_one_is_omitted() {
        let one = s1(0.99, &["head", "poem"]);
        let d = DualProcess::new(Box::new(one), Box::new(DeterministicConsole), 0.9);
        let (lines, _) = plan_lines(&d, "op", "top of poem", BRIEF, false).unwrap();
        assert_eq!(lines, vec!["head poem"], "optional N omitted");
        let one = s1(0.99, &["follow"]);
        let d = DualProcess::new(Box::new(one), Box::new(DeterministicConsole), 0.9);
        let _ = plan_lines(&d, "op", "follow the link", BRIEF, false);
        assert!(matches!(d.last_route(), Some(Route::System2 { .. })));
    }

    #[test]
    fn the_brief_parser_reads_both_forms() {
        assert_eq!(objects_in(BRIEF), vec!["manifesto", "poem"]);
        let ls = "  objects on this machine:\n          30  manifesto\n          12  poem\n";
        assert_eq!(objects_in(ls), vec!["manifesto", "poem"]);
        assert!(objects_in("  objects on this machine:\n    (no objects)\n").is_empty());
        assert!(objects_in("").is_empty());
    }
}
