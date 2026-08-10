//! The operation-surface benchmark — does the resident model actually drive this OS? (ADR-052,
//! REQ-AI-005).
//!
//! Every claim this repository makes about its AI subsystem has so far been a claim about *shape*:
//! the provider is model-agnostic, the plan is validated, the model never executes. None of that
//! says whether the model that is actually resident can produce a usable plan for the operations
//! Aletheia offers. This module answers that, once per registered operation, and reports the answer
//! as a table an operator can read.
//!
//! **What is measured.** For each operation in `prompt::OPERATIONS`, one intent is put through the
//! SAME provider the pipeline uses, and the raw output goes through the SAME `parse_plan` +
//! `validate_plan` the pipeline uses. A row is `ok` only when the model returned a plan that parses,
//! validates, and whose first step is the operation the intent was about. Latency is wall-clock for
//! the whole interpretation.
//!
//! **The control arm.** The deterministic interpreter runs the identical set. It is the oracle: it
//! is correct by construction, so its row is what a perfect model would produce, and its latency is
//! the floor. A model run without its control arm is a number with nothing to be a number *against*.
//!
//! **The identity check, and why it comes first.** `endpoint` is a port, and any process can hold a
//! port. Before a single measurement is taken, the benchmark asks the backend what it is serving and
//! requires the answer to contain the selected manifest's `serve_id`. Without that, the most likely
//! outcome of running this on a developer machine is a table of another model's latencies published
//! under this model's name — a wrong number that looks exactly like a right one. A mismatch is a
//! refusal, not a warning.
//!
//! **What is NOT measured, said plainly.** This benchmark drives the *hosted Core's* operation
//! surface (`aletheia/`). It does not drive the kernel console (`kernel-core/src/shell.rs`), which
//! is a different operation family with a different vocabulary, its own benchmark
//! (`ai::console::bench`) and its own live gate (`scripts/console-ai-e2e.sh`).
//!
//! This paragraph used to end by saying there was *no path* from the model to the console. ADR-053
//! built one, so that sentence is now false and saying it here would be worse than saying nothing.
//! What remains true, and is the part worth keeping: the console dispatcher still runs in kernel
//! space, in a `no_std` crate, with **no inference engine underneath it**. Every model call happens
//! on the host; what reaches the guest is a validated line of ASCII, indistinguishable from one a
//! person typed. Two surfaces, two benchmarks, two gates — calling them one would be the kind of
//! claim `docs/MATURITY.md` exists to prevent.
use super::config::AiConfig;
use super::prompt;
use crate::domain::EntityType;
use crate::intelligence::DeterministicRuntime;
use crate::intent_action::{parse_plan, validate_plan, Intent, Verb};
use std::time::Instant;

/// How one operation went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpOutcome {
    /// A plan that parses, validates, and leads with the expected operation.
    Ok,
    /// A valid plan for a DIFFERENT operation — the model understood the schema but not the task.
    WrongOp(String),
    /// Output that did not parse or did not validate. Held as text because "what did it say" is the
    /// only useful thing to know about a model that failed this way.
    Invalid(String),
    /// The provider itself failed (server down, timeout).
    ProviderError(String),
}

impl OpOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, OpOutcome::Ok)
    }
    /// One short word for the table.
    pub fn verdict(&self) -> &'static str {
        match self {
            OpOutcome::Ok => "ok",
            OpOutcome::WrongOp(_) => "wrong-op",
            OpOutcome::Invalid(_) => "invalid",
            OpOutcome::ProviderError(_) => "error",
        }
    }
}

/// One operation, once, against one provider.
#[derive(Debug, Clone)]
pub struct BenchRow {
    pub op: &'static str,
    pub outcome: OpOutcome,
    pub elapsed_ms: u128,
}

/// A whole run: the model arm, the deterministic control arm, and what was being served.
#[derive(Debug, Clone)]
pub struct BenchReport {
    /// How the measured model is named — the registry id when it has one.
    pub model_label: String,
    pub endpoint: String,
    /// What the backend said it was serving. Empty when it could not be asked.
    pub served: Vec<String>,
    pub model: Vec<BenchRow>,
    pub control: Vec<BenchRow>,
}

