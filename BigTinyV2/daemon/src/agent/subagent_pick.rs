//! Choosing which provider a delegate runs on.
//!
//! Pure: no router, no database, no clock. Every input is copied out of a
//! `ProviderEntry` by `ProviderRouter::subagent_candidates` and handed here, so
//! the whole policy — including the parts that are security decisions rather
//! than preferences — is unit-testable without a daemon.
//!
//! # The two rules that are not preferences
//!
//! **A delegate with no tools is worse than no delegate.** Every built-in
//! specialist's value is its tool calls; a host that cannot call tools does not
//! fail visibly, it returns a fluent, well-shaped, entirely fabricated report.
//! So `supports_tools` is a hard gate, not a score.
//!
//! **A denylisted model stays denied even at the end of the chain.** The last
//! step before refusing is "run on the parent's own provider" — and the parent's
//! model is exactly the expensive one a user is trying to keep delegates off. A
//! denylist consulted only while scoring would pass every other test and be
//! bypassed by the one step that most needs it.
//!
//! # Why sharing a KV slot outranks cost
//!
//! There is no deadlock in running a delegate on the parent's own llama.cpp
//! server: `PermitStream` releases the provider permit when the stream ends, and
//! tool execution happens after that, so a parent waiting on `call_specialist`
//! holds nothing. The delegate simply queues.
//!
//! The cost is the KV cache. On `llama-server --parallel 1` there is one slot,
//! so the delegate's prompt evicts the parent's cached prefix and the parent
//! re-prefills its **entire, growing** transcript on its next step — and on
//! every step after every delegate call. That is paid by the parent, repeatedly,
//! and on a large local model it can dominate the turn. So the rule is not
//! "avoid local hosts", it is "avoid sharing one slot with the parent": a
//! different server, or the same server with `--parallel >= 2` and `id_slot`
//! pinning, costs nothing.

/// What the router knows about one provider, as far as this decision cares.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub provider_id: String,
    /// The provider's own configured model, used when no pin applies.
    pub model: String,
    pub dialect: String,
    pub healthy: bool,
    pub supports_tools: bool,
    /// Effective slot count — configured, probed, or the dialect's default.
    pub concurrency: u32,
    /// True when the count above is a guess rather than something configured or
    /// measured. Ollama is permanently in this state: its concurrency is
    /// `OLLAMA_NUM_PARALLEL`, which it does not expose.
    pub concurrency_is_guess: bool,
    pub context_length: Option<i32>,
    /// `"preferred" | "allowed" | "never"`, set by the user in Kitty.
    pub subagent_role: Option<String>,
    /// `"economy" | "moderate" | "premium"`, folded from Kitty's OpenRouter
    /// catalog. `None` for most self-hosted profiles, which is the common case.
    pub cost_tier: Option<String>,
    /// 0-100, likewise from Kitty. `None` is common and must not be penalised
    /// into last place.
    pub capability_rank: Option<i32>,
    pub fallback_priority: i32,
    /// Whether this is the provider the calling turn is itself running on.
    pub is_parent_provider: bool,
}

/// The outcome, including *why* — a caller has to be able to explain a
/// degraded or refused run to both the user and the model.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// A host was chosen. `note` is `Some` when the choice is worth mentioning
    /// (a dropped pin, or falling back to the parent's own model).
    Chosen {
        provider_id: String,
        model: Option<String>,
        note: Option<String>,
    },
    Refused {
        reason: String,
    },
}

/// A model pin is only valid for the provider it was pinned to.
pub type Pin<'a> = Option<(&'a str, Option<&'a str>)>;

/// Smallest context window a delegate can do useful work in. Below this there
/// is no room for a prompt, tool results and a structured report together.
const MIN_CONTEXT: i32 = 16_384;

/// The denylist pattern `model` matches, if any.
///
/// Patterns are matched case-insensitively, and a trailing `*` makes a prefix —
/// enough to write `claude-fable-*` without pulling in a glob crate, and enough
/// that a user denying a family does not have to enumerate its versions.
///
/// Returns the pattern rather than a bool because every refusal this produces
/// has to name the setting that caused it. A user who has denied their only
/// model needs that sentence, not "no specialist available", and a caller that
/// only knows *that* it was denied cannot write it.
pub fn denied_by<'a>(model: &str, deny: &'a [String]) -> Option<&'a str> {
    let model = model.trim().to_ascii_lowercase();
    deny.iter().find(|pat| {
        let pat = pat.trim().to_ascii_lowercase();
        if pat.is_empty() {
            false
        } else if let Some(prefix) = pat.strip_suffix('*') {
            model.starts_with(prefix)
        } else {
            model == pat
        }
    })
    .map(String::as_str)
}

