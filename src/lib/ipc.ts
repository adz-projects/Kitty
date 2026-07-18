// The ONLY file that calls Tauri `invoke()` / `listen()`. Everything else in the
// frontend goes through these typed wrappers (CLAUDE.md rule 2).

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { open as openDialog, save as saveDialog } from '@tauri-apps/plugin-dialog';
import type {
  AdaptivePathwayDomain,
  AdaptivePathwayEdge,
  AdaptivePathwayHealthIssue,
  AdaptivePathwayMetrics,
  AdaptivePathwaySchismAlert,
  AdaptivePathwaySessionReflection,
  AdaptivePathwaySchismPayload,
  AdaptivePathwayState,
  AdaptivePathwayStatus,
  AdaptivePathwayStatusPayload,
  AdaptivePathwayEmbeddingStatusPayload,
  ApprovalNeededEvent,
  ChatErrorEvent,
  CompleteEvent,
  Config,
  Detection,
  EmbeddingModelStatus,
  EnvVar,
  ExtensionDefault,
  FileAttachment,
  FolderData,
  LogEntry,
  ModeEvent,
  OllamaModel,
  OpenRouterCredits,
  PathInfo,
  ProviderProfile,
  ProviderView,
  PullProgress,
  Recipe,
  RecipeExtension,
  RecipeImportResult,
  RecipeInput,
  Schedule,
  ScheduledTask,
  SessionInfo,
  SessionTitleEvent,
  SettingsTarget,
  SetupValidation,
  StackStatus,
  StackStatusPayload,
  TextDeltaEvent,
  ThinkingEffort,
  ToolCallEvent,
} from './types';