impl BenchReport {
    pub fn passed(&self) -> usize {
        self.model.iter().filter(|r| r.outcome.is_ok()).count()
    }
    pub fn total(&self) -> usize {
        self.model.len()
    }
    /// Median rather than mean: one 40-second outlier on a cold cache would drag a mean into
    /// meaninglessness, and the question this answers is "what does a request usually cost".
    pub fn median_ms(rows: &[BenchRow]) -> u128 {
        if rows.is_empty() {
            return 0;
        }
        let mut v: Vec<u128> = rows.iter().map(|r| r.elapsed_ms).collect();
        v.sort_unstable();
        v[v.len() / 2]
    }
}

/// A representative intent per operation. These are the intents the pipeline itself would carry, so
/// what is measured is the real request shape and not a prompt written to flatter a model.
fn intent_for(op: &str) -> Option<Intent> {
    let subject = "human:owner".to_string();
    let verb = match op {
        "entity.read" => Verb::Read {
            id: "e-bench-1".into(),
        },
        "entity.derive" => Verb::Derive {
            source: "e-bench-1".into(),
            into_type: EntityType::Output,
            content: "derived for the benchmark".into(),
        },
        "world.traverse" => Verb::Traverse {
            from: "e-bench-1".into(),
            edge: "derived_from".into(),
        },
        "capability.grant" => Verb::Grant {
            subject: "agent:reviewer".into(),
            action: "entity.read".into(),
            scope_entities: vec!["e-bench-1".into()],
            approval: false,
        },
        "entity.restore_version" => Verb::RestoreVersion {
            chain: "e-bench-1".into(),
            version: 1,
        },
        "entity.delete" => Verb::Delete {
            id: "e-bench-1".into(),
        },
        _ => return None,
    };
    Some(Intent { subject, verb })
}

/// Run one operation against one provider and judge the result exactly as the pipeline would.
fn run_op(provider: &dyn super::provider::ModelProvider, op: &'static str) -> BenchRow {
    let Some(intent) = intent_for(op) else {
        return BenchRow {
            op,
            outcome: OpOutcome::Invalid("no intent is defined for this operation".into()),
            elapsed_ms: 0,
        };
    };
    let started = Instant::now();
    let raw = provider.interpret(&intent);
    let elapsed_ms = started.elapsed().as_millis();
    let outcome = match raw {
        Err(e) => OpOutcome::ProviderError(format!("{e:?}")),
        Ok(raw) => match parse_plan(&raw) {
            Err(e) => OpOutcome::Invalid(format!("{e:?}")),
            Ok(plan) => match validate_plan(&plan) {
                Err(e) => OpOutcome::Invalid(format!("{e:?}")),
                Ok(()) => {
                    let first = plan.steps[0].op.clone();
                    if first == op {
                        OpOutcome::Ok
                    } else {
                        OpOutcome::WrongOp(first)
                    }
                }
            },
        },
    };
    BenchRow {
        op,
        outcome,
        elapsed_ms,
    }
}

/// Run the whole surface against the configured model, plus the deterministic control arm.
///
/// Returns `Err` — and takes NO measurement — when the configured provider is not a real model, or
/// when the backend is serving something other than the selected model. Both refusals exist for the
/// same reason: a benchmark that quietly measures the fallback interpreter, or someone else's model,
/// produces numbers that are worse than no numbers because they look like evidence.
pub fn run(cfg: &AiConfig) -> Result<BenchReport, String> {
    if !cfg.wants_local_model() {
        return Err(format!(
            "AI_PROVIDER={} / MODEL_BACKEND={} is not a model backend — there is nothing to benchmark",
            cfg.provider, cfg.backend
        ));
    }
    let served = super::llama::served_models(&cfg.endpoint).unwrap_or_default();
    match cfg.entry.as_ref().map(|e| e.serve_id.clone()) {
        None => return Err(
            "the configured model is not a registry entry (MODEL_REF was set), so its identity \
                 cannot be verified — unset MODEL_REF, or run `model use <id>`"
                .into(),
        ),
        Some(want) => {
            if !super::llama::serving_matches(&cfg.endpoint, &want) {
                return Err(format!(
                    "the backend at {} is not serving `{}` (it reports: {}) — start the selected \
                     model, or point MODEL_ENDPOINT at the server that has it",
                    cfg.endpoint,
                    want,
                    if served.is_empty() {
                        "nothing".to_string()
                    } else {
                        served.join(", ")
                    }
                ));
            }
        }
    }

    let provider = super::select_provider(cfg);
    if !provider.healthy() {
        return Err(format!(
            "the model backend at {} is not healthy — nothing was measured",
            cfg.endpoint
        ));
    }
    let control = DeterministicRuntime;
    let mut model_rows = Vec::new();
    let mut control_rows = Vec::new();
    for op in prompt::OPERATIONS {
        model_rows.push(run_op(provider.as_ref(), op));
        control_rows.push(run_op(&control, op));
    }
    Ok(BenchReport {
        model_label: cfg.label(),
        endpoint: cfg.endpoint.clone(),
        served,
        model: model_rows,
        control: control_rows,
    })
}

