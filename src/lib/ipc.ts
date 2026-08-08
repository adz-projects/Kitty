// The ONLY file that calls Tauri `invoke()` / `listen()`. Everything else in the
// frontend goes through these typed wrappers (CLAUDE.md rule 2).

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { open as openDialog, save as saveDialog } from '@tauri-apps/plugin-dialog';
import type {
  AdaptivePathwayEmbeddingStatusPayload,
  AdaptivePathwayMcpStatus,
  PathwayBelief,
  PathwayStats,
  ApprovalNeededEvent,
  ChatErrorEvent,
  CompactionEvent,
  CompleteEvent,
  Config,
  Detection,
  EnvVar,
  FileAttachment,
  FileEntry,
  FolderData,
  LogEntry,
  McpServer,
  McpServerPatch,
  McpServerSpec,
  MemoryStats,
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
  StartupPhase,
  StartupPhasePayload,
  TextDeltaEvent,
  ThinkingEffort,
  ToolCallEvent,
} from './types';

export const ipc = {
  getConfig: () => invoke<Config>('get_config'),
  setConfig: (config: Config) => invoke<void>('set_config', { config }),
  getConfigRecoveryNotice: () => invoke<string | null>('get_config_recovery_notice'),
  toggleOverlay: () => invoke<void>('toggle_overlay'),
  hideOverlay: () => invoke<void>('hide_overlay'),
  openSettings: (section?: string, highlight?: string) =>
    invoke<void>('open_settings', { section: section ?? null, highlight: highlight ?? null }),
  openMain: () => invoke<void>('open_main'),
  /** Feature 5 — always creates a brand-new chat window (never reuses one),
      optionally handing it a session snapshot to adopt on mount (the
      overlay's Expand). Distinct from `setActiveSession`/`getActiveSession`
      below, which remain in place for the unrelated provider
      context-handoff gate (Settings -> Providers). */
  openNewChatWindow: (handoff?: (SessionInfo & Record<string, unknown>) | null) =>
    invoke<void>('open_new_chat_window', { handoff: handoff ?? null }),
  /** One-shot read of *this* window's pending handoff, if Expand created it
      with one — consumed server-side on read, so calling this twice returns
      `null` the second time. */
  getPendingHandoff: () =>
    invoke<(SessionInfo & Record<string, unknown>) | null>('get_pending_handoff'),
  /** Feature 3 — screenshot region capture. Opens the selection window over
      a lightweight preview and resolves once the user confirms a rectangle
      (rejects on Escape/cancel); the final image is a fresh, full-resolution
      capture of just that region, never the preview itself. */
  captureScreenshotRegion: () =>
    invoke<{ mime: string; data_url: string }>('capture_screenshot_region'),
  /** Selection window's own mount-time read of the preview to show. */
  getScreenshotPreview: () =>
    invoke<[string, number, number, number, number] | null>('get_screenshot_preview'),
  reportScreenshotSelection: (x: number, y: number, width: number, height: number) =>
    invoke<void>('report_screenshot_selection', { x, y, width, height }),
  cancelScreenshotSelection: () => invoke<void>('cancel_screenshot_selection'),
  getStackStatus: () => invoke<StackStatus>('get_stack_status'),
  getStartupPhase: () => invoke<StartupPhase>('get_startup_phase'),
  restartBackend: () => invoke<void>('restart_backend'),
  newSession: (cwd?: string, mode?: string | null) =>
    invoke<SessionInfo>('new_session', { cwd: cwd ?? null, mode: mode ?? null }),
  /** Tells Rust which window is now showing `sessionId` — lets a
      notification for that session later focus this specific window
      instead of a generic fallback. Best-effort; call after any successful
      session establish/switch. */
  bindWindowSession: (sessionId: string) => invoke<void>('bind_window_session', { sessionId }),
  /** "Set as working directory" (agentic mode) — repoints an existing
      session's cwd in place instead of forking a new session, so BigTiny's
      directory sandbox can allow both the original chat_dir and this newly-
      set directory at once. */
  setSessionContextDir: (sessionId: string, cwd: string) =>
    invoke<void>('set_session_context_dir', { sessionId, cwd }),
  /** Set a session's custom/default persona server-side (BigTiny's real
      `persona_override` mechanism — a proper `role: "system"` message),
      replacing the old client-side `<system>...</system>` text-prepend hack.
      Called once, before the first outgoing message of a new session. */
  setSessionPersonaOverride: (sessionId: string, persona: string) =>
    invoke<void>('set_session_persona_override', { sessionId, persona }),
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
  /** Fires the "Approval needed" toast/tray-pending state — call only once
      it's known a tool call genuinely needs a human (never for one about to
      be silently auto-resolved), since BigTiny's own hitl_pause event no
      longer triggers a notification directly. */
  notifyApprovalNeeded: (sessionId: string, toolName: string) =>
    invoke<void>('notify_approval_needed', { sessionId, toolName }),
  setMode: (sessionId: string, modeId: string) => invoke<void>('set_mode', { sessionId, modeId }),
  listSessions: () => invoke<Record<string, unknown>[]>('list_sessions'),
  loadSession: (sessionId: string, cwd: string) =>
    invoke<SessionInfo>('load_session', { sessionId, cwd }),
  deleteSession: (sessionId: string, cwd?: string) =>
    invoke<void>('delete_session', { sessionId, cwd: cwd ?? null }),
  renameSession: (sessionId: string, title: string) =>
    invoke<void>('rename_session', { sessionId, title }),
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
  updateRecipe: (id: string, recipe: RecipeInput) => invoke<void>('update_recipe', { id, recipe }),
  deleteRecipe: (id: string) => invoke<void>('delete_recipe', { id }),
  duplicateRecipe: (id: string) => invoke<Recipe>('duplicate_recipe', { id }),
  importRecipeYaml: (path: string) => invoke<RecipeImportResult>('import_recipe_yaml', { path }),
  exportRecipeYaml: (id: string, path: string) => invoke<void>('export_recipe_yaml', { id, path }),
  addRecipeExtension: (sessionId: string, extension: RecipeExtension) =>
    invoke<void>('add_recipe_extension', { sessionId, extension }),
  // Error/warning log (Settings → Advanced) — captured server-side from
  // `tracing::warn!`/`error!` calls via `log_capture`'s in-memory ring buffer.
  listLogEntries: () => invoke<LogEntry[]>('list_log_entries'),
  clearLogEntries: () => invoke<void>('clear_log_entries'),
  /** Daemon-global pre-flight memory recall telemetry (Settings → Advanced). */
  getMemoryStats: () => invoke<MemoryStats>('get_memory_stats'),
  // Instant per-session mode toggle (Round-4)
  getSessionMode: (sessionId: string) => invoke<string | null>('get_session_mode', { sessionId }),
  setSessionMode: (sessionId: string, mode: string | null) =>
    invoke<void>('set_session_mode', { sessionId, mode }),
  forkSession: (sessionId: string, cwd: string, truncateFrom: number | null) =>
    invoke<SessionInfo>('fork_session', { sessionId, cwd, truncateFrom }),
  compactSession: (sessionId: string) =>
    invoke<{
      compacted: boolean;
      messages_compacted?: number;
      tokens_before?: number;
      tokens_after?: number;
    }>('compact_session', { sessionId }),
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
  listDirectory: (path: string) => invoke<FileEntry[]>('list_directory', { path }),
  // Providers
  listProviders: () => invoke<ProviderView[]>('list_providers'),
  upsertProvider: (profile: ProviderProfile, secret: string | null) =>
    invoke<ProviderProfile>('upsert_provider', { profile, secret }),
  deleteProvider: (id: string) => invoke<void>('delete_provider', { id }),
  activateProvider: (id: string | null, sessionId?: string | null) =>
    invoke<void>('activate_provider', { id, sessionId: sessionId ?? null }),
  setSessionProvider: (sessionId: string, providerId: string, model?: string | null) =>
    invoke<void>('set_session_provider', { sessionId, providerId, model: model ?? null }),
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
  // MCP servers — daemon-global, live over REST (BigTiny; no restart needed
  // to add/edit/delete/toggle).
  listMcpServers: () => invoke<McpServer[]>('list_mcp_servers'),
  addMcpServer: (spec: McpServerSpec) => invoke<string>('add_mcp_server', { spec }),
  updateMcpServer: (id: string, patch: McpServerPatch) =>
    invoke<McpServer>('update_mcp_server', { id, patch }),
  deleteMcpServer: (id: string) => invoke<void>('delete_mcp_server', { id }),
  setMcpServerEnabled: (id: string, enabled: boolean) =>
    invoke<McpServer>('set_mcp_server_enabled', { id, enabled }),
  connectMcpServer: (id: string) => invoke<void>('connect_mcp_server', { id }),
  // Bundled sandboxed-WebAssembly math/Python MCP server (kitty-wasm), the
  // Rust replacement for the retired wasm-math-mcp — on by default, no
  // credentials, same self-healing registration pattern as kitty-tools.
  getKittyWasmEnabled: () => invoke<boolean>('get_kitty_wasm_enabled'),
  setKittyWasmEnabled: (enabled: boolean) => invoke<void>('set_kitty_wasm_enabled', { enabled }),
  // Whether the accessible tables/SVG diagrams tools are advertised by the
  // combined kitty-tools server (renders its results in an iframe in chat) —
  // on by default, no credentials. Toggling this alone doesn't restart a
  // separate process; it flips an env var on kitty-tools's registration.
  getVisualizationsEnabled: () => invoke<boolean>('get_visualizations_enabled'),
  setVisualizationsEnabled: (enabled: boolean) =>
    invoke<void>('set_visualizations_enabled', { enabled }),
  // Bundled Rust MCP server hosting shell/workspace/file/word/cache/
  // scratchpad/Excel/PDF tools (the retired replacement-mcp's full surface),
  // plus the visualizations gated by their own flag — on by default, no
  // credentials, same self-healing registration pattern. Web search does
  // NOT live here — see getKittyWebEnabled/getBraveMcpSearchStatus.
  getKittyToolsEnabled: () => invoke<boolean>('get_kitty_tools_enabled'),
  setKittyToolsEnabled: (enabled: boolean) => invoke<void>('set_kitty_tools_enabled', { enabled }),
  // Bundled Rust web search/scrape MCP server (kitty-web) — the Rust
  // replacement for the retired Python kitty-docs-web; on by default, no
  // credentials. Hosts the merged, count-tiered
  // lean_web_search/lean_web_search_read_chunk and lean_web_scrape
  // (DuckDuckGo always works; Brave preference controlled separately below).
  getKittyWebEnabled: () => invoke<boolean>('get_kitty_web_enabled'),
  setKittyWebEnabled: (enabled: boolean) => invoke<void>('set_kitty_web_enabled', { enabled }),
  // Brave search preference on the kitty-web server — off by default,
  // requires an API key. Does not gate whether lean_web_search exists (it
  // always does); only whether it prefers Brave over DuckDuckGo. Disabling
  // always wipes the stored key server-side, so re-enabling always goes
  // through setBraveMcpSearchApiKey, never a plain enabled toggle.
  getBraveMcpSearchStatus: () =>
    invoke<{ enabled: boolean; configured: boolean }>('get_brave_mcp_search_status'),
  setBraveMcpSearchApiKey: (apiKey: string) =>
    invoke<void>('set_brave_mcp_search_api_key', { apiKey }),
  setBraveMcpSearchEnabled: (enabled: boolean) =>
    invoke<void>('set_brave_mcp_search_enabled', { enabled }),
  // Settings deep link
  getSettingsTarget: () => invoke<SettingsTarget | null>('get_settings_target'),
  // Theming
  listThemes: () => invoke<{ builtins: string[]; user: string[] }>('list_themes'),
  readUserTheme: (name: string) => invoke<string>('read_user_theme', { name }),
  openThemesFolder: () => invoke<void>('open_themes_folder'),
  readImageDataUrl: (path: string) => invoke<string>('read_image_data_url', { path }),
  // Wizard / setup
  detectDependencies: () => invoke<Detection>('detect_dependencies'),
  installDependency: (which: 'ollama') => invoke<void>('install_dependency', { which }),
  validateSetup: () => invoke<SetupValidation>('validate_setup'),
  openWizard: (mode?: 'setup' | 'repair') => invoke<void>('open_wizard', { mode: mode ?? 'setup' }),
  getWizardMode: () => invoke<string | null>('get_wizard_mode'),
  completeSetup: () => invoke<void>('complete_setup'),
  getAutostart: () => invoke<boolean>('get_autostart'),
  setAutostart: (enabled: boolean) => invoke<void>('set_autostart', { enabled }),
  // Behavioral-memory engine (`plugins/adaptive-pathway_rust`, linked
  // in-process into BigTiny — see `src-tauri/src/commands/adaptive_pathway.rs`).
  getAdaptivePathwayMcpStatus: () =>
    invoke<AdaptivePathwayMcpStatus | null>('get_adaptive_pathway_mcp_status'),
  setAdaptivePathwayEnabled: (enabled: boolean) =>
    invoke<void>('set_adaptive_pathway_enabled', { enabled }),
  /** Every belief currently held (Settings belief browser). */
  getPathwayBeliefs: () => invoke<{ beliefs: PathwayBelief[]; count: number }>('get_pathway_beliefs'),
  /** Belief counts by layer, plus embedding-migration progress. */
  getPathwayStats: () => invoke<PathwayStats>('get_pathway_stats'),
  /** Belief browser's delete action — suppresses + tombstones, not a bare row delete. */
  deletePathwayBelief: (beliefId: string) =>
    invoke<{ id: string; dropped?: string; error?: string }>('delete_pathway_belief', { beliefId }),
  /** The incognito toggle for one session. */
  setPathwaySessionPaused: (sessionId: string, paused: boolean) =>
    invoke<{ session_id: string; paused: boolean }>('set_pathway_session_paused', {
      sessionId,
      paused,
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
    filters: [{ name: 'Recipe', extensions: ['yaml', 'json'] }],
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

/** Subscribe to startup-phase changes. Returns an unlisten fn. */
export function onStartupPhase(cb: (payload: StartupPhasePayload) => void): Promise<UnlistenFn> {
  return listen<StartupPhasePayload>('stack://startup-phase', (e) => cb(e.payload));
}

/** Embedding-model readiness changes (downloading/present/missing) for the
    pathway engine's shared embedding model — see `EmbeddingModelStatus`. */
export const onAdaptivePathwayEmbeddingStatus = (
  cb: (payload: AdaptivePathwayEmbeddingStatusPayload) => void
) =>
  listen<AdaptivePathwayEmbeddingStatusPayload>('adaptive_pathway://embedding_status', (e) =>
    cb(e.payload)
  );

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
export const onCompaction = (cb: (e: CompactionEvent) => void) =>
  listen<CompactionEvent>('chat://compaction', (e) => cb(e.payload));

export const onUserMessage = (cb: (e: TextDeltaEvent) => void) =>
  listen<TextDeltaEvent>('chat://user-message', (e) => cb(e.payload));
export const onApprovalNeeded = (cb: (e: ApprovalNeededEvent) => void) =>
  listen<ApprovalNeededEvent>('chat://tool-approval-needed', (e) => cb(e.payload));

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

/** A completion/failure notification was clicked for a session no longer bound
    to any specific window (the window that had it switched to a different chat
    in the meantime) — targets exactly one already-open window via `emit_to`
    (`windows::focus_or_open_session`), asking it to reload that session rather
    than opening a generic blank one. */
export const onAdoptSession = (cb: (info: SessionInfo & Record<string, unknown>) => void) =>
  listen<SessionInfo & Record<string, unknown>>('chat://adopt-session', (e) => cb(e.payload));

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

/** Called once, right after mount, by every window's `main.tsx`. Lets the
    Rust-side dev-only load watchdog (`windows::spawn_load_watchdog`) tell a
    window that's still loading apart from one whose first navigation failed
    and will never load on its own. Cheap to call in production too — nothing
    there reads `booted_windows`, since the watchdog itself is compiled out. */
export const windowReady = (): Promise<void> => invoke<void>('window_ready');

/** Tray "New Session" → overlay starts a fresh session. */
export const onNewSessionRequest = (cb: () => void) => listen('session://new', () => cb());