export const ipc = {
  getConfig: () => invoke<Config>('get_config'),
  setConfig: (config: Config) => invoke<void>('set_config', { config }),
  toggleOverlay: () => invoke<void>('toggle_overlay'),
  hideOverlay: () => invoke<void>('hide_overlay'),
  openSettings: (section?: string, highlight?: string) =>
    invoke<void>('open_settings', { section: section ?? null, highlight: highlight ?? null }),
  openMain: () => invoke<void>('open_main'),
  getStackStatus: () => invoke<StackStatus>('get_stack_status'),
  restartGoosed: () => invoke<void>('restart_goosed'),
  newSession: (cwd?: string) => invoke<SessionInfo>('new_session', { cwd: cwd ?? null }),
  sendPrompt: (sessionId: string, text: string, images?: { mime: string; data_url: string }[]) =>
    invoke<void>('send_prompt', { sessionId, text, images: images ?? null }),
  cancelPrompt: (sessionId: string) => invoke<void>('cancel_prompt', { sessionId }),
  /** Fresh (not client-cached) check of whether `session/prompt` is currently
      in flight for this session — used when adopting a session (Expand
      mid-stream, or just resuming one) so the progress indicator reflects
      reality instead of assuming idle just because `session/load`'s replay
      doesn't reliably convey an in-progress turn. */
  isSessionBusy: (sessionId: string) => invoke<boolean>('is_session_busy', { sessionId }),
  // Widened beyond SessionInfo: the handoff (Expand mid-stream) also carries
  // the overlay's live `messages`/`artifacts` render state, since goosed's
  // own `session/load` replay doesn't reliably include an in-progress turn's
  // not-yet-committed partial content (confirmed real report: switching to
  // the full window while a response is still generating dropped it). The
  // backend command treats this as an opaque JSON blob either way.
  setActiveSession: (info: SessionInfo & Record<string, unknown>) =>
    invoke<void>('set_active_session', { info }),
  getActiveSession: () =>
    invoke<(SessionInfo & Record<string, unknown>) | null>('get_active_session'),
  respondPermission: (toolCallId: string, optionId: string | null) =>
    invoke<void>('respond_permission', { toolCallId, optionId }),
  setMode: (sessionId: string, modeId: string) => invoke<void>('set_mode', { sessionId, modeId }),
  listSessions: () => invoke<Record<string, unknown>[]>('list_sessions'),
  loadSession: (sessionId: string, cwd: string) =>
    invoke<SessionInfo>('load_session', { sessionId, cwd }),
  deleteSession: (sessionId: string, cwd?: string) =>
    invoke<void>('delete_session', { sessionId, cwd: cwd ?? null }),
  /** Settings → General "Clear all chat history" — a standalone destructive
      action, unrelated to provider switching. Returns the number deleted. */
  clearAllSessions: () => invoke<number>('clear_all_sessions'),
  // Chat folders (Round-2 item 15)
  listFolders: () => invoke<FolderData>('list_folders'),
  createFolder: (name: string) => invoke<void>('create_folder', { name }),
  renameFolder: (oldName: string, newName: string) =>
    invoke<void>('rename_folder', { old: oldName, new: newName }),
  deleteFolder: (name: string) => invoke<void>('delete_folder', { name }),
  assignSessionFolder: (sessionId: string, folder: string | null) =>
    invoke<void>('assign_session_folder', { sessionId, folder }),
  // Scheduled tasks — an instruction the agent runs later, one-shot or
  // recurring, with or without the app open.
  listScheduledTasks: () => invoke<ScheduledTask[]>('list_scheduled_tasks'),
  createScheduledTask: (
    name: string,
    prompt: string,
    cwd: string | null,
    schedule: Schedule,
    nextFire: string
  ) => invoke<ScheduledTask>('create_scheduled_task', { name, prompt, cwd, schedule, nextFire }),
  updateScheduledTask: (
    id: string,
    name: string,
    prompt: string,
    cwd: string | null,
    schedule: Schedule,
    nextFire: string,
    enabled: boolean
  ) =>
    invoke<void>('update_scheduled_task', {
      id,
      name,
      prompt,
      cwd,
      schedule,
      nextFire,
      enabled,
    }),
  deleteScheduledTask: (id: string) => invoke<void>('delete_scheduled_task', { id }),
  setScheduledTaskEnabled: (id: string, enabled: boolean) =>
    invoke<void>('set_scheduled_task_enabled', { id, enabled }),
  // Recipes — client-side-interpreted Goose recipe templates (see
  // chatStore.ts's sendWithRecipe and docs/BACKLOG.md's now-resolved entry).
  listRecipes: () => invoke<Recipe[]>('list_recipes'),
  createRecipe: (recipe: RecipeInput) => invoke<Recipe>('create_recipe', { recipe }),
  updateRecipe: (id: string, recipe: RecipeInput) =>
    invoke<void>('update_recipe', { id, recipe }),
  deleteRecipe: (id: string) => invoke<void>('delete_recipe', { id }),
  duplicateRecipe: (id: string) => invoke<Recipe>('duplicate_recipe', { id }),
  importRecipeYaml: (path: string) =>
    invoke<RecipeImportResult>('import_recipe_yaml', { path }),
  exportRecipeYaml: (id: string, path: string) =>
    invoke<void>('export_recipe_yaml', { id, path }),
  addRecipeExtension: (sessionId: string, extension: RecipeExtension) =>
    invoke<void>('add_recipe_extension', { sessionId, extension }),
  // Error/warning log (Settings → Advanced) — captured server-side from
  // `tracing::warn!`/`error!` calls via `log_capture`'s in-memory ring buffer.
  listLogEntries: () => invoke<LogEntry[]>('list_log_entries'),
  clearLogEntries: () => invoke<void>('clear_log_entries'),
  // Instant per-session mode toggle (Round-4)
  getSessionMode: (sessionId: string) => invoke<string | null>('get_session_mode', { sessionId }),
  setSessionMode: (sessionId: string, mode: string | null) =>
    invoke<void>('set_session_mode', { sessionId, mode }),
  forkSession: (sessionId: string, cwd: string, truncateFrom: number | null) =>
    invoke<SessionInfo>('fork_session', { sessionId, cwd, truncateFrom }),
  setThinkingEffort: (sessionId: string, value: string) =>
    invoke<ThinkingEffort | null>('set_thinking_effort', { sessionId, value }),
  /** Best-effort: hot-rebind an already-open session onto the currently
      active provider's model after a provider switch (fixes a stale model id
      otherwise sent to the newly-active provider). Never throws — the
      backend swallows its own failures. */
  rebindSessionProvider: (sessionId: string) =>
    invoke<void>('rebind_session_provider', { sessionId }),
  readTextFile: (path: string) => invoke<string>('read_text_file', { path, maxBytes: null }),
  readFileAny: (path: string) => invoke<FileAttachment>('read_file_any', { path, maxBytes: null }),
  /** Copies a file into a chat session's own working directory, so the
      model's own file tools can open it directly — used for chat-only mode
      attachments that can't be inlined as text (`.docx`, `.pdf`, …). Returns
      the copied file's own name (deduplicated against an existing file of
      the same name, if any). */
  copyFileIntoChatFolder: (sourcePath: string, cwd: string) =>
    invoke<string>('copy_file_into_chat_folder', { sourcePath, cwd }),
  writeFile: (path: string, content: string) => invoke<void>('write_file', { path, content }),
  inspectPaths: (paths: string[]) => invoke<PathInfo[]>('inspect_paths', { paths }),
  openPath: (path: string) => invoke<void>('open_path', { path }),
  revealPath: (path: string) => invoke<void>('reveal_path', { path }),
  // Providers
  listProviders: () => invoke<ProviderView[]>('list_providers'),
  upsertProvider: (profile: ProviderProfile, secret: string | null) =>
    invoke<ProviderProfile>('upsert_provider', { profile, secret }),
  deleteProvider: (id: string) => invoke<void>('delete_provider', { id }),
  activateProvider: (id: string | null) => invoke<void>('activate_provider', { id }),
  testActiveProviderConnection: () => invoke<void>('test_active_provider_connection'),
  // Ollama
  ollamaListModels: () => invoke<OllamaModel[]>('ollama_list_models'),
  ollamaDeleteModel: (model: string) => invoke<void>('ollama_delete_model', { model }),
  ollamaPullModel: (model: string) => invoke<string>('ollama_pull_model', { model }),
  ollamaShowContextLength: (model: string) =>
    invoke<number | null>('ollama_show_context_length', { model }),
  openrouterContextLength: (model: string) =>
    invoke<number | null>('openrouter_context_length', { model }),
  openrouterCredits: (providerId: string) =>
    invoke<OpenRouterCredits>('openrouter_credits', { providerId }),
  // Ollama env helper
  readOllamaEnv: () => invoke<EnvVar[]>('read_ollama_env'),
  setOllamaEnv: (name: string, value: string | null) =>
    invoke<void>('set_ollama_env', { name, value }),
  restartOllama: () => invoke<void>('restart_ollama'),
  ensureOllamaRunning: () => invoke<void>('ensure_ollama_running'),
  // Extension defaults (Round-7 Feature 4) — read/write goose's own
  // config.yaml directly; not session-scoped.
  listDefaultExtensions: () => invoke<ExtensionDefault[]>('list_default_extensions'),
  setDefaultExtensionEnabled: (id: string, enabled: boolean) =>
    invoke<void>('set_default_extension_enabled', { id, enabled }),
  addExtension: (name: string, command: string, args: string[], env: string[]) =>
    invoke<void>('add_extension', { name, command, args, env }),
  setExtensionEnv: (id: string, key: string, value: string) =>
    invoke<void>('set_extension_env', { id, key, value }),
  // replacement-mcp (goosed-spawned stdio MCP extension — Kitty only owns its
  // config.yaml registration, not the process; see commands/replacement_mcp.rs)
  getReplacementMcpEnabled: () => invoke<boolean>('get_replacement_mcp_enabled'),
  setReplacementMcpEnabled: (enabled: boolean) =>
    invoke<void>('set_replacement_mcp_enabled', { enabled }),
  disableBuiltinDevExtensions: () => invoke<void>('disable_builtin_dev_extensions'),
  // Settings deep link
  getSettingsTarget: () => invoke<SettingsTarget | null>('get_settings_target'),
  // Theming
  listThemes: () => invoke<{ builtins: string[]; user: string[] }>('list_themes'),
  readUserTheme: (name: string) => invoke<string>('read_user_theme', { name }),
  openThemesFolder: () => invoke<void>('open_themes_folder'),
  readImageDataUrl: (path: string) => invoke<string>('read_image_data_url', { path }),
  // Wizard / setup
  detectDependencies: () => invoke<Detection>('detect_dependencies'),
  installDependency: (which: 'ollama' | 'goose') => invoke<void>('install_dependency', { which }),
  validateSetup: () => invoke<SetupValidation>('validate_setup'),
  openWizard: (mode?: 'setup' | 'repair') => invoke<void>('open_wizard', { mode: mode ?? 'setup' }),
  getWizardMode: () => invoke<string | null>('get_wizard_mode'),
  completeSetup: () => invoke<void>('complete_setup'),
  getAutostart: () => invoke<boolean>('get_autostart'),
  setAutostart: (enabled: boolean) => invoke<void>('set_autostart', { enabled }),
  // Adaptive Pathway extension sidecar
  getAdaptivePathwayStatus: () => invoke<AdaptivePathwayStatus>('get_adaptive_pathway_status'),
  getAdaptivePathwayEmbeddingStatus: () =>
    invoke<EmbeddingModelStatus>('get_adaptive_pathway_embedding_status'),
  restartAdaptivePathway: () => invoke<void>('restart_adaptive_pathway'),
  setAdaptivePathwayEnabled: (enabled: boolean) =>
    invoke<void>('set_adaptive_pathway_enabled', { enabled }),
  adaptivePathwayGetEdge: (edgeId: string) =>
    invoke<AdaptivePathwayEdge>('adaptive_pathway_get_edge', { edgeId }),
  adaptivePathwayGetState: () => invoke<AdaptivePathwayState>('adaptive_pathway_get_state'),
  adaptivePathwayGetMetrics: () => invoke<AdaptivePathwayMetrics>('adaptive_pathway_get_metrics'),
  adaptivePathwayRecordAnnotation: (
    sessionId: string,
    annotationType: string,
    edgeId: string | null,
    actionId: string | null,
    intensity: number
  ) =>
    invoke<void>('adaptive_pathway_record_annotation', {
      sessionId,
      annotationType,
      edgeId,
      actionId,
      intensity,
    }),
  adaptivePathwayToggleSuggestions: (sessionId: string, paused: boolean) =>
    invoke<void>('adaptive_pathway_toggle_suggestions', { sessionId, paused }),
  adaptivePathwayGetSchism: () =>
    invoke<AdaptivePathwaySchismAlert | { state: 'none' }>('adaptive_pathway_get_schism'),
  adaptivePathwayResolveSchism: (keepFaction: 'a' | 'b' | 'both') =>
    invoke<{ status: string }>('adaptive_pathway_resolve_schism', { keepFaction }),
  adaptivePathwayUpdateEnsembleWeights: (
    igWeightMin: number | null,
    igWeightMax: number | null,
    pcWeight: number | null
  ) =>
    invoke<{ ig_weight_min: number; ig_weight_max: number; pc_weight: number }>(
      'adaptive_pathway_update_ensemble_weights',
      { igWeightMin, igWeightMax, pcWeight }
    ),
  adaptivePathwayHealth: () =>
    invoke<{ issues: AdaptivePathwayHealthIssue[] }>('adaptive_pathway_health'),
  adaptivePathwayListDomains: () =>
    invoke<AdaptivePathwayDomain[]>('adaptive_pathway_list_domains'),
  adaptivePathwayUpdateDomain: (
    domainId: string,
    name: string | null,
    dppDiversityWeight: number | null,
    noveltyLambda: number | null,
    locked: boolean | null
  ) =>
    invoke<AdaptivePathwayDomain>('adaptive_pathway_update_domain', {
      domainId,
      name,
      dppDiversityWeight,
      noveltyLambda,
      locked,
    }),
  adaptivePathwayAcceptNudge: (sessionId: string) =>
    invoke<{ status: string; active: boolean; multiplier: number }>(
      'adaptive_pathway_accept_nudge',
      { sessionId }
    ),
  adaptivePathwayDismissNudge: () => invoke<void>('adaptive_pathway_dismiss_nudge'),
  adaptivePathwayGetSessionReflection: (sessionId: string) =>
    invoke<AdaptivePathwaySessionReflection>('adaptive_pathway_get_session_reflection', {
      sessionId,
    }),
};

