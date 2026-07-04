// The ONLY file that calls Tauri `invoke()` / `listen()`. Everything else in the
// frontend goes through these typed wrappers (CLAUDE.md rule 2).

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import type {
  ApprovalNeededEvent,
  ChatErrorEvent,
  CompleteEvent,
  Config,
  Detection,
  EnvVar,
  ModeEvent,
  OllamaModel,
  PathInfo,
  ProviderProfile,
  ProviderView,
  PullProgress,
  SessionInfo,
  SessionTitleEvent,
  SettingsTarget,
  StackStatus,
  StackStatusPayload,
  TextDeltaEvent,
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
  sendPrompt: (sessionId: string, text: string) => invoke<void>('send_prompt', { sessionId, text }),
  cancelPrompt: (sessionId: string) => invoke<void>('cancel_prompt', { sessionId }),
  setActiveSession: (info: SessionInfo) => invoke<void>('set_active_session', { info }),
  getActiveSession: () => invoke<SessionInfo | null>('get_active_session'),
  respondPermission: (toolCallId: string, optionId: string | null) =>
    invoke<void>('respond_permission', { toolCallId, optionId }),
  setMode: (sessionId: string, modeId: string) => invoke<void>('set_mode', { sessionId, modeId }),
  listSessions: () => invoke<Record<string, unknown>[]>('list_sessions'),
  loadSession: (sessionId: string, cwd: string) =>
    invoke<SessionInfo>('load_session', { sessionId, cwd }),
  deleteSession: (sessionId: string) => invoke<void>('delete_session', { sessionId }),
  forkSession: (sessionId: string, cwd: string, truncateFrom: number | null) =>
    invoke<SessionInfo>('fork_session', { sessionId, cwd, truncateFrom }),
  readTextFile: (path: string) => invoke<string>('read_text_file', { path, maxBytes: null }),
  inspectPaths: (paths: string[]) => invoke<PathInfo[]>('inspect_paths', { paths }),
  openPath: (path: string) => invoke<void>('open_path', { path }),
  revealPath: (path: string) => invoke<void>('reveal_path', { path }),
  // Providers
  listProviders: () => invoke<ProviderView[]>('list_providers'),
  upsertProvider: (profile: ProviderProfile, secret: string | null) =>
    invoke<ProviderProfile>('upsert_provider', { profile, secret }),
  deleteProvider: (id: string) => invoke<void>('delete_provider', { id }),
  activateProvider: (id: string | null) => invoke<void>('activate_provider', { id }),
  // Ollama
  ollamaListModels: () => invoke<OllamaModel[]>('ollama_list_models'),
  ollamaDeleteModel: (model: string) => invoke<void>('ollama_delete_model', { model }),
  ollamaPullModel: (model: string) => invoke<string>('ollama_pull_model', { model }),
  // Ollama env helper
  readOllamaEnv: () => invoke<EnvVar[]>('read_ollama_env'),
  setOllamaEnv: (name: string, value: string | null) =>
    invoke<void>('set_ollama_env', { name, value }),
  restartOllama: () => invoke<void>('restart_ollama'),
  // Extensions
  listExtensions: (sessionId: string) =>
    invoke<Record<string, unknown>[]>('list_extensions', { sessionId }),
  setExtensionEnabled: (sessionId: string, name: string, enabled: boolean) =>
    invoke<void>('set_extension_enabled', { sessionId, name, enabled }),
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
  openWizard: (mode?: 'setup' | 'repair') => invoke<void>('open_wizard', { mode: mode ?? 'setup' }),
  getWizardMode: () => invoke<string | null>('get_wizard_mode'),
  completeSetup: () => invoke<void>('complete_setup'),
  getAutostart: () => invoke<boolean>('get_autostart'),
  setAutostart: (enabled: boolean) => invoke<void>('set_autostart', { enabled }),
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

/** Subscribe to stack status changes. Returns an unlisten fn. */
export function onStackStatus(cb: (payload: StackStatusPayload) => void): Promise<UnlistenFn> {
  return listen<StackStatusPayload>('stack://status', (e) => cb(e.payload));
}

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

export interface ProviderHealth {
  reachable: boolean;
  host?: string;
  name?: string;
}
export const onProviderHealth = (cb: (h: ProviderHealth) => void) =>
  listen<ProviderHealth>('provider://health', (e) => cb(e.payload));

/** The label of the window this webview is running in (`overlay` / `main` / …). */
export const windowLabel = (): string => getCurrentWebview().label;

/** Tray "New Session" → overlay starts a fresh session. */
export const onNewSessionRequest = (cb: () => void) => listen('session://new', () => cb());
