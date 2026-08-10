//! Prints what llama.cpp's device registry enumerates, and what
//! `local::backend::select_backend` picks from it for each preference.
//!
//! Kept as an example rather than a test because the answer is machine
//! dependent: a test that asserted "a GPU is present" would fail on CI and on
//! any CPU-only dev box. `local::backend`'s own unit tests cover the policy
//! against synthetic device lists; this is for confirming a *real* machine
//! sees what it should.
//!
//! Pass a GGUF path to additionally load it through `LocalEngine` and report
//! what `fit_params` settled on — the only way to see automatic offload
//! actually resolve, since it needs a real model and a real driver.
//!
//! ```text
//! cargo run --example backend_probe --features local-engine [-- <path.gguf>]
//! ```

fn main() {
    let devices = llama_cpp_2::list_llama_ggml_backend_devices();
    println!("{} device(s) enumerated:", devices.len());
    for d in &devices {
        println!(
            "  [{}] {} / {} ({:?}) free={} MiB total={} MiB",
            d.index,
            d.backend,
            if d.description.is_empty() {
                &d.name
            } else {
                &d.description
            },
            d.device_type,
            d.memory_free / (1024 * 1024),
            d.memory_total / (1024 * 1024),
        );
    }
    for pref in ["auto", "vulkan", "cuda", "cpu"] {
        let s = bigtiny_rust::local::backend::select_from(&devices, pref);
        println!(
            "  {pref:>7} -> {} {}",
            s.backend,
            s.device.as_deref().unwrap_or("-")
        );
    }

    let Some(path) = std::env::args().nth(1) else {
        return;
    };
    // `n_gpu_layers: -1` and `n_ctx: 0` are both the "automatic" sentinels, so
    // this is the configuration that exercises fitting on both axes.
    let mut cfg = bigtiny_rust::config::LocalEngineConfig {
        n_gpu_layers: -1,
        n_ctx: 0,
        ..Default::default()
    };
    for pref in ["auto", "cpu"] {
        cfg.backend = pref.to_string();
        println!("\nloading {path} with backend={pref}...");
        match bigtiny_rust::local::LocalEngine::load(std::path::Path::new(&path), &cfg) {
            Ok(e) => {
                let m = e.model();
                println!(
                    "  geometry: n_layer={} n_embd={} n_head={} n_head_kv={} size={} MiB",
                    m.n_layer(),
                    m.n_embd(),
                    m.n_head(),
                    m.n_head_kv(),
                    m.size() / (1024 * 1024),
                );
                println!(
                    "  on {} ({}), {} layers offloaded, n_ctx {} (budget {} MiB)",
                    e.selected_backend().backend,
                    e.selected_backend().device.as_deref().unwrap_or("cpu"),
                    e.n_gpu_layers(),
                    e.effective_n_ctx(),
                    e.selected_backend().usable_memory / (1024 * 1024),
                );
            }
            Err(e) => println!("  load failed: {e}"),
        }
    }
}
