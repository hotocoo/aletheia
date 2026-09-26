//! System 1: typed decisions with calibrated confidence (ADR-186).
//!
//! A System-1 model does not write. It is handed a STATE (text) and typed QUESTIONS — pick one of
//! these options, is this statement true, which level of this scale — and answers each in one pass
//! with a probability it has been trained to report honestly. That makes it cheap enough to ask
//! first and honest enough to know when to stop: an answer below the manifest's confidence
//! threshold is not an answer, and the request goes to System 2.
//!
//! The wire is Aletheia's, not any model's: `POST /v1/decide` with `{state, questions}` returns
//! `{answers}`, and `GET /v1/models` names what is being served, so the same identity check that
//! guards System 2 (ADR-052, "a port is not a model") guards this one. Which model sits behind the
//! port, how it renders options into tokens, and what hardware it runs on are the sidecar's
//! business and the manifest's record; nothing here names a model.

use super::provider::ModelError;

/// One typed question.
#[derive(Debug, Clone, PartialEq)]
pub enum Question {
    /// Pick exactly one option. `(label, description)`; the label is what comes back.
    Choice {
        instructions: String,
        options: Vec<(String, String)>,
    },
    /// Is the statement true?
    YesNo { instructions: String },
}

/// One answer, with the model's calibrated confidence in it.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Choice { label: String, confidence: f32 },
    YesNo { yes: bool, confidence: f32 },
}

impl Answer {
    pub fn confidence(&self) -> f32 {
        match self {
            Answer::Choice { confidence, .. } | Answer::YesNo { confidence, .. } => *confidence,
        }
    }
}

/// A System-1 model, wherever it runs.
pub trait DecisionProvider {
    fn name(&self) -> &str;
    fn healthy(&self) -> bool;
    /// Answer every question about `state`, in order. An answer the provider could not give is an
    /// error for the whole call — a partial answer set would let a caller act on half a decision.
    fn decide(&self, state: &str, questions: &[Question]) -> Result<Vec<Answer>, ModelError>;
}

/// The wire form of a question list.
pub fn request_json(state: &str, questions: &[Question]) -> serde_json::Value {
    let qs: Vec<serde_json::Value> = questions
        .iter()
        .map(|q| match q {
            Question::Choice {
                instructions,
                options,
            } => serde_json::json!({
                "type": "choice",
                "instructions": instructions,
                "options": options
                    .iter()
                    .map(|(l, d)| serde_json::json!({"label": l, "description": d}))
                    .collect::<Vec<_>>(),
            }),
            Question::YesNo { instructions } => serde_json::json!({
                "type": "yesno",
                "instructions": instructions,
            }),
        })
        .collect();
    serde_json::json!({ "state": state, "questions": qs })
}

/// Parse a wire answer list against the questions that were asked. Anything that does not match —
/// a label that was not offered, a confidence outside [0, 1], a count that differs — is
/// `InvalidOutput`: the sidecar is untrusted exactly as a language model is.
pub fn parse_answers(body: &str, questions: &[Question]) -> Result<Vec<Answer>, ModelError> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|_| ModelError::InvalidOutput)?;
    let arr = v["answers"].as_array().ok_or(ModelError::InvalidOutput)?;
    if arr.len() != questions.len() {
        return Err(ModelError::InvalidOutput);
    }
    let mut out = Vec::with_capacity(arr.len());
    for (a, q) in arr.iter().zip(questions) {
        let confidence = a["confidence"].as_f64().ok_or(ModelError::InvalidOutput)? as f32;
        if !(0.0..=1.0).contains(&confidence) {
            return Err(ModelError::InvalidOutput);
        }
        out.push(match q {
            Question::Choice { options, .. } => {
                let label = a["label"].as_str().ok_or(ModelError::InvalidOutput)?;
                if !options.iter().any(|(l, _)| l == label) {
                    return Err(ModelError::InvalidOutput);
                }
                Answer::Choice {
                    label: label.to_string(),
                    confidence,
                }
            }
            Question::YesNo { .. } => Answer::YesNo {
                yes: a["yes"].as_bool().ok_or(ModelError::InvalidOutput)?,
                confidence,
            },
        });
    }
    Ok(out)
}