/** Native folder picker (default context folder, etc.). Returns null if cancelled. */
export async function pickFolder(): Promise<string | null> {
  const res = await openDialog({ directory: true, multiple: false });
  return typeof res === 'string' ? res : null;
}

/** Native image-file picker (background image). Returns null if cancelled. */
export async function pickImage(): Promise<string | null> {
  const res = await openDialog({
    multiple: false,
    filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg', 'gif', 'webp'] }],
  });
  return typeof res === 'string' ? res : null;
}

/** Native multi-file picker for the composer's attach button (Round-5). No type
    filter — any file can be attached, same as OS drag-drop. Returns the picked
    absolute paths (empty if cancelled) so callers can feed them straight into
    the existing `inspectPaths` → `addDroppedPaths` pipeline. */
export async function pickFiles(): Promise<string[]> {
  const res = await openDialog({ multiple: true, directory: false });
  if (res == null) return [];
  return Array.isArray(res) ? res : [res];
}

/** Native `.exe` picker — the wizard's manual "point at an existing Goose
    install" fallback next to its one-click install. Returns null if
    cancelled. Callers persist the result via `ipc.setConfig` (no dedicated
    Rust command needed, same as every other plain config field). */
export async function pickExecutable(): Promise<string | null> {
  const res = await openDialog({
    multiple: false,
    filters: [{ name: 'Executable', extensions: ['exe'] }],
  });
  return typeof res === 'string' ? res : null;
}

