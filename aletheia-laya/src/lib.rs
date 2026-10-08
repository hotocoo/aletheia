//! The native System-1 runtime for Laya decision checkpoints (ADR-241).
//!
//! Laya is a non-autoregressive decision model: a ModernBERT encoder, a two-layer transformer head,
//! and a scorer that reads one hidden state per option. The reference runtime is Python (`pip
//! install laya`, torch). This crate runs the same checkpoint with candle and answers the Aletheia
//! decision wire (`POST /v1/decide`, ADR-186), so the backend ships as one binary with no
//! interpreter and no framework install. Parity with the reference is measured, not assumed
//! (`scripts/system1/native_parity.py`, numbers in ADR-241).
//!
//! The pieces that decide an answer without a model are separate and tested on their own:
//! [`sequence`] (how a question and its options become tokens, `laya.common.build_sequence`),
//! [`temperature`] and [`answer`] (calibration and confidence).

use serde_json::{json, Value};

/// The question types the checkpoint was trained on, in its embedding order.
pub const CHOICE: usize = 0;
pub const NOUL: usize = 2;

/// What [`sequence`] needs of a tokenizer.
pub trait Encode {
    /// Token ids for `text`, with no special tokens added.
    fn ids(&self, text: &str) -> Vec<u32>;
}

/// The special tokens a sequence is built from, and its two budgets: the whole sequence, and the
/// part before the state (instructions and options).
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub cls: u32,
    pub sep: u32,
    pub mask: u32,
    pub max_len: usize,
    pub head_max: usize,
}

/// `[CLS] <type> question: instructions [SEP] [MASK] opt0 [MASK] opt1 ... [SEP] state [SEP]`, cut to
/// `max_len`; returns the ids and the position of each option's `[MASK]`. Each option is at most
/// 48 tokens; when the options would leave the instructions fewer than 16 tokens of the
/// `head_max` budget they are cut evenly (never below 4), and the instructions keep at least 8.
pub fn sequence(
    tok: &impl Encode,
    sp: Layout,
    state: &str,
    qtype: &str,
    instructions: &str,
    options: &[String],
) -> (Vec<u32>, Vec<usize>) {
    let (max_len, head_max) = (sp.max_len, sp.head_max);
    let scrub = |s: &str| s.replace("[MASK]", " ");
    let mut head = tok.ids(&format!("{qtype} question: {}", scrub(instructions)));
    let mut opts: Vec<Vec<u32>> = options
        .iter()
        .map(|o| {
            let mut v = vec![sp.mask];
            let mut t = tok.ids(&format!(" {}", scrub(o)));
            t.truncate(48);
            v.extend(t);
            v
        })
        .collect();
    let used = |o: &[Vec<u32>]| o.iter().map(Vec::len).sum::<usize>() as i64;
    let mut budget = head_max as i64 - used(&opts);
    if budget < 16 {
        let per = 4.max((head_max as i64 - 16) / opts.len().max(1) as i64) as usize;
        for o in opts.iter_mut() {
            o.truncate(per);
        }
        budget = head_max as i64 - used(&opts);
    }
    head.truncate(8.max(budget.max(0)) as usize);
    let mut ids = vec![sp.cls];
    ids.extend(head);
    ids.push(sp.sep);
    let mut markers = Vec::with_capacity(opts.len());
    for o in opts {
        markers.push(ids.len());
        ids.extend(o);
    }
    ids.push(sp.sep);
    let room = max_len.saturating_sub(ids.len() + 1);
    let mut st = tok.ids(&scrub(state));
    st.truncate(room);
    ids.extend(st);
    ids.push(sp.sep);
    ids.truncate(max_len);
    markers.retain(|&m| m < max_len);
    (ids, markers)
}

/// The calibration temperature for a question of type `qtype` with `k` options: the checkpoint's
/// per-bucket value when it has one, else its per-type value, else 1.
pub fn temperature(cfg: &Value, qtype: usize, k: usize) -> f64 {
    let name = ["choice", "score", "noul"][qtype.min(2)];
    let size = match k {
        0..=2 => "2",
        3..=5 => "3-5",
        6..=10 => "6-10",
        _ => "11+",
    };
    cfg["temperature_by_options"][format!("{name}:{size}")]
        .as_f64()
        .or_else(|| cfg["temperature"][qtype].as_f64())
        .unwrap_or(1.0)
        .max(1e-3)
}

