//! Deterministic lexical feature-hashing vectorizer, ported from
//! `embeddings.py::EmbeddingProvider._embed_hashing`. A fallback used when
//! Ollama's /api/embeddings is unavailable: identical text always maps to the
//! identical vector, unrelated vocabularies land far apart.

use std::collections::VecDeque;

use crate::config::Config;

/// mmh3-compatible MurmurHash3 x86 32-bit. Returns the raw u32 hash, matching
/// Python's `mmh3.hash(key, seed)` (the Python result is that u32 reinterpreted
/// as a signed i32; callers that need Python's signed semantics can cast).
pub fn mmh3_32(data: &[u8], seed: i32) -> i64 {
    murmur3_x86_32(data, seed as u32) as i64
}

/// Standard MurmurHash3 x86_32 (fmix + multiplication by c1/c2 with the
/// standard 0x1b873593/0x5bd1e995 constants). Bit-for-bit compatible with
/// the `mmh3` Python binding for the same key bytes and seed.
fn murmur3_x86_32(key: &[u8], seed: u32) -> u32 {
    const C1: u32 = 0xcc9e_2d51;
    const C2: u32 = 0x1b87_3593;
    let mut h1: u32 = seed;
    let rounded_end = key.len() & !3;

    let mut i = 0;
    while i < rounded_end {
        let mut k1 = u32::from_le_bytes([key[i], key[i + 1], key[i + 2], key[i + 3]]);
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(C2);

        h1 ^= k1;
        h1 = h1.rotate_left(13);
        h1 = h1.wrapping_mul(5).wrapping_add(0xe654_6b64);
        i += 4;
    }

    let mut k1: u32 = 0;
    let tail = &key[rounded_end..];
    match tail.len() {
        3 => {
            k1 ^= (tail[2] as u32) << 16;
            k1 ^= (tail[1] as u32) << 8;
            k1 ^= tail[0] as u32;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(15);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        2 => {
            k1 ^= (tail[1] as u32) << 8;
            k1 ^= tail[0] as u32;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(15);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        1 => {
            k1 ^= tail[0] as u32;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(15);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        _ => {}
    }

    h1 ^= key.len() as u32;

    // finalizer (fmix32)
    h1 ^= h1 >> 16;
    h1 = h1.wrapping_mul(0x85eb_ca6b);
    h1 ^= h1 >> 13;
    h1 = h1.wrapping_mul(0xc2b2_ae35);
    h1 ^= h1 >> 16;

    h1
}

fn hash_token(tok: &str, seed: i32) -> i64 {
    mmh3_32(tok.as_bytes(), seed)
}

/// Deterministic signed-hashing projection to `dim` dims. Returns a
/// unit-norm vector (or zeros for empty input).
pub fn hash_embed(text: &str, dim: usize) -> Vec<f32> {
    let mut vec = vec![0.0f32; dim];
    let lower = text.to_lowercase();
    for tok in word_tokens(&lower) {
        let idx = (hash_token(tok, 2026)).rem_euclid(dim as i64) as usize;
        let sign = if hash_token(tok, 2027) % 2 == 0 {
            1.0
        } else {
            -1.0
        };
        vec[idx] += sign;
    }
    let n: f32 = vec.iter().map(|&x| x * x).sum::<f32>().sqrt();
    if n > 1e-10 {
        for x in vec.iter_mut() {
            *x /= n;
        }
    }
    vec
}

/// Extract [a-z0-9]+ tokens, matching Python's
/// `_WORD_RE = re.compile(r"[a-z0-9]+")` applied to `text.lower()`. The input
/// is expected lowercase (see `hash_embed`).
pub fn word_tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut start: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        let is_word = (b.is_ascii_lowercase() && b.is_ascii_alphabetic()) || b.is_ascii_digit();
        if is_word {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            out.push(&text[s..i]);
        }
    }
    if let Some(s) = start {
        out.push(&text[s..]);
    }
    out
}

/// Small LRU cache keyed on the exact text. Stores the `semantic` flag
/// alongside each vector (true = Ollama, false = hash fallback) so a cache
/// hit can report the space that actually produced it, instead of a
/// hardcoded guess — a hash-fallback vector cached during an Ollama outage
/// must still read back as non-semantic once Ollama recovers, or callers
/// like `reembed_stale_beliefs` that trust the reported space either mistag
/// it as semantic or perpetually skip a belief that's actually re-embeddable.
pub struct EmbedCache {
    cap: usize,
    map: std::collections::HashMap<String, (Vec<f32>, bool)>,
    order: VecDeque<String>,
}

impl EmbedCache {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            map: std::collections::HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn get(&self, text: &str) -> Option<&(Vec<f32>, bool)> {
        self.map.get(text)
    }

    pub fn put(&mut self, text: String, vec: Vec<f32>, semantic: bool) {
        if self.map.contains_key(&text) {
            self.order.retain(|t| *t != text);
        }
        self.map.insert(text.clone(), (vec, semantic));
        self.order.push_back(text);
        while self.order.len() > self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Convenience over the hashing fallback using a Config's dim.
pub fn hash_embed_config(text: &str, cfg: &Config) -> Vec<f32> {
    hash_embed(text, cfg.embedding_dim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_match_word_regex() {
        assert_eq!(word_tokens("hello, world 123!"), vec!["hello", "world", "123"]);
        assert_eq!(word_tokens("  spaced   out "), vec!["spaced", "out"]);
        assert_eq!(word_tokens(""), vec![] as Vec<&str>);
    }

    #[test]
    fn hashing_is_deterministic() {
        let a = hash_embed("reviewing my novel draft about violence", 384);
        let b = hash_embed("reviewing my novel draft about violence", 384);
        assert_eq!(a, b);
    }

    #[test]
    fn hashing_distinguishes_unrelated_topics() {
        let a = hash_embed("reviewing my novel draft about violence", 384);
        let b = hash_embed("writing a privacy policy for a service", 384);
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        assert!(dot < 0.5);
    }

    #[test]
    fn correct_dim_and_unit_norm() {
        let v = hash_embed("some context text", 384);
        assert_eq!(v.len(), 384);
        let n: f32 = v.iter().map(|&x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4);
    }

    #[test]
    fn empty_or_whitespace_is_zeros() {
        assert_eq!(hash_embed("", 384), vec![0.0; 384]);
        assert_eq!(hash_embed("   ", 384), vec![0.0; 384]);
    }

    #[test]
    fn cache_respects_cap_and_evicts_lru() {
        let mut c = EmbedCache::new(3);
        for i in 0..5 {
            c.put(format!("context {i}"), vec![i as f32], true);
        }
        assert_eq!(c.len(), 3);
        // "context 5" was added last; so was 3,4; earlier ones evicted
        assert!(c.get("context 4").is_some());
        assert!(c.get("context 0").is_none());
    }

    #[test]
    fn cache_reorders_on_put() {
        let mut c = EmbedCache::new(2);
        c.put("a".into(), vec![1.0], true);
        c.put("b".into(), vec![2.0], true);
        c.put("a".into(), vec![1.0], true);
        // order now b, a -> adding c evicts b
        c.put("c".into(), vec![3.0], true);
        assert!(c.get("a").is_some());
        assert!(c.get("b").is_none());
    }
}