/// True when `model` matches any denylist pattern.
pub fn is_denied(model: &str, deny: &[String]) -> bool {
    denied_by(model, deny).is_some()
}

/// Whether a candidate could host a delegate at all, ignoring preference.
fn hard_gate(c: &Candidate, deny: &[String]) -> Option<&'static str> {
    if c.subagent_role.as_deref() == Some("never") {
        return Some("it is set to never host subagents");
    }
    if !c.supports_tools {
        return Some("it cannot call tools");
    }
    if !c.healthy {
        return Some("it is unhealthy");
    }
    if c.context_length.is_some_and(|n| n < MIN_CONTEXT) {
        return Some("its context window is too small");
    }
    if is_denied(&c.model, deny) {
        return Some("its model is on the subagent denylist");
    }
    None
}

/// True when picking this host would make the parent re-prefill its transcript.
///
/// A *guessed* slot count on the parent's own provider counts as sharing. The
/// guess is the dialect's default, not a measurement — Ollama is permanently in
/// this state, since its concurrency is `OLLAMA_NUM_PARALLEL` and it does not
/// expose it. Guessing high and being wrong costs the parent a full re-prefill
/// on every step for the rest of the turn; guessing low and being wrong costs
/// one delegate a slightly worse host. The asymmetry decides it.
fn shares_one_slot(c: &Candidate) -> bool {
    c.is_parent_provider && (c.concurrency <= 1 || c.concurrency_is_guess)
}

/// Higher is better. Ordered by the precedence the module doc explains.
fn score(c: &Candidate) -> (i32, i32, i32, i32, i32, i32) {
    let preferred = i32::from(c.subagent_role.as_deref() == Some("preferred"));
    // The parent's own single-slot server is the expensive case; a *different*
    // single-slot server costs the parent nothing.
    let not_sharing = i32::from(!shares_one_slot(c));
    // Credited only when the count is configured or probed. An unprobed default
    // is not evidence of spare capacity, and treating it as such is how a
    // one-slot server that never announced itself wins on the strength of a
    // number nobody checked.
    let parallel = i32::from(c.concurrency >= 2 && !c.concurrency_is_guess);
    let cheap = match c.cost_tier.as_deref() {
        Some("economy") => 2,
        // `None` sits above "premium" but below "economy": an unknown price is
        // not a reason to prefer a model we know is expensive, nor to rule out
        // the self-hosted profiles that make up most of an unknown set.
        Some("moderate") | None => 1,
        _ => 0,
    };
    let capability = c.capability_rank.unwrap_or(50);
    // Lower `fallback_priority` wins, so negate to keep "higher is better".
    (
        preferred,
        not_sharing,
        parallel,
        cheap,
        capability,
        -c.fallback_priority,
    )
}

/// What became of a pin, in enough detail to explain it.
///
/// One function rather than a check at the point of use plus a second at the
/// point of reporting: those two drifted apart, and the shape of the bug was a
/// pin silently dropped with no note explaining why.
///
/// The two failure modes are kept apart because they are not the same event and
/// do not read the same to a user. A pinned *provider* that is gone means the
/// run moved somewhere else. A pinned *model* that is denied while its provider
/// is still fine means the run stayed exactly where it was asked to and only
/// the model changed — reporting that as "the pinned host is unavailable, so
/// this ran on <the pinned host> instead" is a sentence that contradicts itself.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PinStatus {
    /// No pin was given.
    Absent,
    /// Honour it exactly as written.
    Usable,
    /// The provider itself cannot host a delegate. Provider and model both go.
    ProviderIneligible,
    /// The provider is fine; the pinned model is not. Only the model goes.
    ModelDenied,
}

fn pin_status(candidates: &[Candidate], pin: Pin<'_>, deny: &[String]) -> PinStatus {
    let Some((provider, model)) = pin else {
        return PinStatus::Absent;
    };
    let Some(c) = candidates.iter().find(|c| c.provider_id == provider) else {
        return PinStatus::ProviderIneligible;
    };
    if hard_gate(c, deny).is_some() {
        return PinStatus::ProviderIneligible;
    }
    // The pin's model is what would actually be sent, so it is what is judged.
    match model {
        Some(m) if is_denied(m, deny) => PinStatus::ModelDenied,
        _ => PinStatus::Usable,
    }
}