/// One wire answer from the scorer's logits: softmax at `temp`; for a choice the argmax label and
/// a confidence of 1 - H(p)/ln(k); for yes/no the `true` probability, confidence max(p, 1-p).
/// Confidences are rounded to four places, as the reference runtime reports them.
pub fn answer(logits: &[f32], temp: f64, qtype: usize, labels: &[String]) -> Value {
    let z: Vec<f64> = logits.iter().map(|&x| f64::from(x) / temp).collect();
    let mx = z.iter().copied().fold(f64::MIN, f64::max);
    let ex: Vec<f64> = z.iter().map(|v| (v - mx).exp()).collect();
    let sum: f64 = ex.iter().sum();
    let p: Vec<f64> = ex.iter().map(|v| v / sum).collect();
    let round = |v: f64| (v * 10_000.0).round() / 10_000.0;
    if qtype == CHOICE {
        let k = p.len();
        let best = (0..k).fold(0, |b, i| if p[i] > p[b] { i } else { b });
        let ent: f64 = -p.iter().map(|&v| v * v.clamp(1e-12, 1.0).ln()).sum::<f64>();
        let conf = if k < 2 {
            1.0
        } else {
            (1.0 - ent / (k as f64).ln()).clamp(0.0, 1.0)
        };
        json!({"label": labels[best], "confidence": round(conf)})
    } else {
        let t = p[1];
        json!({"yes": t >= 0.5, "confidence": round(t.max(1.0 - t))})
    }
}

/// A wire question, parsed: its type, instructions, the labels it answers with, and the option
/// texts the sequence carries (`label: description`).
pub struct Asked {
    pub qtype: usize,
    pub instructions: String,
    pub labels: Vec<String>,
    pub options: Vec<String>,
}

/// Parse one wire question. Refused by name: an unknown type, a choice with fewer than two options.
pub fn parse_question(q: &Value) -> Result<Asked, String> {
    let instructions = q["instructions"].as_str().unwrap_or("").to_string();
    match q["type"].as_str() {
        Some("choice") => {
            let o = q["options"].as_array().ok_or("a choice needs options")?;
            if o.len() < 2 {
                return Err("a choice needs at least two options".into());
            }
            let (labels, options) = o
                .iter()
                .map(|o| {
                    let l = o["label"].as_str().unwrap_or("").to_string();
                    let d = o["description"].as_str().unwrap_or("");
                    let text = if d.is_empty() {
                        l.clone()
                    } else {
                        format!("{l}: {d}")
                    };
                    (l, text)
                })
                .unzip();
            Ok(Asked {
                qtype: CHOICE,
                instructions,
                labels,
                options,
            })
        }
        Some("yesno") => Ok(Asked {
            qtype: NOUL,
            instructions,
            labels: vec!["false".into(), "true".into()],
            options: vec![
                "false: no, the statement does not hold".into(),
                "true: yes, the statement holds".into(),
            ],
        }),
        other => Err(format!("unknown question type {other:?}")),
    }
}

/// The wire name of a question type in the sequence (`yesno` is the checkpoint's `noul`).
pub fn type_name(qtype: usize) -> &'static str {
    if qtype == CHOICE {
        "choice"
    } else {
        "noul"
    }
}

pub mod encoder;
pub mod fused;
pub mod model;
pub mod wire;

#[cfg(test)]
mod tests {
    use super::*;

    /// One token per whitespace-separated word, numbered by length so ids are stable.
    struct Words;
    impl Encode for Words {
        fn ids(&self, text: &str) -> Vec<u32> {
            text.split_whitespace()
                .map(|w| 1000 + w.len() as u32)
                .collect()
        }
    }
    const SP: Layout = Layout {
        cls: 1,
        sep: 2,
        mask: 3,
        max_len: 512,
        head_max: 192,
    };

