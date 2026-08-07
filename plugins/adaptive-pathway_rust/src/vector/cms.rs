//! Count-based novelty estimator, ported from `decision/novelty.py::CountBasedNovelty`.
//! A small count-min-sketch over token hashes of the context embedding gives
//! a fatigue signal: the more a context has been visited, the lower the
//! novelty bonus (so the engine reaches for less-trodden topics).

use crate::config::NoveltyConfig;

/// Deterministic jump/double hashing on the embedding's raw bytes, matching
/// mmh3.hash(emb_bytes, seed=table_idx*7919+7) % hash_size. mmh3 returns a
/// signed int; Python's `%` is Euclidean (non-negative), so use `rem_euclid`.
fn hash_embedding(emb: &[f32], table_idx: usize, hash_size: usize) -> usize {
    let h = crate::embed::hashing::mmh3_32(&bytes_of(emb), (table_idx as i32) * 7919 + 7);
    h.rem_euclid(hash_size as i64) as usize
}

fn bytes_of(v: &[f32]) -> Vec<u8> {
    // SAFETY: f32 has no invalid bit patterns; reinterpret as bytes.
    let p = v.as_ptr() as *const u8;
    unsafe { std::slice::from_raw_parts(p, std::mem::size_of_val(v)) }.to_vec()
}

pub struct CountBasedNovelty {
    cfg: NoveltyConfig,
    counts: Vec<Vec<u32>>,
    action_counts: std::collections::HashMap<String, u32>,
    user_exploration_score: f64,
    user_exploration_weight: f64,
    agent_multiplier: f64,
    domain_lambdas: std::collections::HashMap<String, f64>,
}

impl CountBasedNovelty {
    pub fn new(cfg: NoveltyConfig) -> Self {
        let counts = vec![vec![0u32; cfg.hash_size]; cfg.n_hash_tables];
        Self {
            user_exploration_weight: 0.1,
            agent_multiplier: 0.5,
            cfg,
            counts,
            action_counts: std::collections::HashMap::new(),
            user_exploration_score: 0.0,
            domain_lambdas: std::collections::HashMap::new(),
        }
    }

    fn table_hashes(&self, ctx: &[f32]) -> Vec<usize> {
        let mut hs = Vec::with_capacity(self.cfg.n_hash_tables);
        for t in 0..self.cfg.n_hash_tables {
            hs.push(hash_embedding(ctx, t, self.cfg.hash_size));
        }
        hs
    }

    pub fn bonus(&self, ctx: &[f32], lambda_override: Option<f64>) -> f64 {
        let lam = self.effective_lambda(lambda_override);
        let hs = self.table_hashes(ctx);
        let counts = self.counts_for(&hs);
        let min_count = self.aggregate(&counts);
        lam / (1.0 + min_count as f64)
    }

    fn effective_lambda(&self, lambda_override: Option<f64>) -> f64 {
        let lam = lambda_override.unwrap_or(self.cfg.default_lambda);
        lam.max(self.cfg.lambda_floor)
    }

    fn counts_for(&self, hs: &[usize]) -> Vec<u32> {
        (0..self.cfg.n_hash_tables)
            .map(|t| self.counts[t][hs[t]])
            .collect()
    }

    fn aggregate(&self, counts: &[u32]) -> u32 {
        if self.cfg.min_count_pessimistic {
            *counts.iter().min().unwrap_or(&0)
        } else {
            let total: u64 = counts.iter().map(|&c| c as u64).sum();
            (total / self.cfg.n_hash_tables as u64) as u32
        }
    }

    pub fn action_bonus(&self, action_id: &str, lambda_override: Option<f64>) -> f64 {
        let lam = self.effective_lambda(lambda_override);
        let c = self.action_counts.get(action_id).copied().unwrap_or(0);
        self.cfg.ucb_multiplier * lam / (1.0 + c as f64)
    }

    pub fn current_score(&self, ctx: &[f32]) -> f64 {
        let hs = self.table_hashes(ctx);
        let counts = self.counts_for(&hs);
        1.0 / (1.0 + self.aggregate(&counts) as f64)
    }

    pub fn visit(&mut self, ctx: &[f32]) {
        let hs = self.table_hashes(ctx);
        for (t, &bucket) in hs.iter().enumerate() {
            self.counts[t][bucket] += 1;
        }
    }

    pub fn visit_action(&mut self, action_id: &str) {
        let e = self.action_counts.entry(action_id.to_string()).or_insert(0);
        *e += 1;
    }

    pub fn visit_count(&self, ctx: &[f32]) -> u32 {
        let hs = self.table_hashes(ctx);
        let counts = self.counts_for(&hs);
        self.aggregate(&counts)
    }

    pub fn action_count(&self, action_id: &str) -> u32 {
        self.action_counts.get(action_id).copied().unwrap_or(0)
    }

    pub fn record_user_action(&mut self) {
        self.user_exploration_score = (1.0 - self.user_exploration_weight)
            * self.user_exploration_score
            + self.user_exploration_weight * 1.0;
    }

    pub fn user_exploration_active(&self) -> bool {
        self.user_exploration_score > 0.5
    }

