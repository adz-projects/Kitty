// Shared TS types. Mirror the Rust structs by hand; keep in sync.
// Pairs:
//   Config / NotificationPrefs  <-> src-tauri/src/config/mod.rs
//   StackStatus <-> src-tauri/src/state.rs; StackStatusPayload <-> src-tauri/src/lifecycle/health.rs
//   StartupPhase <-> src-tauri/src/state.rs; StartupPhasePayload <-> src-tauri/src/lifecycle/mod.rs

export interface NotificationPrefs {
  task_complete: boolean;
  approval_needed: boolean;
  task_failed: boolean;
  stack_degraded: boolean;
}

export interface Config {
  hotkeys: string[];
  clipboard_hotkey: string | null;
  open_window_hotkey: string | null;
  default_context_folder: string | null;
  ollama_base_url: string;
  setup_completed: boolean;
  theme: string;
  background_image: string | null;
  background_dim: number;
  background_position_x: number;
  background_position_y: number;
  background_size: 'cover' | 'contain' | 'stretch' | 'center';
  notifications: NotificationPrefs;
  remember_overlay_position: boolean;
  providers: ProviderProfile[];
  active_provider_id: string | null;
  strict_remote_mode: boolean;
  show_artifacts: boolean;
  /** Whether Kitty spawns/supervises the Adaptive Pathway extension's HTTP
      sidecar (off by default — a separate Python process the user installs). */
  adaptive_pathway_enabled: boolean;
  adaptive_pathway_launch_command: string;
  adaptive_pathway_launch_args: string[];
  adaptive_pathway_db_path: string;
  adaptive_pathway_port: number;
  /** Ollama model tag used for adaptive-pathway's context embeddings — one
      pinned tag shared by every user regardless of chat provider, so learned
      vectors live in the same space. */
  adaptive_pathway_embedding_model: string;
  /** Whether local inference (Ollama) is in play for this install — set by
      the wizard's first-screen fork, toggleable later from Advanced. */
  ollama_enabled: boolean;
}

// --- Providers (Phase 5) ---
export type NetworkTier = 'local' | 'personal' | 'remote';
export type ProviderType = 'ollama' | 'openrouter' | 'anthropic' | 'openai' | 'custom_openai';

export interface ProviderProfile {
  id: string;
  name: string;
  provider_type: ProviderType;
  base_url: string;
  models: string[];
  is_trusted: boolean;
  temperature: number | null;
  top_p: number | null;
  context_length: number | null;
  /** STOPGAP client-side workaround (see chatStore.ts's send()) — Goose has no
      native hook for this yet; remove once block/goose#7617 or equivalent lands. */
  strip_reasoning: boolean;
  /** Custom system prompt; `null` = use the built-in mode-appropriate default
      (see system_prompts.ts). STOPGAP-adjacent — prepended client-side to a
      session's first message (chatStore.ts's send()), since Goose's ACP has no
      system-prompt param it honors. */
  system_prompt: string | null;
  /** Override for how long `session/prompt` tolerates silence before giving
      up, in seconds (default 300 when `null`). Raise this for a provider known
      to have long gaps between streamed updates (e.g. a slow Tailscale-hosted
      host); lower it if a long silence there reliably means "stuck" and
      waiting 5 minutes to find out is worse than a false-positive retry. */
  prompt_idle_timeout_secs: number | null;
  created_at: string;
}

/** ProviderProfile flattened + derived fields (mirrors Rust ProviderView). */
export interface ProviderView extends ProviderProfile {
  network_tier: NetworkTier;
  has_secret: boolean;
  active: boolean;
}

/** `GET /api/v1/key`'s `data` object — only the fields Kitty's UI reads are
    named; the rest (byok_usage*, include_byok_in_limit, ...) pass through
    unread rather than being modeled in full. */
export interface OpenRouterCredits {
  label: string;
  limit: number | null;
  limit_remaining: number | null;
  usage: number;
  is_free_tier: boolean;
  [key: string]: unknown;
}

/** A file attached to a chat (Round-2 item 13): UTF-8 text, or a base64 data URL. */
export interface FileAttachment {
  name: string;
  kind: 'text' | 'binary';
  content: string;
  mime: string | null;
}

/** App-side chat-folder state (Round-2 item 15): folder list + session→folder map. */
export interface FolderData {
  folders: string[];
  assignments: Record<string, string>;
}

