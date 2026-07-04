// Chat render state (Phase 2). Holds the active session and the message list,
// assembled live from `chat://*` events. Per CLAUDE.md rule 3, this is render
// state only — the durable conversation lives in goosed.

import { create } from 'zustand';
import {
  ipc,
  onApprovalNeeded,
  onChatError,
  onComplete,
  onMessageDelta,
  onMode,
  onReasoningDelta,
  onSessionTitle,
  onToolCall,
} from '@/lib/ipc';
import type { ApprovalNeededEvent, ModeInfo, ToolCallUpdate } from '@/lib/types';

export interface ToolCall {
  id: string;
  title: string;
  status: string;
  input?: unknown;
  output?: unknown;
}

export interface Message {
  id: string;
  role: 'user' | 'assistant';
  text: string;
  reasoning: string;
  toolCalls: ToolCall[];
  streaming: boolean;
}

interface ChatState {
  sessionId: string | null;
  cwd: string | null;
  title: string | null;
  mode: string | null;
  availableModes: ModeInfo[];
  messages: Message[];
  pendingApprovals: ApprovalNeededEvent[];
  busy: boolean;
  error: string | null;
  bindEvents: () => void;
  newSession: () => Promise<void>;
  ensureSession: () => Promise<string>;
  send: (text: string) => Promise<void>;
  respondApproval: (toolCallId: string, optionId: string | null) => Promise<void>;
  setMode: (modeId: string) => Promise<void>;
  adoptSession: (info: {
    session_id: string;
    cwd: string;
    current_mode: string;
    available_modes: ModeInfo[];
  }) => void;
}

let bound = false;
let msgSeq = 0;
const newId = () => `m${Date.now()}_${++msgSeq}`;

export const useChatStore = create<ChatState>((set, get) => ({
  sessionId: null,
  cwd: null,
  title: null,
  mode: null,
  availableModes: [],
  messages: [],
  pendingApprovals: [],
  busy: false,
  error: null,

  adoptSession: (info) =>
    set({
      sessionId: info.session_id,
      cwd: info.cwd,
      mode: info.current_mode,
      availableModes: info.available_modes,
    }),

  newSession: async () => {
    const info = await ipc.newSession();
    set({
      sessionId: info.session_id,
      cwd: info.cwd,
      mode: info.current_mode,
      availableModes: info.available_modes,
      title: null,
      messages: [],
      pendingApprovals: [],
      error: null,
      busy: false,
    });
  },

  respondApproval: async (toolCallId: string, optionId: string | null) => {
    set((s) => ({
      pendingApprovals: s.pendingApprovals.filter((a) => a.tool_call_id !== toolCallId),
    }));
    try {
      await ipc.respondPermission(toolCallId, optionId);
    } catch (e) {
      set({ error: String(e) });
    }
  },

  setMode: async (modeId: string) => {
    const sid = get().sessionId;
    if (!sid) return;
    set({ mode: modeId });
    try {
      await ipc.setMode(sid, modeId);
    } catch (e) {
      set({ error: String(e) });
    }
  },

  ensureSession: async () => {
    const current = get().sessionId;
    if (current) return current;
    await get().newSession();
    return get().sessionId!;
  },

  send: async (text: string) => {
    const trimmed = text.trim();
    if (!trimmed || get().busy) return;
    const sessionId = await get().ensureSession();
    const userMsg: Message = {
      id: newId(),
      role: 'user',
      text: trimmed,
      reasoning: '',
      toolCalls: [],
      streaming: false,
    };
    const assistantMsg: Message = {
      id: newId(),
      role: 'assistant',
      text: '',
      reasoning: '',
      toolCalls: [],
      streaming: true,
    };
    set((s) => ({ messages: [...s.messages, userMsg, assistantMsg], busy: true, error: null }));
    try {
      await ipc.sendPrompt(sessionId, trimmed);
    } catch (e) {
      set({ busy: false, error: String(e) });
    }
  },

  bindEvents: () => {
    if (bound) return;
    bound = true;

    const forActive = (sid: string) => get().sessionId === sid;

    const patchStreaming = (fn: (m: Message) => Message) =>
      set((s) => {
        const idx = [...s.messages]
          .reverse()
          .findIndex((m) => m.role === 'assistant' && m.streaming);
        if (idx === -1) return {};
        const realIdx = s.messages.length - 1 - idx;
        const messages = s.messages.slice();
        messages[realIdx] = fn(messages[realIdx]);
        return { messages };
      });

    void onMessageDelta((e) => {
      if (!forActive(e.session_id)) return;
      patchStreaming((m) => ({ ...m, text: m.text + e.text }));
    });

    void onReasoningDelta((e) => {
      if (!forActive(e.session_id)) return;
      patchStreaming((m) => ({ ...m, reasoning: m.reasoning + e.text }));
    });

    void onToolCall((e) => {
      if (!forActive(e.session_id)) return;
      const u: ToolCallUpdate = e.update;
      const id = String(u.toolCallId ?? '');
      patchStreaming((m) => {
        const toolCalls = m.toolCalls.slice();
        const existing = toolCalls.findIndex((t) => t.id === id);
        const prev = existing >= 0 ? toolCalls[existing] : undefined;
        // Keep the first meaningful title; apply status/output only when present.
        const merged: ToolCall = {
          id,
          title: prev?.title ?? String(u.title ?? u.kind ?? 'tool'),
          status:
            u.status != null
              ? String(u.status)
              : (prev?.status ?? (e.phase === 'tool_call' ? 'pending' : 'running')),
          input: u.rawInput ?? prev?.input,
          output: u.rawOutput ?? u.content ?? prev?.output,
        };
        if (existing >= 0) toolCalls[existing] = merged;
        else toolCalls.push(merged);
        return { ...m, toolCalls };
      });
    });

    void onSessionTitle((e) => {
      if (forActive(e.session_id)) set({ title: e.title });
    });

    void onApprovalNeeded((e) => {
      if (!forActive(e.session_id)) return;
      set((s) =>
        s.pendingApprovals.some((a) => a.tool_call_id === e.tool_call_id)
          ? {}
          : { pendingApprovals: [...s.pendingApprovals, e] }
      );
    });

    void onMode((e) => {
      if (forActive(e.session_id)) set({ mode: e.mode });
    });

    void onComplete((e) => {
      if (!forActive(e.session_id)) return;
      patchStreaming((m) => ({ ...m, streaming: false }));
      set({ busy: false, pendingApprovals: [] });
    });

    void onChatError((e) => {
      if (!forActive(e.session_id)) return;
      patchStreaming((m) => ({ ...m, streaming: false }));
      set({ busy: false, error: e.message, pendingApprovals: [] });
    });
  },
}));
