//! Providers and their models. A provider is one service (OpenRouter,
//! Anthropic, a self-hosted server…) with one key and the models it offers;
//! the user checks the models worth choosing from. A **setup**
//! ([`setups`]) is one checked model with the user's own name, thinking
//! level, context length and system prompt; its **card** is the
//! [`ProviderProfile`] a chat, a scheduled task, a specialist host and an
//! engine provider row are pinned to ([`crate::config::Config::card`]), so
//! everything below Settings still sees one model per card.
//!
//! Profile *metadata* lives in app config; secrets live only in the Windows
//! Credential Manager via `keyring`, under the provider's id (a card reads its
//! provider's: [`ProviderProfile::secret_account`]) — never on disk in
//! plaintext (CLAUDE.md rule 4).

mod connection;
pub mod debate;
pub mod endpoint;
pub mod learning;
mod keyring;
mod network;
pub mod setups;

pub use connection::{test_connection, test_connection_with_key};
pub use keyring::{
    delete_secret, get_secret_async, get_secret_checked, migrate_secrets, set_secret_async,
    v1_encryption_key,
};
// Only the desktop uninstall cleanup removes it.
#[cfg(desktop)]
pub use keyring::V1_ENCRYPTION_KEY_ACCOUNT;
pub use network::{is_hosted_endpoint, network_tier_for, NetworkTier};
pub use debate::DebateSeat;
pub use learning::{LearningModelSetting, LEARNING_CARD_ID};
pub use setups::{migrate_to_setups, repoint_setups, Setup};

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

