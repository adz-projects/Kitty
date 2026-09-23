//! Config loader behavior and the §2 default-table pin (project-plan.md §2,
//! §13.5). These tests are load-bearing: `Config` is the single source of
//! truth for every tunable, so its defaults and loader semantics are pinned
//! here, not left to drift.

use memorabilia::config::Config;

fn temp_yaml(body: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("memorabilia_cfg_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("config.yaml");
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn defaults_load_with_no_file() {
    // Plan §13.5: defaults load with no file present.
    assert_eq!(Config::load(None).unwrap(), Config::default());

    let missing = std::env::temp_dir()
        .join(format!("memorabilia_missing_{}", uuid::Uuid::new_v4()))
        .join("config.yaml");
    assert!(!missing.exists());
    assert_eq!(Config::load(Some(&missing)).unwrap(), Config::default());
}

#[test]
fn empty_file_loads_defaults() {
    let p = temp_yaml("");
    assert_eq!(Config::load(Some(&p)).unwrap(), Config::default());

    let p = temp_yaml("   \n  \n");
    assert_eq!(Config::load(Some(&p)).unwrap(), Config::default());
}

#[test]
fn unknown_keys_are_ignored_and_partial_override_applies() {
    // Plan §13.5: unknown keys are ignored; a file overrides only the keys
    // it sets.
    let p = temp_yaml(
        "output_token_cap: 600\nbogus_top_level: 42\noutdated_suppression_days: 30\nextraction:\n  provider_url: http://127.0.0.1:9999\n  batch_size: 3\n  totally_unknown: true\n",
    );
    let c = Config::load(Some(&p)).unwrap();
    let d = Config::default();

    assert_eq!(c.output_token_cap, 600);
    assert_eq!(c.extraction.provider_url, "http://127.0.0.1:9999");
    assert_eq!(c.outdated_suppression_days, 30);
    assert_eq!(c.embedding_provider_url, "http://localhost:11434");
    // Keys the file does not set keep their defaults.
    assert_eq!(c.embedding_dim, d.embedding_dim);
    assert_eq!(c.chunk_chars_max, d.chunk_chars_max);
    assert_eq!(c.extraction.timeout_s, d.extraction.timeout_s);
    assert_eq!(c.extraction.max_concurrent, d.extraction.max_concurrent);
    // The overridden knob took effect (boundary); the neighbours kept
    // theirs.
    assert_eq!(c.extraction.batch_size, 3);
    assert_eq!(c.maintenance_tick_s, d.maintenance_tick_s);
}

#[test]
fn malformed_yaml_is_a_config_error() {
    let p = temp_yaml("output_token_cap: [unclosed\n  - oops");
    let err = Config::load(Some(&p)).unwrap_err();
    assert!(matches!(err, memorabilia::error::Error::Config(_)));
}

#[test]
fn defaults_match_plan_section2() {
    let c = Config::default();
    assert_eq!(c.embedding_dim, 384);
    assert_eq!(c.embedding_model, "qwen3-embedding:0.6b");
    assert_eq!(c.embedding_timeout_ms, 1500);
    assert_eq!(c.embedding_provider_url, "http://localhost:11434");
    assert_eq!(c.embedding_probe_interval_s, 60);
    assert_eq!(c.embedding_cache_size, 256);
    assert_eq!((c.chunk_chars_min, c.chunk_chars_max), (512, 1024));
    assert!((c.cluster_similarity_threshold - 0.92).abs() < 1e-12);
    assert!((c.semantic_dedup_threshold - 0.92).abs() < 1e-12);
    assert!((c.dispute_strength_floor - 0.5).abs() < 1e-12);
    assert_eq!(c.tau_static_days, 365);
    assert_eq!(c.tau_transient_days, 7);
    assert_eq!(c.tau_pre_days, 7);
    assert_eq!(c.tau_post_hours, 48);
    assert_eq!(c.reinforcement_promote_threshold, 5);
    assert_eq!(c.reinforcement_count_max, 5);
    assert!((c.promotion_min_reliability - 0.6).abs() < 1e-12);
    assert!((c.reliability_drift_up - 0.15).abs() < 1e-12);
    assert!((c.reliability_drift_down - 0.25).abs() < 1e-12);
    assert!((c.reliability_band - 0.2).abs() < 1e-12);
    assert_eq!(c.archive_retention_days, 90);
    assert_eq!(c.retrieval_depth_cap, 3);
    assert_eq!(c.retrieval_seed_chunks, 8);
    assert_eq!(c.retrieval_min_seeds, 3);
    assert!((c.activation_prune_threshold - 0.05).abs() < 1e-12);
    assert!((c.activation_hop_decay - 0.5).abs() < 1e-12);
    assert_eq!(c.output_token_cap, 1500);
    assert_eq!(c.injection_token_cap, 500);
    assert!((c.metaboost_cap - 3.0).abs() < 1e-12);
    assert!((c.correlation_lambda - 0.05).abs() < 1e-12);
    assert!((c.metaboost_urgency_high - 1.0).abs() < 1e-12);
    assert!((c.metaboost_urgency_medium - 0.4).abs() < 1e-12);
    assert!((c.metaboost_urgency_low - 0.1).abs() < 1e-12);
    assert!((c.metaboost_importance_high - 0.8).abs() < 1e-12);
    assert!((c.metaboost_importance_medium - 0.3).abs() < 1e-12);
    assert!((c.metaboost_importance_low - 0.1).abs() < 1e-12);
    assert!((c.metaboost_unknown_triage_premium - 0.5).abs() < 1e-12);
    assert_eq!(c.conflict_probe_neighbors, 5);
    assert!((c.conflict_probe_similarity_min - 0.55).abs() < 1e-12);
    assert!((c.rhymes_min_confidence - 0.7).abs() < 1e-12);
    assert_eq!(c.rhymes_batch_cap, 50);
    assert_eq!(c.tombstone_min_chars, 20);
    assert_eq!(c.tombstone_digit_run, 8);
    assert!((c.forget_match_threshold - 0.8).abs() < 1e-12);
    assert_eq!(c.outdated_suppression_days, 90);
    assert_eq!(c.extraction.provider_url, "http://localhost:11434");
    // Plan §2 names the default "local instruct model"; it must name a
    // concrete model (qwen3 4B-class per §3.6) so the default is runnable.
    assert!(!c.extraction.model.is_empty());
    // Raised from the plan's 12s in 0.11.2: the local Gemma-E2B extractor
    // timed out on every chunk at 12s, so nothing was ever extracted.
    assert_eq!(c.extraction.timeout_s, 60);
    assert_eq!(c.extraction.retry_backoff_s, 60);
    assert_eq!(c.extraction.max_concurrent, 1);
    assert_eq!(c.extraction.batch_size, 5);
    assert_eq!(c.maintenance_tick_s, 60);
    assert_eq!(c.maintenance_heavy_interval_hours, 24);
}

#[test]
fn embedding_space_sentinels() {
    use memorabilia::config::{DEFAULT_EMBEDDING_MODEL, HASH_EMBED_MODEL};
    // Plan §3.3: two tags, two incompatible vector spaces.
    assert_eq!(DEFAULT_EMBEDDING_MODEL, "qwen3-embedding:0.6b");
    assert_eq!(HASH_EMBED_MODEL, "__lexical_hash__");
    assert_ne!(DEFAULT_EMBEDDING_MODEL, HASH_EMBED_MODEL);
}