/// Render a report as the operator sees it: one line per operation, then the two summaries.
pub fn render(r: &BenchReport) -> String {
    let mut s = String::new();
    s.push_str(&format!("model:    {}\n", r.model_label));
    s.push_str(&format!("endpoint: {}\n", r.endpoint));
    s.push_str(&format!("serving:  {}\n\n", r.served.join(", ")));
    s.push_str(&format!(
        "{:<24} {:>10} {:>12}   {}\n",
        "operation", "model ms", "control ms", "verdict"
    ));
    for (m, c) in r.model.iter().zip(r.control.iter()) {
        s.push_str(&format!(
            "{:<24} {:>10} {:>12}   {}\n",
            m.op,
            m.elapsed_ms,
            c.elapsed_ms,
            match &m.outcome {
                OpOutcome::WrongOp(other) => format!("wrong-op (planned {other})"),
                OpOutcome::Invalid(why) => format!("invalid: {why}"),
                OpOutcome::ProviderError(why) => format!("error: {why}"),
                OpOutcome::Ok => "ok".to_string(),
            }
        ));
    }
    s.push_str(&format!(
        "\n{}/{} operations planned correctly; median {} ms (control {} ms)\n",
        r.passed(),
        r.total(),
        BenchReport::median_ms(&r.model),
        BenchReport::median_ms(&r.control)
    ));
    s.push_str(
        "this measures the hosted Core's operation surface. The kernel console is a separate\n\
         command family with its own benchmark (`aletheiad console bench`, ADR-053); it still has\n\
         no inference engine under it — the model plans on the host and Aletheia types the line.\n",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The control arm must score a clean sweep. If it does not, the harness is wrong — and every
    /// model number it produces is measured against a broken ruler.
    #[test]
    fn the_deterministic_control_arm_plans_every_operation_correctly() {
        let control = DeterministicRuntime;
        for op in prompt::OPERATIONS {
            let row = run_op(&control, op);
            assert!(
                row.outcome.is_ok(),
                "control arm failed {op}: {:?}",
                row.outcome
            );
            assert_eq!(row.outcome.verdict(), "ok");
        }
    }

    /// Every registered operation has an intent. A missing one would show up as a permanent
    /// `invalid` row that no model could ever pass, which reads as a model defect and is not one.
    #[test]
    fn every_registered_operation_has_a_benchmark_intent() {
        for op in prompt::OPERATIONS {
            assert!(intent_for(op).is_some(), "no benchmark intent for {op}");
        }
    }

    #[test]
    fn benchmarking_the_deterministic_provider_is_refused() {
        let cfg = AiConfig {
            provider: "deterministic".into(),
            ..AiConfig::default()
        };
        let err = run(&cfg).unwrap_err();
        assert!(err.contains("nothing to benchmark"));
    }

    /// Nothing is listening on this port, so identity cannot be established — and the refusal must
    /// come from the identity check, BEFORE any timing is recorded.
    #[test]
    fn an_unverifiable_backend_is_refused_before_anything_is_measured() {
        let cfg = AiConfig {
            endpoint: "http://127.0.0.1:59998".into(),
            ..AiConfig::default()
        };
        let err = run(&cfg).unwrap_err();
        assert!(err.contains("is not serving"), "unexpected refusal: {err}");
    }

    /// An explicit `MODEL_REF` detaches the config from the registry, and an unidentifiable model
    /// must not be benchmarked under a registry name.
    #[test]
    fn an_unregistered_model_cannot_be_benchmarked() {
        let cfg = AiConfig {
            entry: None,
            ..AiConfig::default()
        };
        let err = run(&cfg).unwrap_err();
        assert!(err.contains("not a registry entry"));
    }

    #[test]
    fn the_median_is_the_middle_and_an_empty_run_is_zero() {
        let row = |ms| BenchRow {
            op: "entity.read",
            outcome: OpOutcome::Ok,
            elapsed_ms: ms,
        };
        assert_eq!(
            BenchReport::median_ms(&[row(30), row(10), row(20)]),
            20,
            "median must not be perturbed by order"
        );
        assert_eq!(BenchReport::median_ms(&[]), 0);
    }
}