/// A provider (as stored), or a setup's card (built on demand by
/// [`setups::Setup::card`], with `provider_id` set and one model).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub id: String,
    pub name: String,
    /// `ollama` | `openrouter` | `anthropic` | `openai` | `fireworks` |
    /// `deepinfra` | `custom_openai` | `local`. A plain string, not an enum
    /// — a new hosted OpenAI-compatible dialect (like the 2 above) needs no
    /// type change here, just new match arms wherever behavior actually
    /// differs (see `config/providers/connection.rs`'s
    /// `test_connection` and `bigtiny/providers.rs`'s
    /// `bigtiny_provider_target`).
    pub provider_type: String,
    pub base_url: String,
    /// Every model the provider offered when last fetched (or, for a server
    /// that lists none, typed in). On a model card: exactly its one model.
    #[serde(default)]
    pub models: Vec<String>,
    /// The models the user checked: the ones in Kitty's model list. A model
    /// not in `models` any more is ignored.
    #[serde(default)]
    pub enabled_models: Vec<String>,
    /// The id each model's card went by before setups (`model -> card id`).
    /// Kept only so `setups::repoint_setups` can follow a setup into the
    /// provider its model was merged into; new providers have none.
    #[serde(default)]
    pub model_ids: BTreeMap<String, String>,
    /// Models the user marked as reading images (for models OpenRouter's
    /// catalog does not know, such as self-hosted ones).
    #[serde(default)]
    pub vision_models: Vec<String>,
    /// Each model's context window, when known: confirmed by discovery
    /// (`bigtiny::context_window`) or carried over from a one-model card.
    /// `context_length` above it, when set, overrides every model's.
    #[serde(default)]
    pub context_lengths: BTreeMap<String, u32>,
    /// On a model card only: the provider it belongs to. Never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// User-declared trust (Round-2 item 18). Loopback is always trusted by tier;
    /// this makes a non-loopback provider trusted (globe) instead of untrusted (⚠).
    #[serde(default)]
    pub is_trusted: bool,
    /// Whether this provider may host specialist runs: `"preferred"`,
    /// `"allowed"` (the default when absent) or `"never"`.
    ///
    /// The user's own statement, and the first thing the daemon's host picker
    /// consults — the only signal it has that reflects intent rather than
    /// measurement. Pushed down in the provider's `config` blob.
    #[serde(default)]
    pub subagent_role: Option<String>,
    /// Per-provider sampling params (Round-2 item 27). `None` = provider/model
    /// default (BigTiny omits the field from the completion request entirely
    /// rather than sending an explicit default — see
    /// `bigtiny::providers::sync_active_provider`).
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    /// llama.cpp/Ollama sampling extension, no equivalent on hosted
    /// OpenAI-compatible or Anthropic endpoints. BigTiny only ever sends it
    /// for a `provider_type` of `ollama`/`custom_openai`
    /// (`bigtiny2::provider::openai_compat`), so setting it on a hosted
    /// profile is a silent no-op rather than an error.
    #[serde(default)]
    pub top_k: Option<i32>,
    /// Same scoping as `top_k`.
    #[serde(default)]
    pub min_p: Option<f32>,
    /// Repetition control. `None` here does not mean "off" the way it does
    /// for `temperature`/`top_p` — BigTiny fills in a repetition-safe
    /// default for self-hosted providers when this is unset (see
    /// `bigtiny2::provider::sampling::defaults_for`), because
    /// llama-server's own default disables repetition control entirely.
    /// Set this explicitly only to override that default.
    #[serde(default)]
    pub presence_penalty: Option<f32>,
    #[serde(default)]
    pub frequency_penalty: Option<f32>,
    /// Hard cap on one reply's length. `None` gets BigTiny's own default for
    /// self-hosted providers (see `presence_penalty`'s doc comment) — set
    /// this to override, not to enable a cap that doesn't otherwise exist.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub context_length: Option<u32>,
    /// On a model card: the user marked its model as reading images
    /// (`vision_models`). On a stored provider, only a value from before
    /// providers held many models, moved into `vision_models` at load.
    ///
    /// Manual override: this provider's models accept image content blocks.
    ///
    /// Vision support is otherwise detected from the model *name*
    /// (`src/lib/vision_models.ts`), which is deliberately conservative — an
    /// unrecognized name is treated as text-only. That is the right default
    /// when the cost of guessing wrong is a failed turn, but it means a
    /// self-hosted or oddly-named vision model has no way to be recognized,
    /// and the UI hides its image affordances with no recourse. This flag is
    /// that recourse: it only ever *widens* what's allowed (it ORs with
    /// detection), so ticking it for a text-only model is a user's own
    /// mistake to make, and leaving it off never blocks a model the patterns
    /// already know about.
    #[serde(default)]
    pub supports_vision: bool,
    /// Custom system prompt for this provider (Round-6 Feature 2). `None` =
    /// use the built-in default (see `src/lib/system_prompts.ts`). Applied as
    /// the session's `persona_override` on its first turn, which BigTiny
    /// renders as a real `role: "system"` message.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Override for `session/prompt`'s idle-reset timeout window (default 300s —
    /// see `AcpClient::request_session_prompt`'s doc comment). `None` = use the
    /// default. Useful for a model/provider known to have long gaps between
    /// streamed updates (e.g. a slow Tailscale-hosted host) where the default
    /// is too eager, or conversely one where a long silence reliably means
    /// "stuck" and the user would rather find out sooner than wait 5 minutes.
    #[serde(default)]
    pub prompt_idle_timeout_secs: Option<u32>,
    /// The `-np`/`--parallel` slot count this provider's own llama-server(-
    /// compatible) endpoint was started with, when known. `None` (the
    /// default) means: never pin this provider's turns to a KV-cache slot —
    /// correct for Ollama and anything not deliberately running a
    /// multi-slot llama-server. Threaded through to BigTiny's
    /// `ProviderConfig::parallel_slots` (`bigtiny::providers::
    /// sync_active_provider`), which derives `id_slot` per turn from it. A
    /// value here that doesn't match the real server's `--parallel` doesn't
    /// error — it just silently thrashes the KV cache instead of pinning
    /// it, so this is deliberately a plain number the user sets to match
    /// their own server config, not something Kitty can discover on its
    /// own.
    #[serde(default)]
    pub parallel_slots: Option<u32>,
    /// The user's override for whether this provider's model can call tools.
    /// `None` (the default) means detect it: the OpenRouter catalog when it
    /// knows the model, else assume yes (the daemon still notices a model
    /// that refuses tools and says so). Decides whether the model is offered
    /// tools at all, and whether a dropped file is sent as a path (for the
    /// file tools to open) or inlined.
    #[serde(default)]
    pub supports_tools: Option<bool>,
    /// Why this profile can no longer be used, when it cannot:
    /// `"unsupported"` for the retired "On this device" (`local`) type. A
    /// disabled profile stays listed so the user can see and delete it, but is
    /// never synced to the engine or used for a chat.
    #[serde(default)]
    pub disabled_reason: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

impl ProviderProfile {
    pub fn network_tier(&self) -> NetworkTier {
        network_tier_for(&self.base_url)
    }

