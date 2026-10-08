//! The checkpoint itself: ModernBERT encoder (candle's), the two-layer head and the scorer, loaded
//! from a Laya checkpoint directory (`model.safetensors`, `rl_agent_config.json`, `encoder/`,
//! `tokenizer/`). Computed in f32 from the checkpoint's f16 weights, as the reference runtime does
//! on CPU and MPS; f16 compute was measured and rejected (ADR-241).

use crate::{answer, parse_question, sequence, temperature, type_name, Encode, Layout};
use candle_core::{DType, Device, IndexOp, Module, Tensor, D};
use candle_nn::{layer_norm, linear, Embedding, LayerNorm, LayerNormConfig, Linear, VarBuilder};
use candle_transformers::models::modernbert::{Config, ModernBert};
use serde_json::{json, Value};
use tokenizers::Tokenizer;

type R<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl Encode for Tokenizer {
    fn ids(&self, text: &str) -> Vec<u32> {
        self.encode(text, false)
            .map(|e| e.get_ids().to_vec())
            .unwrap_or_default()
    }
}

/// `torch.nn.TransformerEncoderLayer(norm_first=True)` with its default ReLU feed-forward.
struct HeadLayer {
    in_w: Tensor,
    in_b: Tensor,
    out: Linear,
    lin1: Linear,
    lin2: Linear,
    norm1: LayerNorm,
    norm2: LayerNorm,
    heads: usize,
}

impl HeadLayer {
    fn load(vb: VarBuilder, d: usize) -> candle_core::Result<Self> {
        let cfg = LayerNormConfig {
            eps: 1e-5,
            ..Default::default()
        };
        Ok(HeadLayer {
            in_w: vb.get((3 * d, d), "self_attn.in_proj_weight")?,
            in_b: vb.get(3 * d, "self_attn.in_proj_bias")?,
            out: linear(d, d, vb.pp("self_attn.out_proj"))?,
            lin1: linear(d, 4 * d, vb.pp("linear1"))?,
            lin2: linear(4 * d, d, vb.pp("linear2"))?,
            norm1: layer_norm(d, cfg, vb.pp("norm1"))?,
            norm2: layer_norm(d, cfg, vb.pp("norm2"))?,
            heads: d / 64,
        })
    }

    /// `pad`: (batch, 1, 1, len), 0 for a token and -inf for padding.
    fn forward(&self, x: &Tensor, pad: &Tensor) -> candle_core::Result<Tensor> {
        let (b, l, d) = x.dims3()?;
        let hd = d / self.heads;
        let y = self.norm1.forward(x)?;
        let qkv = y
            .broadcast_matmul(&self.in_w.t()?)?
            .broadcast_add(&self.in_b)?;
        let split = |i: usize| -> candle_core::Result<Tensor> {
            qkv.narrow(2, i * d, d)?
                .reshape((b, l, self.heads, hd))?
                .transpose(1, 2)?
                .contiguous()
        };
        let (q, k, v) = (split(0)?, split(1)?, split(2)?);
        let att = (q.matmul(&k.t()?)? / (hd as f64).sqrt())?.broadcast_add(pad)?;
        let att = candle_nn::ops::softmax_last_dim(&att)?;
        let o = att.matmul(&v)?.transpose(1, 2)?.reshape((b, l, d))?;
        let x = (x + self.out.forward(&o)?)?;
        let y = self.norm2.forward(&x)?;
        x + self.lin2.forward(&self.lin1.forward(&y)?.relu()?)?
    }
}

/// A loaded checkpoint.
pub struct Laya {
    tok: Tokenizer,
    sp: Layout,
    pad: u32,
    enc: ModernBert,
    head: Vec<HeadLayer>,
    type_emb: Embedding,
    sc_norm: LayerNorm,
    sc_1: Linear,
    sc_2: Linear,
    cfg: Value,
    dev: Device,
}