/** Mirrors `config::scheduled_tasks::Schedule` — a Rust internally-tagged
    enum, so the discriminant lives in `kind`. */
export type Schedule = { kind: 'one_shot' } | { kind: 'recurring'; interval_secs: number };

/** Mirrors `config::scheduled_tasks::ScheduledTask`. `next_fire` is an ISO
    8601 string (chrono's default `DateTime<Local>` serde representation). */
export interface ScheduledTask {
  id: string;
  name: string;
  prompt: string;
  cwd: string | null;
  schedule: Schedule;
  next_fire: string;
  enabled: boolean;
}

export interface OllamaModel {
  name: string;
  size: number;
  modified_at: string;
  details?: { parameter_size?: string; quantization_level?: string };
}

/** Mirrors `config::recipes::ParameterInputType`. */
export type ParameterInputType = 'string' | 'number' | 'boolean' | 'date' | 'file' | 'select';

/** Mirrors `config::recipes::ParameterRequirement`. `user_prompt` is the one
    parameter (at most one per recipe) whose value comes from whatever the
    user typed after `/slug` — see `src/lib/recipes.ts`'s `primaryParameter`. */
export type ParameterRequirement = 'required' | 'optional' | 'user_prompt';

/** Mirrors `config::recipes::RecipeParameter`. For the `user_prompt`
    parameter, `description` is user-facing invocation guidance shown in the
    slash-autocomplete dropdown and as the composer's placeholder — not just
    schema metadata. */
export interface RecipeParameter {
  key: string;
  input_type: ParameterInputType;
  requirement: ParameterRequirement;
  description: string;
  default: string | null;
  options: string[];
}

/** Known real-schema extension types. Only `builtin`/`platform`/`stdio` have
    an ACP equivalent Kitty can add to a live session (see
    `add_recipe_extension`); the rest are stored for YAML round-trip fidelity
    and silently skipped at launch. */
export type RecipeExtensionType =
  'stdio' | 'builtin' | 'platform' | 'streamable_http' | 'frontend' | 'inline_python';

/** Mirrors `config::recipes::RecipeExtension`. Extra real-schema fields
    Kitty doesn't specifically interpret pass through via an index signature
    (Rust's `#[serde(flatten)] extra: HashMap<...>`). */
export interface RecipeExtension {
  type: RecipeExtensionType;
  name: string;
  cmd?: string | null;
  args: string[];
  env_keys: string[];
  description?: string | null;
  timeout?: number | null;
  bundled?: boolean | null;
  [extra: string]: unknown;
}

/** Mirrors `config::recipes::Recipe`. A recipe is a client-side-interpreted
    template (Kitty attaches its `instructions`/`prompt`/`extensions` to an
    ordinary chat turn rather than shelling out to the real `goose run
    --recipe` CLI runner — see `docs/BACKLOG.md`'s now-resolved recipes entry
    and `chatStore.ts`'s `sendWithRecipe`). Still mirrors the real, portable
    Goose recipe YAML schema field-for-field so it round-trips through
    import/export as a real `.yaml` file — only `id`/`slug`/`is_builtin`/
    `created_at`/`max_reasoning_tokens` are Kitty-only, stripped on export. */
export interface Recipe {
  id: string;
  slug: string;
  title: string;
  description: string;
  instructions: string | null;
  prompt: string | null;
  version: string;
  parameters: RecipeParameter[];
  extensions: RecipeExtension[];
  activities: string[];
  is_builtin: boolean;
  created_at: string;
  /** Hard cap on how much a recipe-invoked turn is allowed to reason before
      Kitty auto-cancels it — see `chatStore.ts`'s `activeRecipeTurn`. Not
      part of the real Goose recipe schema (no ACP-exposed numeric reasoning-
      token config exists to query per model, only effort levels), so this is
      excluded from YAML export. Defaults to 2048. */
  max_reasoning_tokens: number;
}

/** Fields supplied when creating/updating a recipe — everything except the
    Kitty-only bookkeeping the backend owns. Mirrors Rust `RecipeInput`. */
export interface RecipeInput {
  slug: string;
  title: string;
  description: string;
  instructions: string | null;
  prompt: string | null;
  parameters: RecipeParameter[];
  extensions: RecipeExtension[];
  activities: string[];
  max_reasoning_tokens: number;
}