    pub fn user_exploration_score(&self) -> f64 {
        self.user_exploration_score
    }

    pub fn get_lambda_for_mode(&self, mode: &str) -> f64 {
        if mode == "agent" {
            (self.cfg.default_lambda * self.agent_multiplier).max(self.cfg.lambda_floor)
        } else {
            self.cfg.default_lambda
        }
    }

    pub fn get_lambda_for_domain(&self, domain_id: &str) -> f64 {
        self.domain_lambdas
            .get(domain_id)
            .copied()
            .unwrap_or(self.cfg.default_lambda)
    }

    pub fn bump_domain_lambda(&mut self, domain_id: &str, amount: f64) {
        let e = self.domain_lambdas.entry(domain_id.to_string()).or_insert(self.cfg.default_lambda);
        *e = (*e + amount).min(self.cfg.default_lambda * 2.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, NoveltyConfig};

    fn ncfg() -> NoveltyConfig {
        Config::default().novelty
    }

    fn vec(seed: u64) -> Vec<f32> {
        let mut x = seed;
        (0..384)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((x >> 33) as f64 / u32::MAX as f64 - 0.5) as f32
            })
            .collect()
    }

    #[test]
    fn initialization() {
        let n = CountBasedNovelty::new(ncfg());
        assert_eq!(n.cfg.n_hash_tables, 3);
        assert_eq!(n.cfg.hash_size, 2048);
        assert!(n.cfg.min_count_pessimistic);
        assert!((n.cfg.default_lambda - 0.5).abs() < 1e-9);
    }

    #[test]
    fn bonus_decays_with_visits() {
        let mut n = CountBasedNovelty::new(ncfg());
        let ctx = vec(1);
        let b0 = n.bonus(&ctx, None);
        assert!(b0 > 0.0 && b0 <= 1.0);
        for _ in 0..10 {
            n.visit(&ctx);
        }
        let b1 = n.bonus(&ctx, None);
        assert!(b1 < b0);
    }

    #[test]
    fn visit_count_increments() {
        let mut n = CountBasedNovelty::new(ncfg());
        let ctx = vec(1);
        assert_eq!(n.visit_count(&ctx), 0);
        n.visit(&ctx);
        assert_eq!(n.visit_count(&ctx), 1);
        n.visit(&ctx);
        assert_eq!(n.visit_count(&ctx), 2);
    }

    #[test]
    fn different_contexts_different_buckets() {
        let mut n = CountBasedNovelty::new(ncfg());
        let ca = vec![1.0; 384];
        let cb = vec![-1.0; 384];
        n.visit(&ca);
        n.visit(&ca);
        assert!(n.visit_count(&ca) > 0);
        assert_eq!(n.visit_count(&cb), 0);
    }

    #[test]
    fn lambda_override() {
        let n = CountBasedNovelty::new(ncfg());
        let ctx = vec(1);
        let d = n.bonus(&ctx, None);
        let o = n.bonus(&ctx, Some(2.0));
        assert!(o > d);
    }

    #[test]
    fn lambda_floor_enforced() {
        let n = CountBasedNovelty::new(ncfg());
        let ctx = vec(1);
        let b = n.bonus(&ctx, Some(0.0));
        assert!(b >= n.cfg.lambda_floor);
    }

    #[test]
    fn action_bonus_decays() {
        let mut n = CountBasedNovelty::new(ncfg());
        let b1 = n.action_bonus("tool_x", None);
        n.visit_action("tool_x");
        let b2 = n.action_bonus("tool_x", None);
        assert!(b2 < b1);
        assert_eq!(n.action_count("tool_x"), 1);
    }

    #[test]
    fn user_exploration_score_rises() {
        let mut n = CountBasedNovelty::new(ncfg());
        assert_eq!(n.user_exploration_score(), 0.0);
        assert!(!n.user_exploration_active());
        for _ in 0..10 {
            n.record_user_action();
        }
        assert!(n.user_exploration_score() > 0.5);
        assert!(n.user_exploration_active());
    }

    #[test]
    fn get_lambda_for_mode() {
        let n = CountBasedNovelty::new(ncfg());
        assert_eq!(n.get_lambda_for_mode("thought_partner"), 0.5);
        assert!(n.get_lambda_for_mode("agent") < 0.5);
    }

    #[test]
    fn domain_lambda_bump_capped() {
        let mut n = CountBasedNovelty::new(ncfg());
        assert_eq!(n.get_lambda_for_domain("python"), 0.5);
        n.bump_domain_lambda("python", 0.02);
        assert!(n.get_lambda_for_domain("python") > 0.5);
        for _ in 0..100 {
            n.bump_domain_lambda("python", 0.02);
        }
        assert!(n.get_lambda_for_domain("python") <= 1.0);
    }

    #[test]
    fn hash_seeds_produce_different_buckets() {
        let n = CountBasedNovelty::new(ncfg());
        let ctx = vec(1);
        let h0 = hash_embedding(&ctx, 0, n.cfg.hash_size);
        let h1 = hash_embedding(&ctx, 1, n.cfg.hash_size);
        let h2 = hash_embedding(&ctx, 2, n.cfg.hash_size);
        assert!(h0 != h1 || h1 != h2);
    }
}