impl Laya {
    /// Load the checkpoint in `dir` onto `dev`. The weights are memory-mapped, so loading costs
    /// what reading the configs costs; the pages arrive as the first forward pass touches them.
    pub fn load(dir: &std::path::Path, dev: Device) -> R<Self> {
        let read = |p: &str| std::fs::read_to_string(dir.join(p)).map_err(|e| format!("{p}: {e}"));
        let tok = Tokenizer::from_file(dir.join("tokenizer/tokenizer.json")).map_err(err)?;
        let mut ev: Value = serde_json::from_str(&read("encoder/config.json")?).map_err(err)?;
        // Newer encoder configs nest the two RoPE bases under `rope_parameters`.
        if ev.get("global_rope_theta").is_none() {
            let rp = ev["rope_parameters"].clone();
            ev["global_rope_theta"] = rp["full_attention"]["rope_theta"].clone();
            ev["local_rope_theta"] = rp["sliding_attention"]["rope_theta"].clone();
        }
        let ecfg: Config = serde_json::from_value(ev).map_err(err)?;
        let cfg: Value = serde_json::from_str(&read("rl_agent_config.json")?).map_err(err)?;
        // SAFETY: the file is mapped read-only and not modified while the process runs; a
        // checkpoint is replaced by a new snapshot directory, never rewritten in place.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DType::F32, &dev)
        }
        .map_err(err)?;
        // candle's encoder names its tensors `model.*`; the checkpoint's are `encoder.*`.
        let enc = ModernBert::load(
            vb.clone().rename_f(|s| s.replacen("model.", "encoder.", 1)),
            &ecfg,
        )
        .map_err(err)?;
        let d = ecfg.hidden_size;
        let n_head = cfg["head_layers"].as_u64().unwrap_or(2) as usize;
        let head = (0..n_head)
            .map(|i| HeadLayer::load(vb.pp(format!("head.layers.{i}")), d))
            .collect::<candle_core::Result<Vec<_>>>()
            .map_err(err)?;
        let lcfg = LayerNormConfig {
            eps: 1e-5,
            ..Default::default()
        };
        let id = |t: &str| {
            tok.token_to_id(t)
                .ok_or(format!("the tokenizer has no {t}"))
        };
        Ok(Laya {
            sp: Layout {
                cls: id("[CLS]")?,
                sep: id("[SEP]")?,
                mask: id("[MASK]")?,
                max_len: cfg["max_len"].as_u64().unwrap_or(512) as usize,
                head_max: cfg["head_max_len"].as_u64().unwrap_or(192) as usize,
            },
            pad: id("[PAD]")?,
            type_emb: Embedding::new(vb.get((3, d), "type_emb.weight").map_err(err)?, d),
            sc_norm: layer_norm(d, lcfg, vb.pp("scorer.0")).map_err(err)?,
            sc_1: linear(d, d, vb.pp("scorer.1")).map_err(err)?,
            sc_2: linear(d, 1, vb.pp("scorer.3")).map_err(err)?,
            tok,
            enc,
            head,
            cfg,
            dev,
        })
    }

    /// One wire request, all its questions in one padded batch: one forward pass.
    pub fn decide(&self, req: &Value) -> R<Value> {
        let state = req["state"].as_str().unwrap_or("");
        let qs = req["questions"].as_array().ok_or("no questions")?;
        if qs.is_empty() {
            return Err("no questions".into());
        }
        let mut items = Vec::with_capacity(qs.len());
        for q in qs {
            let a = parse_question(q)?;
            let (ids, markers) = sequence(
                &self.tok,
                self.sp,
                state,
                type_name(a.qtype),
                &a.instructions,
                &a.options,
            );
            if markers.len() != a.options.len() {
                return Err("the options do not fit the question budget".into());
            }
            items.push((ids, markers, a));
        }
        self.forward(&items).map_err(err)
    }

    fn forward(
        &self,
        items: &[(Vec<u32>, Vec<usize>, crate::Asked)],
    ) -> candle_core::Result<Value> {
        let n = items.len();
        let l = items.iter().map(|i| i.0.len()).max().unwrap_or(1);
        let mut ids = vec![self.pad; n * l];
        let mut att = vec![0u32; n * l];
        for (r, it) in items.iter().enumerate() {
            ids[r * l..r * l + it.0.len()].copy_from_slice(&it.0);
            att[r * l..r * l + it.0.len()].fill(1);
        }
        let pad: Vec<f32> = att
            .iter()
            .map(|&a| if a == 1 { 0.0 } else { f32::NEG_INFINITY })
            .collect();
        let ids = Tensor::from_vec(ids, (n, l), &self.dev)?;
        let att = Tensor::from_vec(att, (n, l), &self.dev)?;
        let pad = Tensor::from_vec(pad, (n, 1, 1, l), &self.dev)?;
        let qt: Vec<u32> = items.iter().map(|i| i.2.qtype as u32).collect();
        let te = self
            .type_emb
            .forward(&Tensor::from_vec(qt, n, &self.dev)?)?;
        let mut h = self
            .enc
            .forward(&ids, &att)?
            .broadcast_add(&te.unsqueeze(1)?)?;
        for layer in &self.head {
            h = layer.forward(&h, &pad)?;
        }
        let mut answers = Vec::with_capacity(n);
        for (r, (_, markers, asked)) in items.iter().enumerate() {
            let k = markers.len();
            let at: Vec<u32> = markers.iter().map(|&m| m as u32).collect();
            let m = h
                .i(r)?
                .index_select(&Tensor::from_vec(at, k, &self.dev)?, 0)?;
            let m = self.sc_1.forward(&self.sc_norm.forward(&m)?)?.gelu_erf()?;
            let logits: Vec<f32> = self
                .sc_2
                .forward(&m)?
                .squeeze(D::Minus1)?
                .to_dtype(DType::F32)?
                .to_vec1()?;
            let t = temperature(&self.cfg, asked.qtype, k);
            answers.push(answer(&logits, t, asked.qtype, &asked.labels));
        }
        Ok(json!({ "answers": answers }))
    }
}