/** Mirrors Rust `RecipeImportResult` — a successfully-imported recipe plus
    any non-fatal warnings (e.g. a dropped `settings`/`response`/`retry`/
    `sub_recipes` block real Goose recipes can carry but Kitty can't apply). */
export interface RecipeImportResult {
  recipe: Recipe;
  warnings: string[];
}

/** Mirrors Rust `log_capture::LogEntry` — one captured WARN/ERROR tracing
    event, powering Settings → Advanced's error log. */
export interface LogEntry {
  timestamp: string;
  level: string;
  target: string;
  message: string;
}

export interface PullProgress {
  pull_id: string;
  model: string;
  status: string;
  total?: number;
  completed?: number;
  done: boolean;
  error?: string;
}

export interface EnvVar {
  name: string;
  value: string | null;
}

/** A BigTiny MCP server registration — daemon-global, live over REST (no
    restart to add/edit/delete/toggle). `streamable_http` is the MCP spec's
    successor to the old two-endpoint `sse` transport: a single POST endpoint,
    response framed as plain JSON or one SSE `data:` frame. `headers` carries
    auth for either remote transport (e.g. `{"Authorization": "Bearer ..."}`)
    — `stdio` never uses `url`/`headers`. */
export interface McpServer {
  id: string;
  name: string;
  transport: 'stdio' | 'sse' | 'streamable_http';
  command: string | null;
  args: string[];
  url: string | null;
  env: Record<string, string>;
  headers: Record<string, string>;
  enabled: boolean;
  status: 'connected' | 'disconnected' | 'error';
  error_message: string | null;
}

/** Input to `ipc.addMcpServer` — mirrors `McpServer` minus the server-assigned
    id/status fields. */
export interface McpServerSpec {
  name: string;
  transport: 'stdio' | 'sse' | 'streamable_http';
  command?: string | null;
  args?: string[];
  url?: string | null;
  env?: Record<string, string>;
  headers?: Record<string, string>;
  enabled?: boolean;
}

/** Input to `ipc.updateMcpServer` — every field optional, only what's set is
    changed. */
export interface McpServerPatch {
  name?: string;
  transport?: 'stdio' | 'sse' | 'streamable_http';
  command?: string | null;
  args?: string[];
  url?: string | null;
  env?: Record<string, string>;
  headers?: Record<string, string>;
  enabled?: boolean;
}

export interface SettingsTarget {
  section: string;
  highlight: string | null;
}

// --- Wizard (Phase 7) ---
export interface DepStatus {
  installed: boolean;
  version: string | null;
  path: string | null;
  latest_version: string | null;
  is_outdated: boolean | null;
}

export interface Detection {
  ollama: DepStatus;
}

/** Result of `validate_setup` — powers the wizard's Done-step summary/soft
    Finish gate and Setup & Repair's lighter re-check. */
export interface SetupValidation {
  ready: boolean;
  issues: string[];
  adaptive_pathway_ok: boolean;
}

// Serde `rename_all = "snake_case"` on the Rust enum.
export type StackStatus =
  | 'starting'
  | 'ok'
  | 'ollama_down'
  | 'backend_down'
  | 'no_model'
  | 'provider_unreachable';

export interface StackStatusPayload {
  status: StackStatus;
  detail: string | null;
}

// One-time startup progress, separate from StackStatus (which is a
// steady-state health readout and has no concept of "spawning"/"warming").
// `ready` has no further bearing on chat availability once reached.
export type StartupPhase = 'spawning_backend' | 'warming_model' | 'ready';

export interface StartupPhasePayload {
  phase: StartupPhase;
}

// --- Adaptive Pathway extension sidecar (kept separate from StackStatus —
// this is an optional augmentation, not a chat-blocking dependency). ---
export type AdaptivePathwayStatus = 'disabled' | 'starting' | 'ok' | 'down';

export interface AdaptivePathwayStatusPayload {
  status: AdaptivePathwayStatus;
}

/** Only emitted when `schism_state` flips into `detected`/`reviewing`. */
export interface AdaptivePathwaySchismPayload {
  state: string;
}

/** Readiness of the shared `qwen3-embedding:0.6b` Ollama model. Separate from
    `AdaptivePathwayStatus`: the sidecar can be `ok` (reachable, serving hints)
    while this is `downloading`/`missing` — degrades to the hashing fallback
    rather than reading as an outage. */