    #[test]
    fn a_sequence_marks_each_option_and_ends_with_the_state() {
        let opts = vec!["ls: list".to_string(), "rm: remove it".to_string()];
        let (ids, m) = sequence(&Words, SP, "show all", "choice", "which?", &opts);
        // [CLS] choice question: which? [SEP] [MASK] ls: list [MASK] rm: remove it [SEP] show all [SEP]
        assert_eq!(m, vec![5, 8]);
        assert_eq!(ids[0], SP.cls);
        assert_eq!(ids[4], SP.sep);
        assert_eq!((ids[5], ids[8]), (SP.mask, SP.mask));
        assert_eq!(ids[12], SP.sep);
        assert_eq!(ids.len(), 16);
        assert_eq!(*ids.last().unwrap(), SP.sep);
    }

    #[test]
    fn many_options_are_cut_evenly_and_the_instructions_keep_eight() {
        let long = "a b c d e f g h i j k l m n o p q r s t".to_string();
        let opts = vec![long; 60];
        let ins = "w ".repeat(100);
        let (ids, m) = sequence(&Words, SP, "", "choice", &ins, &opts);
        // 60 options: per = max(4, (192 - 16) / 60) = 4 tokens each, [MASK] included.
        assert_eq!(m.len(), 60);
        assert!(m.windows(2).all(|w| w[1] - w[0] == 4));
        // The instructions keep max(8, 192 - 240) = 8 tokens: [CLS] + 8 + [SEP] before the first option.
        assert_eq!(m[0], 10);
        assert!(ids.len() <= 512);
    }

    #[test]
    fn a_mask_token_written_by_the_operator_never_becomes_a_marker() {
        let opts = vec!["x".to_string(), "y [MASK] z".to_string()];
        let (_, m) = sequence(&Words, SP, "[MASK] [MASK]", "choice", "[MASK]?", &opts);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn the_state_is_cut_to_fit_and_the_sequence_never_exceeds_max_len() {
        let opts = vec!["a".to_string(), "b".to_string()];
        let state = "s ".repeat(1000);
        let (ids, m) = sequence(
            &Words,
            Layout { max_len: 64, ..SP },
            &state,
            "choice",
            "q",
            &opts,
        );
        assert_eq!(ids.len(), 64);
        assert_eq!(*ids.last().unwrap(), SP.sep);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn temperatures_prefer_the_bucket_then_the_type() {
        let cfg = json!({"temperature": [1.5, 1.2, 2.0], "temperature_by_options": {"choice:2": 0.05, "choice:11+": 2.6}});
        assert_eq!(temperature(&cfg, CHOICE, 2), 0.05);
        assert_eq!(temperature(&cfg, CHOICE, 47), 2.6);
        assert_eq!(temperature(&cfg, CHOICE, 4), 1.5);
        assert_eq!(temperature(&cfg, NOUL, 2), 2.0);
        assert_eq!(temperature(&json!({}), CHOICE, 3), 1.0);
    }

    #[test]
    fn answers_carry_the_argmax_and_an_entropy_confidence() {
        let l = vec!["a".to_string(), "b".to_string()];
        let a = answer(&[0.0, 10.0], 1.0, CHOICE, &l);
        assert_eq!(a["label"], "b");
        assert!(a["confidence"].as_f64().unwrap() > 0.99);
        let even = answer(&[1.0, 1.0], 1.0, CHOICE, &l);
        assert_eq!(even["confidence"].as_f64().unwrap(), 0.0);
        let yn = answer(&[0.0, 2.0], 1.0, NOUL, &l);
        assert_eq!(yn["yes"], true);
        assert_eq!(yn["confidence"].as_f64().unwrap(), 0.8808);
    }

    #[test]
    fn questions_are_parsed_or_refused_by_name() {
        let q = json!({"type": "choice", "instructions": "i", "options": [{"label": "ls", "description": "list"}, {"label": "rm"}]});
        let a = parse_question(&q).unwrap();
        assert_eq!(a.options, vec!["ls: list", "rm"]);
        assert_eq!(a.labels, vec!["ls", "rm"]);
        let one = json!({"type": "choice", "options": [{"label": "ls"}]});
        assert_eq!(
            parse_question(&one).err().unwrap(),
            "a choice needs at least two options"
        );
        assert!(parse_question(&json!({"type": "score"})).is_err());
        assert_eq!(
            parse_question(&json!({"type": "yesno"})).unwrap().qtype,
            NOUL
        );
    }
}