/** Native save-file dialog for the session export (Round-3 item 24: OpenAI
    messages-array JSONL). Returns null if cancelled. */
export async function pickSavePath(defaultName: string): Promise<string | null> {
  const res = await saveDialog({
    defaultPath: defaultName,
    filters: [{ name: 'JSON Lines', extensions: ['jsonl'] }],
  });
  return res ?? null;
}

/** Native picker for importing a standalone Goose recipe file. `.yml` is
    deliberately not offered — Goose's own docs say only `.yaml`/`.json` are
    supported for recipes (the Rust-side command rejects `.yml` explicitly
    too, with a clearer message, in case one gets through some other way). */
export async function pickRecipeYaml(): Promise<string | null> {
  const res = await openDialog({
    multiple: false,
    filters: [{ name: 'Goose recipe', extensions: ['yaml', 'json'] }],
  });
  return typeof res === 'string' ? res : null;
}

/** Native save-file dialog for exporting a recipe as a real, portable Goose
    recipe `.yaml` (usable by the standalone `goose run --recipe` CLI). */
export async function pickRecipeSavePath(defaultName: string): Promise<string | null> {
  const res = await saveDialog({
    defaultPath: defaultName,
    filters: [{ name: 'YAML', extensions: ['yaml'] }],
  });
  return res ?? null;
}

