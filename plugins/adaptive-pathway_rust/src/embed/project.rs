//! Wrap-add projection of an arbitrary-length embedding to a fixed
//! `dim`, ported from `embeddings.py::EmbeddingProvider._project`.
//! Models commonly emit 768/1024-dim vectors; folding (wrap-add) rather
//! than truncating keeps information from every source dimension.

pub fn project(raw: &[f32], dim: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; dim];
    if raw.len() == dim {
        out.copy_from_slice(raw);
    } else if raw.len() > dim {
        for (i, &v) in raw.iter().enumerate() {
            out[i % dim] += v;
        }
    } else {
        out[..raw.len()].copy_from_slice(raw);
    }
    let n: f32 = out.iter().map(|&x| x * x).sum::<f32>().sqrt();
    if n > 1e-10 {
        for x in out.iter_mut() {
            *x /= n;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_dim_passes_through() {
        let v = vec![1.0, 0.0];
        let p = project(&v, 2);
        assert_eq!(p.len(), 2);
        assert!((p[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn larger_dim_folds() {
        let raw: Vec<f32> = (1..=6).map(|i| i as f32).collect();
        let p = project(&raw, 3);
        assert_eq!(p.len(), 3);
        let n: f32 = p.iter().map(|&x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4);
    }

    #[test]
    fn smaller_dim_pads() {
        let raw = vec![1.0, 2.0];
        let p = project(&raw, 384);
        assert_eq!(p.len(), 384);
        // after normalization, nonzero only in first two entries
        assert!(p[0] > 0.0 && p[1] > 0.0);
        for x in p.iter().skip(2) {
            assert_eq!(*x, 0.0);
        }
        let n: f32 = p.iter().map(|&x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4);
        // ratio preserved: p[0]/p[1] == 1/2
        assert!((p[0] / p[1] - 0.5).abs() < 1e-4);
    }
}