    /// Whether this provider cannot work without an API key. A self-hosted
    /// endpoint (Ollama, or a custom OpenAI-compatible server) often has none.
    pub fn requires_key(&self) -> bool {
        !matches!(self.provider_type.as_str(), "ollama" | "custom_openai")
    }

    /// Usable for chat and synced to the engine.
    pub fn is_usable(&self) -> bool {
        self.disabled_reason.is_none()
    }

    /// Whether this provider's model can call tools: the user's override,
    /// else the OpenRouter catalog's answer for this exact model, else yes.
    pub fn tools_supported(
        &self,
        catalog: Option<&crate::openrouter::catalog::OpenRouterCatalog>,
    ) -> bool {
        if let Some(explicit) = self.supports_tools {
            return explicit;
        }
        let model = self.models.first().map(String::as_str).unwrap_or("");
        catalog
            .and_then(|c| crate::openrouter::catalog::tools_support_for(model, &c.entries))
            .unwrap_or(true)
    }
}

/// The built-in system prompt, used when a provider has no `system_prompt`
/// of its own. Set as the session's persona when the session is created
/// (`commands::session::new_session`) and when a new chat's provider is
/// changed, so every turn - including scheduled runs - gets it.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a capable, direct agentic assistant. You have \
filesystem and shell tools scoped to this conversation's own working directory. Use tools \
proactively rather than describing what you would do — take the action. When you create or \
save a file, use a relative path inside the working directory rather than an absolute path \
elsewhere. Be direct about assumptions and uncertainty rather than glossing over them, and \
prefer verifiable action (running a command, reading a file, writing output) over speculation.";

/// Appended to every chat's system prompt, whether the built-in one or a
/// provider's own: where the files the user asked for go, so the artifacts
/// drawer (which lists `outputs/` and nothing else) shows them.
pub const OUTPUTS_INSTRUCTION: &str = "Files the user asked you to produce — a document, a \
report, a dataset, a folder of downloads — go in the `outputs/` folder inside the working \
directory (create it if it does not exist). Working files you make along the way — scripts, \
scratch notes, caches, intermediate data — stay outside it. Only put something in `outputs/` \
if the user asked for it.";

/// `prompt` followed, after a blank line, by [`OUTPUTS_INSTRUCTION`] — once:
/// a prompt that already carries it is returned unchanged.
pub fn with_outputs_instruction(prompt: &str) -> String {
    let prompt = prompt.trim_end();
    if prompt.contains(OUTPUTS_INSTRUCTION) {
        return prompt.to_string();
    }
    format!("{prompt}\n\n{OUTPUTS_INSTRUCTION}")
}

/// The built-in prompt as a session runs with it: for a chat with no provider
/// card, and for a scheduled run with none.
pub fn default_system_prompt() -> String {
    with_outputs_instruction(DEFAULT_SYSTEM_PROMPT)
}

/// The system prompt a session on `profile` runs with.
pub fn system_prompt_for(profile: &ProviderProfile) -> String {
    let own = profile
        .system_prompt
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(DEFAULT_SYSTEM_PROMPT);
    with_outputs_instruction(own)
}

/// Retire the "On this device" (`local`) provider type: there is no local
/// chat engine. Its profiles are kept but disabled, and if one was the
/// default the default is cleared and flagged, so the UI can ask the user to
/// pick another. Returns whether anything changed. Pure, and idempotent.
pub fn migrate_local_profiles(cfg: &mut crate::config::Config) -> bool {
    let mut changed = false;
    for p in cfg
        .providers
        .iter_mut()
        .filter(|p| p.provider_type == "local")
    {
        if p.disabled_reason.is_none() {
            p.disabled_reason = Some("unsupported".to_string());
            changed = true;
        }
    }
    let default_disabled = cfg
        .active_provider_id
        .as_deref()
        .and_then(|id| cfg.provider_of_card(id))
        .is_some_and(|p| !p.is_usable());
    if default_disabled {
        cfg.active_provider_id = None;
        cfg.needs_default_provider = true;
        changed = true;
    }
    changed
}