/// Pick a host for a delegate.
///
/// `deny` is applied to every candidate *and* to the pin, so no path through
/// this function can return a denylisted model.
pub fn choose_host(candidates: &[Candidate], pin: Pin<'_>, deny: &[String]) -> Decision {
    let pin_state = pin_status(candidates, pin, deny);
    // 1. An explicit pin, if it is still eligible. An explicit statement
    //    outranks scoring — the same rule `resolve_provider_for_app` applies.
    if pin_state == PinStatus::Usable {
        let (provider, model) = pin.expect("Usable implies a pin is present");
        return Decision::Chosen {
            provider_id: provider.to_string(),
            model: model.map(str::to_string),
            note: None,
        };
    }
    // Only the *model* was denied. The provider was chosen deliberately and is
    // still eligible, so stay on it and let it serve its own configured model —
    // moving the run somewhere else would answer a question nobody asked. This
    // is the reason the two failure modes are separate states rather than one
    // "pin dropped" flag.
    if pin_state == PinStatus::ModelDenied {
        let (provider, model) = pin.expect("a status implies a pin");
        return Decision::Chosen {
            provider_id: provider.to_string(),
            model: None,
            note: Some(format!(
                "the pinned model {} is on the subagent denylist, so this ran on {provider}'s \
                 own model instead",
                model.unwrap_or("(unnamed)")
            )),
        };
    }

    // Otherwise the pin is dropped whole — provider *and* model. The model was
    // chosen for that provider, and applying it to a substitute is how an
    // Anthropic model id ends up at a llama-server.

    let eligible: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| hard_gate(c, deny).is_none())
        .collect();

    if eligible.is_empty() {
        // Name the reason rather than saying "none available". A user who
        // denied their only model, or marked their only tool-capable provider
        // "never", needs to know which setting did it.
        let reason = match candidates.len() {
            0 => "no provider is configured".to_string(),
            _ => {
                let mut whys: Vec<String> = candidates
                    .iter()
                    .filter_map(|c| {
                        hard_gate(c, deny).map(|w| format!("{}: {w}", c.provider_id))
                    })
                    .collect();
                whys.sort();
                whys.dedup();
                format!(
                    "no provider can host a specialist ({}). Check Settings → Providers.",
                    whys.join("; ")
                )
            }
        };
        return Decision::Refused { reason };
    }

    // 2 & 3. Best eligible, preferring one that is not the parent's. Expressed
    //        as a score component rather than two passes so a `preferred`
    //        marking can still win over "not the parent's".
    let best = eligible
        .iter()
        .max_by_key(|c| score(c))
        .expect("non-empty");

    let mut notes: Vec<String> = Vec::new();
    if pin_state == PinStatus::ProviderIneligible {
        let (p, m) = pin.expect("a status implies a pin");
        let named = match m {
            Some(m) => format!("{p} ({m})"),
            None => p.to_string(),
        };
        notes.push(format!(
            "the pinned host {named} is unavailable, so this ran on {} instead",
            best.provider_id
        ));
    }
    // 4. The parent's own provider is a real answer, but never a silent one.
    if best.is_parent_provider {
        notes.push(
            "no separate subagent host was available, so this ran on your main model".to_string(),
        );
        // Named separately from the line above, because the cost is not "you
        // paid for your main model" — it is that the parent now re-prefills its
        // whole transcript on every step after this call.
        if shares_one_slot(best) {
            notes.push(format!(
                "{} appears to have one generation slot, so this run evicted the \
                 conversation's cached context",
                best.provider_id
            ));
        }
    }

    Decision::Chosen {
        provider_id: best.provider_id.clone(),
        // No model override: a scored host runs its own configured model. The
        // only model this function ever returns is one that was pinned *to that
        // provider* and survived the gates.
        model: None,
        note: (!notes.is_empty()).then(|| notes.join("; ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str) -> Candidate {
        Candidate {
            provider_id: id.to_string(),
            model: format!("{id}-model"),
            dialect: "openai".into(),
            healthy: true,
            supports_tools: true,
            concurrency: 8,
            concurrency_is_guess: false,
            context_length: Some(128_000),
            subagent_role: None,
            cost_tier: None,
            capability_rank: None,
            fallback_priority: 0,
            is_parent_provider: false,
        }
    }

    fn chosen(d: &Decision) -> &str {
        match d {
            Decision::Chosen { provider_id, .. } => provider_id,
            Decision::Refused { reason } => panic!("expected a host, got refusal: {reason}"),
        }
    }

    /// A pinned provider whose *model* is denied is not a reason to move the
    /// run. The provider was chosen deliberately and is still eligible; only
    /// the model has to change.
    ///
    /// The bug this pins down produced a self-contradicting sentence — "the
    /// pinned host P (M) is unavailable, so this ran on P instead" — because
    /// one flag stood for two different events.
    #[test]
    fn a_denied_pin_model_keeps_its_provider_and_says_only_the_model_moved() {
        let mut a = candidate("a");
        a.model = "a-cheap".into();
        let c = vec![a, candidate("b")];
        let deny = vec!["expensive-*".to_string()];

        let d = choose_host(&c, Some(("a", Some("expensive-one"))), &deny);
        let Decision::Chosen {
            provider_id,
            model,
            note,
        } = d
        else {
            panic!("expected a host");
        };
        assert_eq!(provider_id, "a", "the provider pin still stands");
        assert_eq!(model, None, "but its own model is used, not the denied one");
        let note = note.expect("a dropped model must be explained");
        assert!(note.contains("expensive-one"), "got: {note}");
        assert!(note.contains("denylist"), "got: {note}");
        assert!(
            !note.contains("unavailable"),
            "the host was available; only the model was denied: {note}"
        );
    }

    /// A slot count nobody measured is not evidence of spare capacity. Ollama
    /// is permanently in this state, and crediting its default is how the
    /// parent's own box wins on a number that was never checked.
    #[test]
    fn a_guessed_slot_count_earns_no_parallelism_credit() {
        let mut parent = candidate("local");
        parent.is_parent_provider = true;
        parent.concurrency = 8;
        parent.concurrency_is_guess = true;

        let mut other = candidate("other");
        other.concurrency = 2;

        assert_eq!(
            chosen(&choose_host(&[parent.clone(), other], None, &[])),
            "other",
            "a measured 2 beats a guessed 8 on the parent's own server"
        );

        // ...and when it is the only option it is still chosen, with the cost
        // named rather than hidden.
        let d = choose_host(&[parent], None, &[]);
        assert_eq!(chosen(&d), "local");
        let Decision::Chosen { note: Some(n), .. } = d else {
            panic!("running on the parent must be explained");
        };
        assert!(n.contains("one generation slot"), "got: {n}");
    }

    /// Every refusal has to name the setting that caused it, which a bool
    /// cannot do.
    #[test]
    fn denied_by_returns_the_pattern_that_matched() {
        let deny = vec!["claude-fable-*".to_string(), "gpt-5-pro".to_string()];
        assert_eq!(denied_by("Claude-Fable-5-1", &deny), Some("claude-fable-*"));
        assert_eq!(denied_by("gpt-5-pro", &deny), Some("gpt-5-pro"));
        assert_eq!(denied_by("claude-haiku-4-5", &deny), None);
    }

    /// The common case: no Kitty hints at all. Most self-hosted profiles will
    /// never match the OpenRouter catalog, so this is what the picker actually
    /// runs on — not a degenerate case to handle afterwards.
    #[test]
    fn picks_a_host_with_no_cost_or_capability_hints() {
        let c = vec![candidate("a"), candidate("b")];
        assert!(matches!(
            choose_host(&c, None, &[]),
            Decision::Chosen { .. }
        ));
    }

    #[test]
    fn a_tool_incapable_host_is_refused_not_ranked_last() {
        let mut only = candidate("local");
        only.supports_tools = false;
        match choose_host(&[only], None, &[]) {
            Decision::Refused { reason } => assert!(reason.contains("cannot call tools"), "{reason}"),
            other => panic!("a delegate with no tools fabricates its report: {other:?}"),
        }
    }

    /// The load-bearing ordering test: the denylist must survive all the way to
    /// the parent-fallback step, which is the one that would otherwise use the
    /// expensive model the user was trying to avoid.
    #[test]
    fn a_denylisted_model_is_refused_even_as_the_parents_own_fallback() {
        let mut parent = candidate("anthropic");
        parent.model = "claude-fable-5-1".into();
        parent.is_parent_provider = true;

        let deny = vec!["claude-fable-*".to_string()];
        match choose_host(&[parent], None, &deny) {
            Decision::Refused { reason } => {
                assert!(reason.contains("denylist"), "{reason}");
                assert!(reason.contains("Settings"), "must say where to change it: {reason}");
            }
            other => panic!("the denylist was bypassed by the parent fallback: {other:?}"),
        }
    }

    /// Denying a model must not deny its siblings on the same provider.
    #[test]
    fn the_denylist_is_per_model_not_per_provider() {
        let mut expensive = candidate("anthropic-big");
        expensive.model = "claude-fable-5-1".into();
        let mut cheap = candidate("anthropic-small");
        cheap.model = "claude-haiku-4-5".into();

        let deny = vec!["claude-fable-*".to_string()];
        let d = choose_host(&[expensive, cheap], None, &deny);
        assert_eq!(chosen(&d), "anthropic-small");
    }

    #[test]
    fn a_pinned_but_denylisted_model_does_not_smuggle_past_the_gate() {
        let host = candidate("anthropic");
        let deny = vec!["claude-fable-*".to_string()];
        // The pin names a denied model even though the provider's own default
        // is fine — the pin is what would actually be sent.
        let d = choose_host(&[host], Some(("anthropic", Some("claude-fable-5-1"))), &deny);
        match d {
            Decision::Chosen { model, note, .. } => {
                assert_eq!(model, None, "the denied pin must be dropped");
                assert!(note.is_some(), "dropping a pin must be reported");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// A model pin belongs to its provider. Carrying it onto a substitute is
    /// how an Anthropic model id reaches a llama-server.
    #[test]
    fn an_ineligible_pin_drops_the_model_with_the_provider() {
        let mut pinned = candidate("gone");
        pinned.healthy = false;
        let other = candidate("other");

        let d = choose_host(&[pinned, other], Some(("gone", Some("some-model"))), &[]);
        match d {
            Decision::Chosen {
                provider_id,
                model,
                note,
            } => {
                assert_eq!(provider_id, "other");
                assert_eq!(model, None, "the pin's model must not follow it");
                assert!(note.unwrap().contains("unavailable"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Not "avoid local" — avoid *sharing one slot with the parent*. A second
    /// single-slot server is free as far as the parent's KV cache is concerned.
    #[test]
    fn a_single_slot_host_is_avoided_only_when_it_is_the_parents_own() {
        let mut parent = candidate("llama-parent");
        parent.concurrency = 1;
        parent.is_parent_provider = true;
        let mut other = candidate("llama-other");
        other.concurrency = 1;

        let d = choose_host(&[parent.clone(), other], None, &[]);
        assert_eq!(chosen(&d), "llama-other");

        // With only the parent available it is still chosen — degrading beats
        // refusing, since a refused delegate means the parent does the work
        // inline in its own context.
        let d = choose_host(&[parent], None, &[]);
        assert_eq!(chosen(&d), "llama-parent");
        match d {
            Decision::Chosen { note, .. } => {
                assert!(note.unwrap().contains("your main model"))
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn an_explicit_preference_outranks_cost_and_capability() {
        let mut preferred = candidate("mine");
        preferred.subagent_role = Some("preferred".into());
        preferred.cost_tier = Some("premium".into());
        preferred.capability_rank = Some(10);

        let mut cheap = candidate("cheap");
        cheap.cost_tier = Some("economy".into());
        cheap.capability_rank = Some(90);

        let d = choose_host(&[preferred, cheap], None, &[]);
        assert_eq!(chosen(&d), "mine");
    }

    #[test]
    fn economy_wins_when_nothing_else_separates_two_hosts() {
        let mut premium = candidate("premium");
        premium.cost_tier = Some("premium".into());
        let mut economy = candidate("economy");
        economy.cost_tier = Some("economy".into());

        let d = choose_host(&[premium, economy], None, &[]);
        assert_eq!(chosen(&d), "economy");
    }

    #[test]
    fn a_context_window_too_small_for_a_delegate_is_a_hard_gate() {
        let mut tiny = candidate("tiny");
        tiny.context_length = Some(4_096);
        match choose_host(&[tiny], None, &[]) {
            Decision::Refused { reason } => assert!(reason.contains("context"), "{reason}"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// A user who forbids their only host should be told which setting did it.
    #[test]
    fn a_never_marking_refuses_by_name() {
        let mut only = candidate("only");
        only.subagent_role = Some("never".into());
        match choose_host(&[only], None, &[]) {
            Decision::Refused { reason } => {
                assert!(reason.contains("never host subagents"), "{reason}")
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn denylist_patterns_match_exactly_or_by_prefix() {
        let deny = vec!["claude-fable-*".into(), "gpt-5-pro".into()];
        assert!(is_denied("claude-fable-5-1", &deny));
        assert!(is_denied("CLAUDE-FABLE-5-1", &deny));
        assert!(is_denied("gpt-5-pro", &deny));
        assert!(!is_denied("gpt-5-pro-mini", &deny), "exact means exact");
        assert!(!is_denied("claude-haiku-4-5", &deny));
        assert!(!is_denied("anything", &[]));
    }
}