export type EmbeddingModelStatus = 'unknown' | 'present' | 'downloading' | 'missing';

export interface AdaptivePathwayEmbeddingStatusPayload {
  status: EmbeddingModelStatus;
}

/** `GET /edges/{edge_id}` result — the "why was this suggested" detail. */
export interface AdaptivePathwayEdge {
  id: string;
  semantic_primitive: string;
  domain_id: string;
  confidence: number;
  status: string;
  tier: string;
}

/** `GET /schism` result when a schism is active (`{state:"none"}` otherwise). */
export interface AdaptivePathwaySchismAlert {
  state: string;
  faction_a: number[];
  faction_b: number[];
  within_a: number;
  within_b: number;
  between: number;
  faction_a_models: number;
  faction_b_models: number;
  detected_at: string | null;
}

export interface AdaptivePathwayEnsembleWeights {
  ig_weight_min: number;
  ig_weight_max: number;
  pc_weight: number;
}

/** `GET /state` result — loosely typed (only the keys Kitty's UI actually
    reads are named; the rest pass through as unknown). */
/** Which embedding backend context vectors are actually coming from right
    now — distinct from `EmbeddingModelStatus` (whether the tag is installed
    in Ollama): this reflects whether the sidecar's `EmbeddingProvider` has
    actually succeeded against it. `'untried'` means no `decide`/
    `record_outcome`/`record_annotation` call carrying a `context` has fired
    yet this process lifetime. */
export interface AdaptivePathwayEmbeddingInfo {
  backend: 'ollama' | 'hashing' | 'untried';
  model: string;
  url: string;
}

export interface AdaptivePathwayState {
  schism_state: string;
  ensemble_weights: AdaptivePathwayEnsembleWeights;
  warm_ready: boolean;
  feature_utilization: number;
  feature_collision_rate: number;
  plateau_risk_score: number;
  domain_count: number;
  embedding: AdaptivePathwayEmbeddingInfo;
  [key: string]: unknown;
}

/** `metrics.exploration_health` — how much of the hint mix is coming from
    the exploration models vs. the standard path. `user_exploration_score` is
    intentionally not read by any UI (extension docs mark it internal-only). */
export interface AdaptivePathwayExplorationHealth {
  ig_pc_hint_ratio: number;
  action_entropy_50w: number;
  unique_primitives_active: number;
  wildcard_slot_used: number;
  user_exploration_score: number;
}

/** `GET /metrics` result — loosely typed like `AdaptivePathwayState`; Graph
    Health only reads `metrics.exploration_health` today. */
export interface AdaptivePathwayMetrics {
  metrics: {
    exploration_health: AdaptivePathwayExplorationHealth;
    [key: string]: unknown;
  };
}

/** `GET /session_reflection?session_id=...` result — the "see the roads not
    taken?" session-footer link. `top_domains` is `[domain, count]` pairs
    (Python's `Counter.most_common()`, serialized as JSON arrays). Note the
    field is `acceptance_score`, not `acceptance_rate` — the extension's own
    changelog names it differently from what the engine actually returns. */
export interface AdaptivePathwaySessionReflection {
  session_id: string;
  top_domains: [string, number][];
  acceptance_score: number;
  unchosen_novel_edges: number;
  reflection: string;
  has_untested: boolean;
  exploration_health: AdaptivePathwayExplorationHealth;
}

/** `GET /health` result — Graph Health tab (Round-D Batch 2). */
export interface AdaptivePathwayHealthIssue {
  severity: string;
  component: string;
  message: string;
  details: Record<string, unknown>;
}

/** `GET /graph_health` result (Round-7 item 6) — mirrors
    `adaptive_pathway.types.GraphHealth`, the richer companion to
    `AdaptivePathwayHealthIssue`'s issues-only `/health` payload. The nested
    `*_health`/`tier_distribution` blocks and `hotspot_details` entries are
    intentionally loosely typed (`Record<string, unknown>`) — their shape is
    Python-engine-internal and not otherwise mirrored on the Rust/TS side. */
export interface AdaptivePathwayGraphHealth {
  total_edges: number;
  high_confidence_pct: number;
  flagged_hotspots: number;
  last_override_rate: number;
  blocking_issues: boolean;
  dimensionality_health: Record<string, unknown>;
  ensemble_health: Record<string, unknown>;
  novelty_health: Record<string, unknown>;
  tier_distribution: Record<string, number>;
  hotspot_details: Record<string, unknown>[];
}