/// Report whether the provider a chat is on answered, from a real send's
/// outcome (Kitty makes no inference calls of its own to probe with).
///
/// Only about *reachability*, and only for the chat's own card: `error_type`
/// is `None` for a success, which clears an offline banner, and the daemon's
/// classification for a failure, of which only `network_unreachable` means
/// "can't reach it". A bad key, no credits or an overflowing context are that
/// chat's error card, not a connectivity problem to broadcast (#9). Loopback
/// providers have nothing to be unreachable in this sense.
pub fn emit_health_from_send_result(
    app: &AppHandle,
    card: Option<&ProviderProfile>,
    error_type: Option<&str>,
) {
    let reachable = match error_type {
        None => true,
        Some("network_unreachable") => false,
        Some(_) => return,
    };
    let Some(p) = card.filter(|p| !matches!(network_tier_for(&p.base_url), NetworkTier::Local))
    else {
        return;
    };
    let host = network::host_of(&p.base_url);
    let _ = app.emit(
        "provider://health",
        json!({ "provider_id": p.id, "reachable": reachable, "host": host, "name": p.name }),
    );
}

// ------------------------------------------------------------ model cards

impl ProviderProfile {
    /// The keyring account holding this profile's key: a card reads its
    /// provider's.
    pub fn secret_account(&self) -> &str {
        self.provider_id.as_deref().unwrap_or(&self.id)
    }

    /// The model a card runs (its only one).
    pub fn model(&self) -> &str {
        self.models.first().map(String::as_str).unwrap_or("")
    }

    /// Whether `model` is listed and checked.
    pub fn is_enabled(&self, model: &str) -> bool {
        self.models.iter().any(|m| m == model) && self.enabled_models.iter().any(|m| m == model)
    }

    /// The listed models that are checked, in listed order.
    pub fn checked_models(&self) -> impl Iterator<Item = &String> {
        self.models
            .iter()
            .filter(|m| self.enabled_models.contains(m))
    }
}

impl crate::config::Config {
    /// Every setup as its card, in the setups' order (unusable ones too:
    /// `is_usable` tells them apart).
    pub fn cards(&self) -> Vec<ProviderProfile> {
        self.setups
            .iter()
            .filter_map(|s| {
                let p = self.providers.iter().find(|p| p.id == s.provider_id)?;
                Some(s.card(p))
            })
            .collect()
    }

    /// The setup with this id, as its card.
    pub fn card(&self, card_id: &str) -> Option<ProviderProfile> {
        let s = self.setup(card_id)?;
        let p = self.providers.iter().find(|p| p.id == s.provider_id)?;
        Some(s.card(p))
    }

    /// The provider a setup belongs to.
    pub fn provider_of_card(&self, card_id: &str) -> Option<&ProviderProfile> {
        let s = self.setup(card_id)?;
        self.providers.iter().find(|p| p.id == s.provider_id)
    }

    /// Remember the context window confirmed for a setup's model. Returns
    /// whether anything changed. A setup's own `context_length` is the
    /// user's and wins.
    pub fn set_card_context_length(&mut self, card_id: &str, n: u32) -> bool {
        let Some(s) = self.setup(card_id) else {
            return false;
        };
        if s.context_length.is_some() {
            return false;
        }
        let (pid, model) = (s.provider_id.clone(), s.model.clone());
        let Some(p) = self.providers.iter_mut().find(|p| p.id == pid) else {
            return false;
        };
        p.context_lengths.insert(model, n) != Some(n)
    }

    /// Keep the default on a usable setup: when its model was unchecked or
    /// it or its provider deleted, `fallback` (else the first usable setup)
    /// takes over. Returns whether the default changed.
    pub fn repair_default(&mut self, fallback: Option<String>) -> bool {
        let cards = self.cards();
        let ok = |id: &str| cards.iter().any(|c| c.id == id && c.is_usable());
        if self.active_provider_id.as_deref().is_some_and(ok) {
            return false;
        }
        let next = fallback
            .filter(|id| ok(id))
            .or_else(|| cards.iter().find(|c| c.is_usable()).map(|c| c.id.clone()));
        let changed = next != self.active_provider_id;
        self.active_provider_id = next;
        changed
    }
}

