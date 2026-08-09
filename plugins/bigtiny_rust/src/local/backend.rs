//! Backend selection and VRAM reporting (docs/ANDROID.md §3.3, D20).
//!
//! Everything here comes from llama.cpp's own ggml backend registry via
//! [`list_llama_ggml_backend_devices`] — no `nvidia-smi`, no subprocess, no
//! second source of truth. That matters beyond tidiness: the model card's
//! "Backend now" line, its VRAM figure, and the "Recommended for this device"
//! badge all read the *same* device record the loader will actually use, so
//! they cannot disagree with each other or with reality.
//!
//! **Superseding §3.3's fit formula.** That section specified a hand-rolled
//! `(file_size x resident_fraction) + KV + scratch, x1.18` estimate. It isn't
//! implemented and shouldn't be: `memory_free`/`memory_total` here are
//! measured, not estimated.
//!
//! **`fit_params` is deliberately not wired yet.** llama.cpp's own
//! optimal-`n_gpu_layers` solver (`LlamaModelParams::fit_params`, feature
//! `common`) is the right long-term answer for GPU offload, but it requires
//! `n_gpu_layers` left at its `-1` default, co-decides model *and* context
//! params in one call (which this crate builds separately — see
//! [`super::engine`]'s `base_params`), and is explicitly not thread-safe. On a
//! CPU-only build it would resolve to "0 layers offloaded" every time, so
//! wiring it now would mean adding an unvalidated, globally-mutating call to
//! every model load in exchange for nothing. It lands with the first GPU
//! backend, where it can actually be tested.

use llama_cpp_2::{
    list_llama_ggml_backend_devices, LlamaBackendDevice, LlamaBackendDeviceType,
};

/// Which compute backend a load will actually use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Cuda,
    Vulkan,
    Cpu,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cuda => "cuda",
            Self::Vulkan => "vulkan",
            Self::Cpu => "cpu",
        }
    }
}

/// The chosen backend plus the device record it came from.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SelectedBackend {
    /// `"cuda"` | `"vulkan"` | `"cpu"`.
    pub backend: String,
    /// Human-readable device, e.g. `"NVIDIA GeForce RTX 3080"`. `None` on CPU.
    pub device: Option<String>,
    /// ggml device index, usable with `LlamaModelParams::with_devices`.
    pub device_index: Option<usize>,
    /// Free/total VRAM in bytes on the selected device. Both `0` on CPU —
    /// there is no separate budget to report, and inventing one (system RAM)
    /// would make the card's VRAM row lie.
    pub memory_free: u64,
    pub memory_total: u64,
}

impl SelectedBackend {
    pub fn kind(&self) -> BackendKind {
        match self.backend.as_str() {
            "cuda" => BackendKind::Cuda,
            "vulkan" => BackendKind::Vulkan,
            _ => BackendKind::Cpu,
        }
    }

    fn cpu() -> Self {
        Self {
            backend: BackendKind::Cpu.as_str().to_string(),
            device: None,
            device_index: None,
            memory_free: 0,
            memory_total: 0,
        }
    }
}

/// Rank a device: CUDA beats Vulkan beats everything else (D20 step 2).
/// `None` for anything that isn't a usable accelerator.
fn gpu_rank(d: &LlamaBackendDevice) -> Option<u8> {
    if !matches!(
        d.device_type,
        LlamaBackendDeviceType::Gpu | LlamaBackendDeviceType::IntegratedGpu
    ) {
        return None;
    }
    match d.backend.to_ascii_lowercase().as_str() {
        b if b.contains("cuda") => Some(0),
        b if b.contains("vulkan") => Some(1),
        _ => None,
    }
}

fn kind_of(d: &LlamaBackendDevice) -> BackendKind {
    match d.backend.to_ascii_lowercase() {
        b if b.contains("cuda") => BackendKind::Cuda,
        b if b.contains("vulkan") => BackendKind::Vulkan,
        _ => BackendKind::Cpu,
    }
}

