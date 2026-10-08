//! Fused CPU kernels for the encoder's hot elementwise path (ADR-249).
//!
//! `docs/evidence/system1/native-profile-2026-10-08.md` found 40 % of a decision's CPU time in
//! candle's elementwise ops and its scalar `erf`, not in matrix multiplication. The encoder's GeGLU
//! (`gelu_erf(a) * b` over the two halves of a 5 248-wide projection) was the largest of them: three
//! passes and two intermediate tensors per layer. Here it is one pass, split across cores, with an
//! `erf` that the compiler can vectorize.

use candle_core::{CpuStorage, CustomOp1, Layout, Result, Shape, Tensor};
use rayon::prelude::*;

/// `erf` for `f32` (Abramowitz and Stegun 7.1.26, absolute error below 1.5e-7 before f32
/// rounding): branch-free apart from the sign, in `f32` throughout so the loop vectorizes.
#[inline(always)]
pub fn erf(x: f32) -> f32 {
    let a = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * a);
    let y = 1.0
        - (((((1.061_405_4 * t - 1.453_152) * t) + 1.421_413_8) * t - 0.284_496_74) * t
            + 0.254_829_6)
            * t
            * (-a * a).exp();
    y.copysign(x)
}

/// The exact (erf) GELU.
#[inline(always)]
pub fn gelu(x: f32) -> f32 {
    0.5 * x * (1.0 + erf(x * core::f32::consts::FRAC_1_SQRT_2))
}

struct GeGlu;

impl CustomOp1 for GeGlu {
    fn name(&self) -> &'static str {
        "aletheia-geglu"
    }

    fn cpu_fwd(&self, storage: &CpuStorage, layout: &Layout) -> Result<(CpuStorage, Shape)> {
        let CpuStorage::F32(data) = storage else {
            candle_core::bail!("geglu: f32 only");
        };
        let Some((start, end)) = layout.contiguous_offsets() else {
            candle_core::bail!("geglu: contiguous input only");
        };
        let dims = layout.shape().dims();
        let width = *dims.last().unwrap_or(&0);
        if !width.is_multiple_of(2) {
            candle_core::bail!("geglu: the last dimension must be even, got {width}");
        }
        let half = width / 2;
        let data = &data[start..end];
        let mut out = vec![0f32; data.len() / 2];
        out.par_chunks_mut(half)
            .zip(data.par_chunks(width))
            .for_each(|(o, row)| {
                let (a, b) = row.split_at(half);
                for ((o, &a), &b) in o.iter_mut().zip(a).zip(b) {
                    *o = gelu(a) * b;
                }
            });
        let mut shape = dims.to_vec();
        if let Some(last) = shape.last_mut() {
            *last = half;
        }
        Ok((CpuStorage::F32(out), Shape::from(shape)))
    }
}

/// `softmax(att * scale + mask)` over the last dimension in one parallel pass (ADR-249). `att` is
/// (batch, heads, L, L); `mask` is additive, (batch, 1, L, L), shared by every head.
pub fn masked_softmax(att: &Tensor, mask: &Tensor, scale: f32) -> Result<Tensor> {
    let (b, h, l, k) = att.dims4()?;
    let (mb, _, ml, mk) = mask.dims4()?;
    if (mb, ml, mk) != (b, l, k) {
        candle_core::bail!(
            "masked_softmax: mask {:?} does not fit {:?}",
            mask.dims(),
            att.dims()
        );
    }
    let mask: Vec<f32> = mask.contiguous()?.flatten_all()?.to_vec1()?;
    att.contiguous()?.apply_op1_no_bwd(&MaskedSoftmax {
        mask,
        heads: h,
        rows: l,
        cols: k,
        scale,
    })
}

struct MaskedSoftmax {
    mask: Vec<f32>,
    heads: usize,
    rows: usize,
    cols: usize,
    scale: f32,
}

