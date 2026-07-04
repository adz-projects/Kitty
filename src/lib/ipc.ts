// The ONLY file that calls Tauri `invoke()` / `listen()`. Everything else in the
// frontend goes through these typed wrappers (CLAUDE.md rule 2).

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  ApprovalNeededEvent,
  ChatErrorEvent,
  CompleteEvent,
  Config,
  ModeEvent,
  SessionInfo,
  SessionTitleEvent,
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
  openSettings: (section?: string) => invoke<void>('open_settings', { section: section ?? null }),
  openMain: () => invoke<void>('open_main'),
  getStackStatus: () => invoke<StackStatus>('get_stack_status'),
  restartGoosed: () => invoke<void>('restart_goosed'),
  newSession: () => invoke<SessionInfo>('new_session'),
  sendPrompt: (sessionId: string, text: string) => invoke<void>('send_prompt', { sessionId, text }),
  setActiveSession: (info: SessionInfo) => invoke<void>('set_active_session', { info }),
  getActiveSession: () => invoke<SessionInfo | null>('get_active_session'),
  respondPermission: (toolCallId: string, optionId: string | null) =>
    invoke<void>('respond_permission', { toolCallId, optionId }),
  setMode: (sessionId: string, modeId: string) => invoke<void>('set_mode', { sessionId, modeId }),
};

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

export const onApprovalNeeded = (cb: (e: ApprovalNeededEvent) => void) =>
  listen<ApprovalNeededEvent>('chat://tool-approval-needed', (e) => cb(e.payload));
export const onMode = (cb: (e: ModeEvent) => void) =>
  listen<ModeEvent>('chat://mode', (e) => cb(e.payload));

/** Tray "New Session" → overlay starts a fresh session. */
export const onNewSessionRequest = (cb: () => void) => listen('session://new', () => cb());