/// Pick from an already-enumerated device list. Split out from
/// [`select_backend`] so the policy is testable without a GPU (or any ggml
/// backend at all) present.
///
/// `preference` is `LocalEngineConfig::backend`: `"auto"` (or anything
/// unrecognised) ranks by [`gpu_rank`]; `"cpu"` forces CPU; `"cuda"`/`"vulkan"`
/// pin that backend and **fall back to CPU rather than erroring** if it isn't
/// present — a machine that lost its GPU (driver update, eGPU unplugged, VM
/// migration) should degrade to slow, not to broken.
pub fn select_from(devices: &[LlamaBackendDevice], preference: &str) -> SelectedBackend {
    let pref = preference.trim().to_ascii_lowercase();
    if pref == "cpu" {
        return SelectedBackend::cpu();
    }

    let chosen = if pref == "cuda" || pref == "vulkan" {
        let want = if pref == "cuda" {
            BackendKind::Cuda
        } else {
            BackendKind::Vulkan
        };
        let found = devices
            .iter()
            .filter(|d| gpu_rank(d).is_some() && kind_of(d) == want)
            // Most free VRAM wins among equals.
            .max_by_key(|d| d.memory_free);
        if found.is_none() {
            tracing::warn!(
                "backend {pref:?} was requested but no such device is available; using CPU"
            );
        }
        found
    } else {
        devices
            .iter()
            .filter_map(|d| gpu_rank(d).map(|r| (r, d)))
            // Rank first (CUDA over Vulkan), then most free VRAM.
            .min_by_key(|(rank, d)| (*rank, usize::MAX - d.memory_free))
            .map(|(_, d)| d)
    };

    match chosen {
        Some(d) => SelectedBackend {
            backend: kind_of(d).as_str().to_string(),
            device: Some(if d.description.is_empty() {
                d.name.clone()
            } else {
                d.description.clone()
            }),
            device_index: Some(d.index),
            memory_free: d.memory_free as u64,
            memory_total: d.memory_total as u64,
        },
        None => SelectedBackend::cpu(),
    }
}

/// Query the live backend registry and select per `preference`.
///
/// Not cached. Enumeration is a registry walk plus a driver query for free
/// memory — cheap, and the free-memory figure is only useful *because* it's
/// live. A `OnceLock` here would also go stale the moment Phase 4.5's
/// in-process reload applies a changed `backend` setting without restarting
/// the daemon.
pub fn select_backend(preference: &str) -> SelectedBackend {
    select_from(&list_llama_ggml_backend_devices(), preference)
}