/** `GET /domains` entry — Domain Profiles tab (Round-D Batch 2). */
export interface AdaptivePathwayDomain {
  id: string;
  name: string;
  source: string;
  dpp_diversity_weight: number;
  novelty_lambda: number;
  revision_rate: number;
  acceptance_rate: number;
  sessions: number;
  edge_count: number;
  override_rate: number;
  last_inferred: string | null;
  locked: boolean;
}

// --- Chat / ACP (Phase 2) --- mirrors src-tauri/src/commands SessionInfo + events

export interface ModeInfo {
  id: string;
  name: string;
  description: string;
}

export interface SessionInfo {
  session_id: string;
  cwd: string;
  current_mode: string;
  available_modes: ModeInfo[];
  /** `null` when the active model doesn't support effort control at all (a
      single-option "off"-only model — see `parse_thinking_effort` in
      commands/session.rs). Live per-session control, no goosed restart. */
  thinking_effort: ThinkingEffort | null;
}

export interface EffortOption {
  name: string;
  value: string;
}

export interface ThinkingEffort {
  current_value: string;
  options: EffortOption[];
}

/** Raw ACP tool-call `update` object (shape varies; read defensively).
    Confirmed live: `tool_call` carries title + rawInput; `tool_call_update`
    carries status + content + rawOutput; later updates may omit title/status. */
export interface ToolCallUpdate {
  toolCallId?: string;
  title?: string;
  kind?: string;
  status?: string;
  rawInput?: unknown;
  rawOutput?: unknown;
  content?: unknown;
  [key: string]: unknown;
}

export interface TextDeltaEvent {
  session_id: string;
  text: string;
}

export interface ToolCallEvent {
  session_id: string;
  phase: 'tool_call' | 'tool_call_update';
  update: ToolCallUpdate;
}

export interface SessionTitleEvent {
  session_id: string;
  title: string;
}

export interface CompleteEvent {
  session_id: string;
  result: {
    stopReason?: string;
    /** Confirmed ACP `session/prompt` result shape (docs/acp-protocol.md). */
    usage?: { totalTokens?: number; inputTokens?: number; outputTokens?: number };
  };
}

export interface ChatErrorEvent {
  session_id: string;
  message: string;
}

// --- Approvals / modes (Phase 3) ---

export interface ApprovalOption {
  optionId: string;
  name: string;
  kind: string;
}

export interface ApprovalToolCall {
  toolCallId?: string;
  title?: string;
  kind?: string;
  status?: string;
  rawInput?: unknown;
  [key: string]: unknown;
}

export interface ApprovalNeededEvent {
  session_id: string;
  tool_call_id: string;
  tool_call: ApprovalToolCall;
  options: ApprovalOption[];
}

export interface ModeEvent {
  session_id: string;
  mode: string;
}

// --- Sessions / filesystem (Phase 4) ---

/** Mirrors src-tauri PathInfo. */
export interface PathInfo {
  path: string;
  name: string;
  is_dir: boolean;
  exists: boolean;
}

/** Mirrors `commands::file::FileEntry` — one file from a `list_directory`
    disk-scan (Artifacts pane, Round-7 item 5). */
export interface FileEntry {
  name: string;
  path: string;
  size: number;
  modified: number;
}

/** Parsed from a raw ACP `session/list` entry. */
export interface SessionSummary {
  sessionId: string;
  title: string;
  cwd: string;
  updatedAt: string;
  messageCount?: number;
  providerId?: string;
  modelId?: string;
}

/** Parse a raw session/list object (see docs/acp-protocol.md) defensively. */
export function parseSession(raw: Record<string, unknown>): SessionSummary {
  const meta = (raw._meta as Record<string, unknown>) ?? {};
  return {
    sessionId: String(raw.sessionId ?? ''),
    title: String(raw.title ?? 'New Chat'),
    cwd: String(raw.cwd ?? ''),
    updatedAt: String(raw.updatedAt ?? ''),
    messageCount: typeof meta.messageCount === 'number' ? meta.messageCount : undefined,
    providerId: typeof meta.providerId === 'string' ? meta.providerId : undefined,
    modelId: typeof meta.modelId === 'string' ? meta.modelId : undefined,
  };
}