/** Subscribe to stack status changes. Returns an unlisten fn. */
export function onStackStatus(cb: (payload: StackStatusPayload) => void): Promise<UnlistenFn> {
  return listen<StackStatusPayload>('stack://status', (e) => cb(e.payload));
}

/** Adaptive Pathway sidecar status changes (kept separate from stack status —
    an optional augmentation, not a chat-blocking dependency). */
export const onAdaptivePathwayStatus = (cb: (payload: AdaptivePathwayStatusPayload) => void) =>
  listen<AdaptivePathwayStatusPayload>('adaptive_pathway://status', (e) => cb(e.payload));

/** Embedding-model readiness changes (downloading/present/missing) — separate
    from the sidecar status above; see `EmbeddingModelStatus`. */
export const onAdaptivePathwayEmbeddingStatus = (
  cb: (payload: AdaptivePathwayEmbeddingStatusPayload) => void
) =>
  listen<AdaptivePathwayEmbeddingStatusPayload>('adaptive_pathway://embedding_status', (e) =>
    cb(e.payload)
  );

/** Fires only when `schism_state` flips into `detected`/`reviewing` — drives
    the Schism Resolution modal without needing Settings open. */
export const onAdaptivePathwaySchism = (cb: (payload: AdaptivePathwaySchismPayload) => void) =>
  listen<AdaptivePathwaySchismPayload>('adaptive_pathway://schism', (e) => cb(e.payload));

// --- Chat event subscriptions (Phase 2) ---
export const onMessageDelta = (cb: (e: TextDeltaEvent) => void) =>
  listen<TextDeltaEvent>('chat://message-delta', (e) => cb(e.payload));
export const onReasoningDelta = (cb: (e: TextDeltaEvent) => void) =>
  listen<TextDeltaEvent>('chat://reasoning-delta', (e) => cb(e.payload));
export const onToolCall = (cb: (e: ToolCallEvent) => void) =>
  listen<ToolCallEvent>('chat://tool-call', (e) => cb(e.payload));
export const onSessionTitle = (cb: (e: SessionTitleEvent) => void) =>
  listen<SessionTitleEvent>('chat://session-title', (e) => cb(e.payload));
export const onComplete = (cb: (e: CompleteEvent) => void) =>
  listen<CompleteEvent>('chat://complete', (e) => cb(e.payload));
export const onChatError = (cb: (e: ChatErrorEvent) => void) =>
  listen<ChatErrorEvent>('chat://error', (e) => cb(e.payload));

export const onUserMessage = (cb: (e: TextDeltaEvent) => void) =>
  listen<TextDeltaEvent>('chat://user-message', (e) => cb(e.payload));
export const onApprovalNeeded = (cb: (e: ApprovalNeededEvent) => void) =>
  listen<ApprovalNeededEvent>('chat://tool-approval-needed', (e) => cb(e.payload));
export const onMode = (cb: (e: ModeEvent) => void) =>
  listen<ModeEvent>('chat://mode', (e) => cb(e.payload));