/// Bring one-model cards (and the older many-model profiles) up to the
/// provider shape, without merging anything: every listed model is checked,
/// the first keeps the profile's id as its card id (so chats pinned to it
/// are unaffected) and the rest get `<id>-m<n>`, as the old split named them.
/// The card's vision flag and context window move onto its model. Pure.
pub fn upgrade_to_model_lists(providers: &mut [ProviderProfile]) {
    for p in providers.iter_mut() {
        if !p.model_ids.is_empty() || p.provider_id.is_some() {
            continue;
        }
        p.enabled_models = p.models.clone();
        for (i, model) in p.models.iter().enumerate() {
            let id = if i == 0 {
                p.id.clone()
            } else {
                format!("{}-m{i}", p.id)
            };
            p.model_ids.insert(model.clone(), id);
            if p.supports_vision {
                p.vision_models.push(model.clone());
            }
            if let Some(n) = p.context_length {
                p.context_lengths.insert(model.clone(), n);
            }
        }
        p.supports_vision = false;
        // A card's window was its model's (written by discovery as often as
        // by the user), so it moves onto the model.
        p.context_length = None;
    }
}

/// What two one-model cards must share to become one provider: the same
/// service, key and settings. Only the name, models and per-model facts may
/// differ.
fn merge_key(p: &ProviderProfile, secret: &Option<String>) -> String {
    let mut shared = p.clone();
    shared.id = String::new();
    shared.name = String::new();
    shared.models = Vec::new();
    shared.enabled_models = Vec::new();
    shared.model_ids = BTreeMap::new();
    shared.vision_models = Vec::new();
    shared.context_lengths = BTreeMap::new();
    shared.created_at = String::new();
    shared.base_url = p.base_url.trim().trim_end_matches('/').to_ascii_lowercase();
    format!(
        "{}\u{0}{}",
        serde_json::to_string(&shared).unwrap_or_default(),
        secret.as_deref().unwrap_or("")
    )
}

/// A card's old name without its model: `"OpenRouter (gpt-5)"` →
/// `"OpenRouter"`.
fn provider_name_of(card: &ProviderProfile) -> String {
    let name = card.name.trim();
    for model in &card.models {
        if let Some(rest) = name.strip_suffix(&format!("({model})")) {
            return rest.trim().to_string();
        }
    }
    name.to_string()
}

/// The service's own name. `None` for a custom server.
pub fn type_label(provider_type: &str) -> Option<&'static str> {
    Some(match provider_type {
        "openrouter" => "OpenRouter",
        "anthropic" => "Anthropic",
        "openai" => "OpenAI",
        "fireworks" => "Fireworks",
        "deepinfra" => "DeepInfra",
        "ollama" => "Ollama",
        _ => return None,
    })
}

/// Merge cards of the same service, key and settings into one provider.
/// `secrets[i]` is card `i`'s key. Every card keeps its id (now its model's
/// card id), so chats and schedules pinned to it are unaffected. Returns the
/// ids of the profiles merged away (their keys were copies). Pure.
pub fn merge_same_service(
    providers: &mut Vec<ProviderProfile>,
    secrets: &[Option<String>],
) -> Vec<String> {
    let mut out: Vec<ProviderProfile> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut names: Vec<Vec<String>> = Vec::new();
    let mut merged_away = Vec::new();
    for (p, secret) in providers.drain(..).zip(secrets) {
        let key = merge_key(&p, secret);
        match keys.iter().position(|k| *k == key) {
            Some(at) => {
                names[at].push(provider_name_of(&p));
                let into = &mut out[at];
                for model in &p.models {
                    if into.models.contains(model) {
                        continue;
                    }
                    into.models.push(model.clone());
                    if p.enabled_models.contains(model) {
                        into.enabled_models.push(model.clone());
                    }
                    if let Some(id) = p.model_ids.get(model) {
                        into.model_ids.insert(model.clone(), id.clone());
                    }
                    if p.vision_models.contains(model) {
                        into.vision_models.push(model.clone());
                    }
                    if let Some(n) = p.context_lengths.get(model) {
                        into.context_lengths.insert(model.clone(), *n);
                    }
                }
                merged_away.push(p.id.clone());
            }
            None => {
                keys.push(key);
                names.push(vec![provider_name_of(&p)]);
                out.push(p);
            }
        }
    }
    // The old card name, less its model; when merged cards were named
    // differently, the service's own name.
    for (p, names) in out.iter_mut().zip(names) {
        let first = names[0].clone();
        let same = names.iter().all(|n| *n == first);
        p.name = if same || type_label(&p.provider_type).is_none() {
            if first.is_empty() {
                p.name.clone()
            } else {
                first
            }
        } else {
            type_label(&p.provider_type).unwrap_or_default().to_string()
        };
    }
    *providers = out;
    merged_away
}