/// Every device the registry knows about, for the Settings model card's
/// diagnostics. Unfiltered on purpose: showing a CPU-only machine an empty
/// list is less informative than showing it the CPU entry ggml reports.
pub fn available_devices() -> Vec<SelectedBackend> {
    list_llama_ggml_backend_devices()
        .iter()
        .map(|d| SelectedBackend {
            backend: kind_of(d).as_str().to_string(),
            device: Some(if d.description.is_empty() {
                d.name.clone()
            } else {
                d.description.clone()
            }),
            device_index: Some(d.index),
            memory_free: d.memory_free as u64,
            memory_total: d.memory_total as u64,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(
        index: usize,
        backend: &str,
        description: &str,
        device_type: LlamaBackendDeviceType,
        free: usize,
    ) -> LlamaBackendDevice {
        LlamaBackendDevice {
            index,
            name: format!("{backend}{index}"),
            description: description.to_string(),
            backend: backend.to_string(),
            memory_total: free * 2,
            memory_free: free,
            device_type,
        }
    }

    fn cuda(i: usize, free: usize) -> LlamaBackendDevice {
        dev(i, "CUDA", "NVIDIA GeForce RTX 3080", LlamaBackendDeviceType::Gpu, free)
    }
    fn vulkan(i: usize, free: usize) -> LlamaBackendDevice {
        dev(i, "Vulkan", "AMD Radeon RX 7900", LlamaBackendDeviceType::Gpu, free)
    }
    fn cpu_dev() -> LlamaBackendDevice {
        dev(9, "CPU", "", LlamaBackendDeviceType::Cpu, 0)
    }

    /// The CPU-only case — which is every build today, since no GPU cargo
    /// feature is enabled. Must report CPU with a zeroed VRAM budget, not a
    /// borrowed system-RAM figure that would make the model card lie.
    #[test]
    fn a_machine_with_no_gpu_selects_cpu_and_reports_no_vram() {
        let s = select_from(&[cpu_dev()], "auto");
        assert_eq!(s.kind(), BackendKind::Cpu);
        assert_eq!(s.device, None);
        assert_eq!(s.memory_total, 0);
        assert_eq!(s.memory_free, 0);
    }

    #[test]
    fn an_empty_registry_selects_cpu() {
        assert_eq!(select_from(&[], "auto").kind(), BackendKind::Cpu);
    }

    /// D20 step 2: CUDA outranks Vulkan when both are enumerated, regardless
    /// of order or free memory.
    #[test]
    fn auto_prefers_cuda_over_vulkan() {
        let s = select_from(&[vulkan(0, 24_000), cuda(1, 8_000), cpu_dev()], "auto");
        assert_eq!(s.kind(), BackendKind::Cuda);
        assert_eq!(s.device_index, Some(1));
    }

    /// Among same-backend devices, most free VRAM wins — a 24 GB card and an
    /// 8 GB card are both "CUDA", and picking the small one would silently
    /// cap what fits.
    #[test]
    fn auto_picks_the_device_with_the_most_free_memory() {
        let s = select_from(&[cuda(0, 8_000), cuda(1, 24_000)], "auto");
        assert_eq!(s.device_index, Some(1));
        assert_eq!(s.memory_free, 24_000);
    }

    #[test]
    fn cpu_preference_forces_cpu_even_with_a_gpu_present() {
        let s = select_from(&[cuda(0, 24_000)], "cpu");
        assert_eq!(s.kind(), BackendKind::Cpu);
        assert_eq!(s.device_index, None);
    }

    #[test]
    fn an_explicit_backend_preference_is_honoured() {
        let s = select_from(&[cuda(0, 8_000), vulkan(1, 4_000)], "vulkan");
        assert_eq!(s.kind(), BackendKind::Vulkan);
        assert_eq!(s.device_index, Some(1));
    }

    /// A pinned backend that has since disappeared (driver update, eGPU
    /// unplugged, VM migrated) must degrade to CPU, not fail the load — slow
    /// beats broken, and the user can see the backend changed on the card.
    #[test]
    fn a_pinned_backend_that_is_gone_falls_back_to_cpu() {
        let s = select_from(&[vulkan(0, 4_000), cpu_dev()], "cuda");
        assert_eq!(s.kind(), BackendKind::Cpu);
    }

    /// An unrecognised preference behaves as `auto` rather than erroring or
    /// silently forcing CPU — same "a typo shouldn't break the engine"
    /// reasoning as `EmbedPooling::parse` and `parse_kv_cache_type`.
    #[test]
    fn an_unknown_preference_behaves_as_auto() {
        let s = select_from(&[cuda(0, 8_000)], "wat");
        assert_eq!(s.kind(), BackendKind::Cuda);
    }

    /// A discrete GPU and an integrated one are both offload targets; the
    /// filter must not drop iGPUs (common on laptops, and the only GPU on
    /// many of them).
    #[test]
    fn integrated_gpus_count_as_offload_targets() {
        let igpu = dev(
            0,
            "Vulkan",
            "Intel Arc Graphics",
            LlamaBackendDeviceType::IntegratedGpu,
            2_000,
        );
        assert_eq!(select_from(&[igpu], "auto").kind(), BackendKind::Vulkan);
    }

    /// A backend ggml reports that we have no ranking for (e.g. SYCL, Metal,
    /// a future one) must not be selected by `auto` — we can't reason about
    /// its memory model, and CPU is the safe answer.
    #[test]
    fn an_unranked_gpu_backend_is_not_auto_selected() {
        let sycl = dev(0, "SYCL", "Some Accelerator", LlamaBackendDeviceType::Gpu, 8_000);
        assert_eq!(select_from(&[sycl], "auto").kind(), BackendKind::Cpu);
    }
}