/** OS file/folder drop onto this window → absolute paths. */
export function onFileDrop(cb: (paths: string[]) => void): Promise<UnlistenFn> {
  return getCurrentWebview().onDragDropEvent((e) => {
    if (e.payload.type === 'drop') cb(e.payload.paths);
  });
}

export const onPullProgress = (cb: (e: PullProgress) => void) =>
  listen<PullProgress>('ollama://pull-progress', (e) => cb(e.payload));

export const onSettingsNavigate = (cb: (t: SettingsTarget) => void) =>
  listen<SettingsTarget>('settings://navigate', (e) => cb(e.payload));

export const onThemeChanged = (cb: () => void) => listen('theme://changed', () => cb());

export const onWizardNavigate = (cb: (mode: string) => void) =>
  listen<{ mode: string }>('wizard://navigate', (e) => cb(e.payload.mode));

/** Clipboard-to-Kitty hotkey/tray item (Round-4): the overlay is already
    shown by the time this fires; payload is whichever the clipboard held. */
export type ClipboardAttachEvent =
  { kind: 'text'; text: string } | { kind: 'image'; mime: string; data_url: string };
export const onClipboardAttach = (cb: (e: ClipboardAttachEvent) => void) =>
  listen<ClipboardAttachEvent>('clipboard://attach', (e) => cb(e.payload));

/** A new session was created in *any* window (Round-4 item 6) — overlay and
    main each own an independent store, so this is how one tells the other
    its session list/recents dropdown is stale. */
export const onSessionCreated = (cb: () => void) => listen('session://created', () => cb());

/** A single session was deleted in *any* window (e.g. `regenerate()`'s
    background cleanup of the session it forked away from, or a user-driven
    delete from the sidebar) — same cross-window-staleness reason as
    `onSessionCreated`. Distinct from `onSessionsCleared` below, which is the
    bulk "delete everything" case. Payload lets a window check whether the
    deleted session is the one it currently has open. */
export const onSessionDeleted = (cb: (sessionId: string) => void) =>
  listen<{ sessionId: string }>('session://deleted', (e) => cb(e.payload.sessionId));

/** A session was handed off to the full window (Expand / auto-promote). Lets an
    *already-open* main window re-adopt it — its mount-time getActiveSession runs
    only once. Payload is the same shape getActiveSession returns. */
export const onActiveSession = (cb: (info: SessionInfo & Record<string, unknown>) => void) =>
  listen<SessionInfo & Record<string, unknown>>('session://active', (e) => cb(e.payload));

/** Folder state (create/rename/delete/assign) changed in *any* window
    (Round-5) — same cross-window-staleness reason as `onSessionCreated`, but
    for the app-side chat-folder mapping the other window's sidebar renders. */
export const onFoldersChanged = (cb: () => void) => listen('folders://changed', () => cb());

/** Every session was just deleted (Settings → General "Clear all chat
    history") — same cross-window staleness reason as `onSessionCreated`. */
export const onSessionsCleared = (cb: (deleted: number) => void) =>
  listen<{ deleted: number }>('session://cleared', (e) => cb(e.payload.deleted));

/** A scheduled task was created/updated/deleted/toggled — same cross-window
    staleness pattern as `onFoldersChanged`, in case the Settings window and
    another window are both open. */
export const onScheduledTasksChanged = (cb: () => void) =>
  listen('scheduled_tasks://changed', () => cb());

/** Fires on any recipe create/update/delete/duplicate/import — same
    live-refresh staleness pattern as `onScheduledTasksChanged`, since both
    `Composer.tsx` (slash-command matching) and Settings → Recipes may be
    open at once. */
export const onRecipesChanged = (cb: () => void) => listen('recipes://changed', () => cb());

export interface ProviderHealth {
  reachable: boolean;
  host?: string;
  name?: string;
}
export const onProviderHealth = (cb: (h: ProviderHealth) => void) =>
  listen<ProviderHealth>('provider://health', (e) => cb(e.payload));

/** Fired after a provider is (de)activated + goosed respawns (Round-2 item 4). */
export const onProviderActivated = (cb: () => void) => listen('provider://activated', () => cb());

/** The label of the window this webview is running in (`overlay` / `main` / …). */
export const windowLabel = (): string => getCurrentWebview().label;

/** Tray "New Session" → overlay starts a fresh session. */
export const onNewSessionRequest = (cb: () => void) => listen('session://new', () => cb());