/// Settings layout: providers hold model lists (`upgrade_to_model_lists`).
pub const SCHEMA_MODEL_LISTS: u32 = 1;
/// Settings layout: cards of one service merged (`merge_same_service`).
pub const SCHEMA_MERGED: u32 = 2;

/// Merge the cards an upgrade left one per model into providers, once.
/// Runs at startup, before anything syncs the providers. Keys are compared,
/// so a key that cannot be read postpones the merge to the next launch
/// (nothing is lost meanwhile: every card already works on its own).
pub async fn migrate_merge_providers(app: &AppHandle) {
    let snapshot = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        if cfg.providers_schema >= SCHEMA_MERGED {
            return;
        }
        cfg.providers.clone()
    };
    let mut secrets = Vec::with_capacity(snapshot.len());
    for p in &snapshot {
        match get_secret_checked(&p.id).await {
            Ok(s) => secrets.push(s),
            Err(e) => {
                tracing::warn!("could not read the key for provider {}: {e}", p.id);
                return;
            }
        }
    }
    let merged_away = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        if cfg
            .providers
            .iter()
            .map(|p| &p.id)
            .ne(snapshot.iter().map(|p| &p.id))
        {
            // Changed meanwhile: try again next launch.
            return;
        }
        let mut providers = cfg.providers.clone();
        let merged_away = merge_same_service(&mut providers, &secrets);
        cfg.providers = providers;
        cfg.providers_schema = SCHEMA_MERGED;
        repoint_setups(&mut cfg);
        if let Err(e) = crate::config::save(&cfg) {
            tracing::warn!("could not save the merged providers: {e}");
            return;
        }
        merged_away
    };
    if !merged_away.is_empty() {
        tracing::info!(
            "merged {} model cards into their providers",
            merged_away.len()
        );
    }
    for id in merged_away {
        delete_secret(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with(id: &str, name: &str, models: &[&str]) -> ProviderProfile {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "provider_type": "custom_openai",
            "base_url": "http://box:8080", "models": models,
        }))
        .unwrap()
    }

    /// A config holding `providers`, each checked model a setup.
    fn with_setups(providers: Vec<ProviderProfile>) -> crate::config::Config {
        let mut cfg = crate::config::Config {
            providers,
            setups_migrated: false,
            ..Default::default()
        };
        migrate_to_setups(&mut cfg);
        cfg
    }

    fn typed(id: &str, provider_type: &str) -> ProviderProfile {
        let mut p: ProviderProfile = serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "provider_type": provider_type,
            "base_url": "http://box:8080", "models": ["m"],
        }))
        .unwrap();
        upgrade_to_model_lists(std::slice::from_mut(&mut p));
        p
    }

    /// Decision #65: a local profile is kept but disabled, and a local
    /// default is cleared and flagged so the user is asked for another.
    #[test]
    fn local_profiles_are_disabled_and_a_local_default_is_cleared() {
        let mut cfg = with_setups(vec![typed("loc", "local"), typed("or", "openrouter")]);
        cfg.active_provider_id = Some("loc".into());
        assert!(migrate_local_profiles(&mut cfg));
        assert_eq!(
            cfg.providers[0].disabled_reason.as_deref(),
            Some("unsupported")
        );
        assert!(cfg.providers[1].is_usable());
        assert_eq!(cfg.active_provider_id, None);
        assert!(cfg.needs_default_provider);
        assert!(!migrate_local_profiles(&mut cfg), "idempotent");
    }

    #[test]
    fn a_remote_default_survives_the_local_migration() {
        let mut cfg = with_setups(vec![typed("loc", "local"), typed("or", "openrouter")]);
        cfg.active_provider_id = Some("or".into());
        migrate_local_profiles(&mut cfg);
        assert_eq!(cfg.active_provider_id.as_deref(), Some("or"));
        assert!(!cfg.needs_default_provider);
    }

    #[test]
    fn a_provider_without_its_own_prompt_gets_the_default() {
        let mut p = typed("a", "openrouter");
        assert_eq!(system_prompt_for(&p), default_system_prompt());
        p.system_prompt = Some("  ".into());
        assert_eq!(
            system_prompt_for(&p),
            default_system_prompt(),
            "blank is not a prompt"
        );
        p.system_prompt = Some("Be terse.".into());
        assert_eq!(
            system_prompt_for(&p),
            format!("Be terse.\n\n{OUTPUTS_INSTRUCTION}"),
            "a custom prompt gets the outputs instruction too"
        );
    }

    #[test]
    fn the_outputs_instruction_is_added_once() {
        let once = with_outputs_instruction("Be terse.");
        assert_eq!(with_outputs_instruction(&once), once);
        assert_eq!(once.matches(OUTPUTS_INSTRUCTION).count(), 1);
        let mut p = typed("a", "openrouter");
        p.system_prompt = Some(once.clone());
        assert_eq!(system_prompt_for(&p), once);
        assert!(default_system_prompt().starts_with(DEFAULT_SYSTEM_PROMPT));
    }

    #[test]
    fn tool_support_prefers_the_override_then_assumes_yes() {
        let mut p = typed("a", "custom_openai");
        assert!(p.tools_supported(None));
        p.supports_tools = Some(false);
        assert!(!p.tools_supported(None));
    }

    #[test]
    fn an_old_card_becomes_a_provider_whose_model_keeps_the_card_id() {
        let mut providers = vec![
            profile_with("a", "Solo", &["m1"]),
            profile_with("b", "Box", &["x", "y"]),
        ];
        providers[0].supports_vision = true;
        providers[0].context_length = Some(32_768);
        upgrade_to_model_lists(&mut providers);
        let mut again = providers.clone();
        let cfg = with_setups(providers);
        let ids: Vec<String> = cfg.cards().iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids, ["a", "b", "b-m1"]);
        let a = cfg.card("a").unwrap();
        assert_eq!(a.models, ["m1"]);
        assert_eq!(a.provider_id.as_deref(), Some("a"));
        assert_eq!(a.secret_account(), "a");
        assert!(a.supports_vision, "the vision mark moved onto the model");
        assert_eq!(a.context_length, Some(32_768));
        assert_eq!(cfg.card("b-m1").unwrap().model(), "y");
        assert_eq!(cfg.card("b-m1").unwrap().secret_account(), "b");
        assert_eq!(cfg.providers[0].context_length, None);
        // Idempotent.
        let before = again[1].model_ids.clone();
        upgrade_to_model_lists(&mut again);
        assert_eq!(again[1].model_ids, before);
    }

    #[test]
    fn cards_of_one_service_and_key_merge_and_keep_their_ids() {
        let mut providers = vec![
            profile_with("p1", "OpenRouter (gpt-x)", &["gpt-x"]),
            profile_with("p2", "Box", &["local"]),
            profile_with("p3", "OpenRouter (claude-y)", &["claude-y"]),
            profile_with("p4", "OpenRouter (z)", &["z"]),
        ];
        for p in providers.iter_mut() {
            p.provider_type = "openrouter".into();
            p.base_url = "https://openrouter.ai/api/v1".into();
        }
        providers[1].provider_type = "custom_openai".into();
        upgrade_to_model_lists(&mut providers);
        let mut cfg = with_setups(providers);
        let mut providers = cfg.providers.clone();
        let secrets = [
            Some("k1".to_string()),
            None,
            Some("k1".to_string()),
            Some("k2".to_string()),
        ];
        let gone = merge_same_service(&mut providers, &secrets);
        assert_eq!(gone, ["p3"]);
        let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["p1", "p2", "p4"]);
        assert_eq!(
            providers[0].name, "OpenRouter",
            "both cards said OpenRouter"
        );
        assert_eq!(providers[0].models, ["gpt-x", "claude-y"]);
        assert_eq!(providers[0].model_ids["claude-y"], "p3");
        assert_eq!(providers[2].name, "OpenRouter");
        cfg.providers = providers;
        repoint_setups(&mut cfg);
        let p3 = cfg.card("p3").unwrap();
        assert_eq!(p3.secret_account(), "p1");
        assert_eq!(p3.model(), "claude-y");
    }

    #[test]
    fn different_settings_keep_cards_apart() {
        let mut providers = vec![
            profile_with("a", "A", &["m1"]),
            profile_with("b", "B", &["m2"]),
        ];
        providers[1].is_trusted = true;
        upgrade_to_model_lists(&mut providers);
        assert!(merge_same_service(&mut providers, &[None, None]).is_empty());
        assert_eq!(providers.len(), 2);
    }

    #[test]
    fn a_setup_on_an_unchecked_model_hands_the_default_on() {
        let mut p = profile_with("p", "P", &["a", "b"]);
        upgrade_to_model_lists(std::slice::from_mut(&mut p));
        let mut cfg = with_setups(vec![p]);
        cfg.active_provider_id = Some("p".into());
        cfg.providers[0].enabled_models = vec!["b".into()];
        assert!(!cfg.card("p").unwrap().is_usable(), "a is unchecked");
        assert!(cfg.repair_default(None));
        assert_eq!(cfg.active_provider_id.as_deref(), Some("p-m1"));
        assert!(!cfg.repair_default(None));
    }

    #[test]
    fn a_confirmed_window_is_remembered_per_model() {
        let mut p = profile_with("p", "P", &["a", "b"]);
        upgrade_to_model_lists(std::slice::from_mut(&mut p));
        let mut cfg = with_setups(vec![p]);
        assert!(cfg.set_card_context_length("p-m1", 200_000));
        assert!(!cfg.set_card_context_length("p-m1", 200_000));
        assert_eq!(cfg.card("p-m1").unwrap().context_length, Some(200_000));
        assert_eq!(cfg.card("p").unwrap().context_length, None);
        cfg.setups[1].context_length = Some(8_192);
        assert!(
            !cfg.set_card_context_length("p-m1", 1_000_000),
            "the user's wins"
        );
        assert_eq!(cfg.card("p-m1").unwrap().context_length, Some(8_192));
    }

    #[test]
    fn old_shape_provider_migrates_with_defaults() {
        // A profile written before Round-2 (no is_trusted / temperature / etc.)
        // must still deserialize, defaulting the new fields. Also carries the
        // since-removed `tools_enabled` (Round-7: dropped in favor of the
        // per-session chat/agentic toggle) to confirm a stale field is silently
        // ignored rather than erroring.
        let json = r#"{
            "id": "p1", "name": "Box", "provider_type": "ollama",
            "base_url": "http://localhost:11434", "models": ["llama3.2:3b"],
            "tools_enabled": true, "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let p: ProviderProfile = serde_json::from_str(json).unwrap();
        assert!(!p.is_trusted);
        assert_eq!(p.temperature, None);
        assert_eq!(p.top_p, None);
        assert_eq!(p.top_k, None);
        assert_eq!(p.min_p, None);
        assert_eq!(p.presence_penalty, None);
        assert_eq!(p.frequency_penalty, None);
        assert_eq!(p.max_tokens, None);
        assert_eq!(p.context_length, None);
        assert_eq!(p.models, vec!["llama3.2:3b"]);
        assert!(!p.supports_vision);
        assert_eq!(p.system_prompt, None);
        assert_eq!(p.prompt_idle_timeout_secs, None);
        assert_eq!(p.parallel_slots, None);
    }

    /// Phase 2b acceptance: an `ollama` profile saved while Kitty still
    /// managed an Ollama process must keep working afterwards, as a *remote*
    /// endpoint pointed at a server the user runs.
    ///
    /// The risk isn't deserialization — `provider_type` is an untyped
    /// `String`, so it was never going to fail to load. It's that the row
    /// becomes decorative: still listed, no longer routable. These assertions
    /// pin the two things that keep it live, both of which are easy to delete
    /// by accident while removing "the Ollama code": the profile still
    /// reaches BigTiny as an `openai_compat` provider, and its granular type
    /// still rides along as `provider_dialect`, which is the *only* channel
    /// telling the daemon to apply the self-hosted sampling floor and put
    /// `top_k`/`min_p` on the wire.
    #[test]
    fn a_legacy_ollama_profile_still_routes_after_managed_ollama_was_removed() {
        let json = r#"{
            "id": "p1", "name": "My Ollama", "provider_type": "ollama",
            "base_url": "http://192.168.1.50:11434", "models": ["qwen3.5:4b"],
            "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let p: ProviderProfile = serde_json::from_str(json).unwrap();
        assert_eq!(p.provider_type, "ollama");

        let (wire_type, base) = crate::bigtiny::providers::bigtiny_provider_target(&p);
        assert_eq!(wire_type, "openai_compat");
        assert_eq!(base, "http://192.168.1.50:11434");

        // Not "local": a server the user runs needs nothing of ours on disk,
        // so it must not make the app report a missing local model.
        assert_ne!(p.provider_type, "local");
    }
}