/// A System-1 sidecar on localhost, spoken to over the Aletheia decision wire.
pub struct HttpDecisionProvider {
    endpoint: String,
    serve_id: String,
    label: String,
    timeout_ms: u64,
}

/// A System-1 answer is one forward pass; a sidecar that has not answered in this long is not
/// serving System 1 at the speed that makes it worth asking first.
const DECIDE_TIMEOUT_MS: u64 = 5_000;

impl HttpDecisionProvider {
    pub fn new(endpoint: &str, serve_id: &str) -> Self {
        HttpDecisionProvider {
            endpoint: endpoint.to_string(),
            serve_id: serve_id.to_string(),
            label: format!("system1:{serve_id}"),
            timeout_ms: DECIDE_TIMEOUT_MS,
        }
    }
    pub fn from_entry(e: &super::registry::ModelEntry, endpoint: &str) -> Self {
        Self::new(endpoint, &e.serve_id)
    }
}

impl DecisionProvider for HttpDecisionProvider {
    fn name(&self) -> &str {
        &self.label
    }
    /// Healthy means: something answers AND it says it is serving the model that was selected.
    fn healthy(&self) -> bool {
        super::llama::serving_matches(&self.endpoint, &self.serve_id)
    }
    fn decide(&self, state: &str, questions: &[Question]) -> Result<Vec<Answer>, ModelError> {
        let (host, port) = super::llama::endpoint_host_port(&self.endpoint);
        let body = request_json(state, questions).to_string();
        let (status, resp) = super::llama::http(
            &host,
            port,
            "POST",
            "/v1/decide",
            Some(&body),
            self.timeout_ms,
        )
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => ModelError::Timeout,
            _ => ModelError::NotLoaded,
        })?;
        if status != 200 {
            return Err(ModelError::Runtime);
        }
        parse_answers(&resp, questions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qs() -> Vec<Question> {
        vec![
            Question::Choice {
                instructions: "which".into(),
                options: vec![("ls".into(), "list".into()), ("cat".into(), "print".into())],
            },
            Question::YesNo {
                instructions: "destructive?".into(),
            },
        ]
    }

    #[test]
    fn a_well_formed_answer_set_parses() {
        let a = parse_answers(
            r#"{"answers":[{"label":"cat","confidence":0.97},{"yes":false,"confidence":0.8}]}"#,
            &qs(),
        )
        .unwrap();
        assert_eq!(
            a[0],
            Answer::Choice {
                label: "cat".into(),
                confidence: 0.97
            }
        );
        assert_eq!(a[1].confidence(), 0.8);
    }

    #[test]
    fn an_answer_the_question_did_not_offer_is_refused() {
        for bad in [
            r#"{"answers":[{"label":"rm","confidence":0.99},{"yes":true,"confidence":0.9}]}"#,
            r#"{"answers":[{"label":"ls","confidence":1.5},{"yes":true,"confidence":0.9}]}"#,
            r#"{"answers":[{"label":"ls","confidence":0.9}]}"#,
            r#"{"answers":[{"label":"ls","confidence":0.9},{"confidence":0.9}]}"#,
            "not json",
        ] {
            assert_eq!(
                parse_answers(bad, &qs()),
                Err(ModelError::InvalidOutput),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_request_carries_labels_descriptions_and_state() {
        let v = request_json("hello", &qs());
        assert_eq!(v["state"], "hello");
        assert_eq!(v["questions"][0]["type"], "choice");
        assert_eq!(v["questions"][0]["options"][1]["label"], "cat");
        assert_eq!(v["questions"][1]["type"], "yesno");
    }

    #[test]
    fn nothing_listening_is_unhealthy() {
        let p = HttpDecisionProvider::new("http://127.0.0.1:59998", "anything");
        assert!(!p.healthy());
        assert_eq!(
            p.decide("x", &qs()).unwrap_err(),
            ModelError::NotLoaded,
            "a refused connection is a model that is not there"
        );
    }
}
