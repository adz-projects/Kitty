//! In-memory vector store with amortized-doubling growth, ported from
//! `storage/vec.py::VectorIndex`. Search computes cosine similarity via the
//! precomputed norms; top-k uses a full desc sort, which is fine at the
//! belief-candidate scales this crate works with.

use super::ops;

const INITIAL_CAPACITY: usize = 16;

pub struct VectorIndex {
    ids: Vec<String>,
    embeddings: Vec<Vec<f32>>,
    norms: Vec<f32>,
    dim: usize,
    count: usize,
}

impl Default for VectorIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl VectorIndex {
    pub fn new() -> Self {
        Self {
            ids: Vec::with_capacity(INITIAL_CAPACITY),
            embeddings: Vec::with_capacity(INITIAL_CAPACITY),
            norms: Vec::with_capacity(INITIAL_CAPACITY),
            dim: 0,
            count: 0,
        }
    }

    pub fn build(&mut self, ids: Vec<String>, embeddings: Vec<Vec<f32>>) {
        self.ids = ids;
        self.embeddings = embeddings;
        self.norms = self
            .embeddings
            .iter()
            .map(|e| {
                let n = ops::norm(e);
                if n < 1e-12 {
                    1.0
                } else {
                    n
                }
            })
            .collect();
        self.count = self.ids.len();
        self.dim = self.embeddings.first().map(|e| e.len()).unwrap_or(0);
    }

    /// Top-k nearest ids to `query` by cosine, descending, as (id, score).
    /// Empty store or zero-norm query -> empty.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(String, f64)> {
        if self.count == 0 {
            return vec![];
        }
        let qn = ops::norm(query);
        if qn < 1e-12 {
            return vec![];
        }
        let scaled: Vec<f32> = query.iter().map(|&x| x / qn).collect();
        let mut scores: Vec<(usize, f64)> = Vec::with_capacity(self.count);
        for i in 0..self.count {
            let s = ops::dot(&self.embeddings[i], &scaled) as f64 / self.norms[i] as f64;
            scores.push((i, s));
        }
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores.truncate(k.min(self.count));
        scores
            .into_iter()
            .map(|(i, s)| (self.ids[i].clone(), s))
            .collect()
    }

    pub fn add(&mut self, id: String, embedding: Vec<f32>) {
        let dim = embedding.len();
        if self.count == 0 || self.dim != dim {
            // Dimensionality change or first add: cannot preserve a
            // mismatched matrix (the Python original reallocates with
            // capacity = 0 in this case, i.e. tracking nothing over).
            self.ids.clear();
            self.embeddings.clear();
            self.norms.clear();
            self.count = 0;
            self.dim = dim;
        }
        let n = ops::norm(&embedding);
        let n = if n < 1e-12 { 1.0 } else { n };
        self.ids.push(id);
        self.embeddings.push(embedding);
        self.norms.push(n);
        self.count += 1;
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f32, y: f32) -> Vec<f32> {
        vec![x, y]
    }

    #[test]
    fn empty_store_returns_empty() {
        let idx = VectorIndex::new();
        assert!(idx.search(&[1.0, 0.0], 5).is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn search_orders_by_cosine() {
        let mut idx = VectorIndex::new();
        idx.add("a".into(), v(1.0, 0.0));
        idx.add("b".into(), v(0.0, 1.0));
        idx.add("c".into(), v(0.9, 0.1));
        let res = idx.search(&v(1.0, 0.0), 5);
        assert_eq!(res[0].0, "a");
        assert_eq!(res[1].0, "c");
        assert_eq!(res[2].0, "b");
        assert!(res[1].1 > res[2].1);
    }

    #[test]
    fn zero_query_returns_empty() {
        let mut idx = VectorIndex::new();
        idx.add("a".into(), v(1.0, 0.0));
        assert!(idx.search(&[0.0, 0.0], 5).is_empty());
    }

    #[test]
    fn k_larger_than_count_returns_all() {
        let mut idx = VectorIndex::new();
        idx.add("a".into(), v(1.0, 0.0));
        idx.add("b".into(), v(0.0, 1.0));
        let res = idx.search(&v(1.0, 0.0), 10);
        assert_eq!(res.len(), 2);
    }
}