impl CustomOp1 for MaskedSoftmax {
    fn name(&self) -> &'static str {
        "aletheia-masked-softmax"
    }

    fn cpu_fwd(&self, storage: &CpuStorage, layout: &Layout) -> Result<(CpuStorage, Shape)> {
        let CpuStorage::F32(data) = storage else {
            candle_core::bail!("masked_softmax: f32 only");
        };
        let Some((start, end)) = layout.contiguous_offsets() else {
            candle_core::bail!("masked_softmax: contiguous input only");
        };
        let data = &data[start..end];
        let (k, per_batch) = (self.cols, self.heads * self.rows);
        let mut out = vec![0f32; data.len()];
        out.par_chunks_mut(k)
            .zip(data.par_chunks(k))
            .enumerate()
            .for_each(|(r, (o, x))| {
                let batch = r / per_batch;
                let row = r % self.rows;
                let m = &self.mask[(batch * self.rows + row) * k..][..k];
                let mut max = f32::NEG_INFINITY;
                for (o, (&x, &m)) in o.iter_mut().zip(x.iter().zip(m)) {
                    *o = x * self.scale + m;
                    max = max.max(*o);
                }
                let mut sum = 0f32;
                for o in o.iter_mut() {
                    *o = (*o - max).exp();
                    sum += *o;
                }
                let inv = 1.0 / sum;
                for o in o.iter_mut() {
                    *o *= inv;
                }
            });
        Ok((CpuStorage::F32(out), layout.shape().clone()))
    }
}

/// `gelu_erf(first half) * second half` over the last dimension, in one parallel pass.
pub fn geglu(xs: &Tensor) -> Result<Tensor> {
    xs.contiguous()?.apply_op1_no_bwd(&GeGlu)
}

#[cfg(test)]
mod tests {
    use super::*;
    use candle_core::{Device, D};

    #[test]
    fn erf_is_within_1e_6_of_the_series_across_its_range() {
        let mut worst = 0f64;
        for i in -60_000..=60_000 {
            let x = i as f32 / 10_000.0;
            let d = (erf(x) as f64 - libm_erf(x as f64)).abs();
            worst = worst.max(d);
        }
        assert!(worst < 1e-6, "worst {worst}");
    }

    fn libm_erf(x: f64) -> f64 {
        // erf through erfc's continued relation is overkill here; std has no erf, so check against
        // a high-order series where it converges and the asymptote where it saturates.
        if x.abs() > 4.0 {
            return x.signum();
        }
        let mut sum = 0.0;
        let mut term = x;
        let mut n = 0.0;
        while term.abs() > 1e-17 {
            sum += term / (2.0 * n + 1.0);
            n += 1.0;
            term *= -x * x / n;
        }
        sum * 2.0 / core::f64::consts::PI.sqrt()
    }

    #[test]
    fn fused_masked_softmax_matches_candles_ops() {
        let d = Device::Cpu;
        let att = Tensor::randn(0f32, 3.0, (2, 4, 7, 7), &d).unwrap();
        let mut m = vec![0f32; 2 * 7 * 7];
        for (i, v) in m.iter_mut().enumerate() {
            if i % 5 == 3 {
                *v = f32::NEG_INFINITY;
            }
        }
        let mask = Tensor::from_vec(m, (2, 1, 7, 7), &d).unwrap();
        let fused = masked_softmax(&att, &mask, 0.125).unwrap();
        let reference = candle_nn::ops::softmax_last_dim(
            &(&att * 0.125).unwrap().broadcast_add(&mask).unwrap(),
        )
        .unwrap();
        let diff = (fused - reference)
            .unwrap()
            .abs()
            .unwrap()
            .flatten_all()
            .unwrap()
            .max(0)
            .unwrap()
            .to_scalar::<f32>()
            .unwrap();
        assert!(diff < 1e-6, "max difference {diff}");
    }

    #[test]
    fn fused_geglu_matches_candles_ops() {
        let d = Device::Cpu;
        let x = Tensor::randn(0f32, 2.0, (3, 5, 16), &d).unwrap();
        let fused = geglu(&x).unwrap();
        let halves = x.chunk(2, D::Minus1).unwrap();
        let reference = (halves[0].gelu_erf().unwrap() * &halves[1]).unwrap();
        let diff = (fused - reference)
            .unwrap()
            .abs()
            .unwrap()
            .flatten_all()
            .unwrap()
            .max(0)
            .unwrap()
            .to_scalar::<f32>()
            .unwrap();
        assert!(diff < 1e-5, "max difference {diff}");
    }
}
