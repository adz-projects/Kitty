// Chat render state. Assembled live from `chat://*` events; the durable
// conversation lives in goosed (CLAUDE.md rule 3). The assembly is turn-aware so
// it handles both live prompting and full-conversation replay on session/load.

import { create } from 'zustand';
import {
  ipc,
  onApprovalNeeded,
  onChatError,
  onComplete,
  onMessageDelta,
  onMode,
  onProviderHealth,
  onReasoningDelta,
  onSessionTitle,
  onToolCall,
  onUserMessage,
  windowLabel,
} from '@/lib/ipc';
import type {
  ApprovalNeededEvent,
  ModeInfo,
  NetworkTier,
  PathInfo,
  ToolCallUpdate,
} from '@/lib/types';

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
  /** Currently being appended to (internal to the assembly). */
  open: boolean;
}

export interface Artifact {
  path: string;
  name: string;
  tool: string;
}

/** An inlined document (large paste or dropped text file) in chat-only mode. */
export interface Attachment {
  id: string;
  label: string;
  content: string;
}

interface ChatState {
  sessionId: string | null;
  cwd: string | null;
  title: string | null;
  mode: string | null;
  availableModes: ModeInfo[];
  messages: Message[];
  artifacts: Artifact[];
  droppedFiles: PathInfo[];
  attachments: Attachment[];
  pendingApprovals: ApprovalNeededEvent[];
  busy: boolean;
  error: string | null;
  // Active-provider derived state (Phase 9/10)
  toolsEnabled: boolean;
  providerTier: NetworkTier | null;
  providerHost: string | null;
  providerOffline: boolean;
  model: string | null;
  bindEvents: () => void;
  refreshProvider: () => Promise<void>;
  branch: (uiIndex: number) => Promise<void>;
  regenerate: (assistantIndex: number) => Promise<void>;
  addPastedText: (text: string, label?: string) => void;
  removeAttachment: (id: string) => void;
  newSession: (cwd?: string) => Promise<void>;
  ensureSession: () => Promise<string>;
  loadSession: (sessionId: string, cwd: string, title?: string) => Promise<void>;
  reloadCurrent: () => Promise<void>;
  send: (text: string) => Promise<void>;
  cancel: () => Promise<void>;
  respondApproval: (toolCallId: string, optionId: string | null) => Promise<void>;
  setMode: (modeId: string) => Promise<void>;
  addDroppedPaths: (paths: string[]) => Promise<void>;
  removeDroppedPath: (path: string) => void;
  setWorkingDir: (folder: string) => Promise<void>;
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

const ARTIFACT_RE = /text_editor|write|create|edit|str_replace/i;

function deriveArtifact(u: ToolCallUpdate): Artifact | null {
  const meta = u as Record<string, unknown>;
  const goose = (meta._meta as { goose?: { toolCall?: { toolName?: string } } })?.goose;
  const toolName = goose?.toolCall?.toolName ?? '';
  const label = `${u.title ?? ''} ${toolName}`;
  if (!ARTIFACT_RE.test(label)) return null;
  const input = u.rawInput as { path?: string; file_path?: string; paths?: string[] } | undefined;
  const p =
    input?.path ?? input?.file_path ?? (Array.isArray(input?.paths) ? input?.paths[0] : undefined);
  if (typeof p !== 'string' || !p) return null;
  return {
    path: p,
    name: p.split(/[\\/]/).pop() || p,
    tool: toolName || String(u.title || 'tool'),
  };
}

const closeOpen = (msgs: Message[]): Message[] =>
  msgs.map((m) => (m.open ? { ...m, open: false, streaming: false } : m));

export const useChatStore = create<ChatState>((set, get) => ({
  sessionId: null,
  cwd: null,
  title: null,
  mode: null,
  availableModes: [],
  messages: [],
  artifacts: [],
  droppedFiles: [],
  attachments: [],
  pendingApprovals: [],
  busy: false,
  error: null,
  toolsEnabled: true,
  providerTier: null,
  providerHost: null,
  providerOffline: false,
  model: null,

  refreshProvider: async () => {
    try {
      const providers = await ipc.listProviders();
      const active = providers.find((p) => p.active);
      set({
        toolsEnabled: active ? active.tools_enabled : true,
        providerTier: active ? active.network_tier : null,
        providerHost: active ? new URL(active.base_url).host : null,
        model: active?.models[0] ?? null,
      });
    } catch {
      set({ toolsEnabled: true, providerTier: null, providerHost: null, model: null });
    }
  },

  adoptSession: (info) =>
    set({
      sessionId: info.session_id,
      cwd: info.cwd,
      mode: info.current_mode,
      availableModes: info.available_modes,
    }),

  newSession: async (cwd?: string) => {
    const info = await ipc.newSession(cwd);
    set({
      sessionId: info.session_id,
      cwd: info.cwd,
      mode: info.current_mode,
      availableModes: info.available_modes,
      title: null,
      messages: [],
      artifacts: [],
      attachments: [],
      pendingApprovals: [],
      error: null,
      busy: false,
    });
    await get().refreshProvider();
  },

  ensureSession: async () => {
    const current = get().sessionId;
    if (current) return current;
    await get().newSession();
    return get().sessionId!;
  },

  loadSession: async (sessionId: string, cwd: string, title?: string) => {
    // Set the id first so replayed events (which arrive during the call) match.
    set({
      sessionId,
      cwd,
      title: title ?? null,
      messages: [],
      artifacts: [],
      pendingApprovals: [],
      error: null,
      busy: true,
    });
    try {
      const info = await ipc.loadSession(sessionId, cwd);
      set({ mode: info.current_mode, availableModes: info.available_modes });
      await get().refreshProvider();
    } catch (e) {
      set({ error: String(e) });
    } finally {
      set((s) => ({ busy: false, messages: closeOpen(s.messages) }));
    }
  },

  cancel: async () => {
    const sid = get().sessionId;
    if (!sid || !get().busy) return;
    try {
      await ipc.cancelPrompt(sid);
      // Optimistically release the UI; goosed can take a moment to wind down the
      // turn on a slow model, and its later completion event is idempotent.
      set((s) => ({ busy: false, messages: closeOpen(s.messages) }));
    } catch (e) {
      set({ error: String(e) });
    }
  },

  reloadCurrent: async () => {
    const { sessionId, cwd, title } = get();
    if (sessionId && cwd) await get().loadSession(sessionId, cwd, title ?? undefined);
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

  addDroppedPaths: async (paths: string[]) => {
    if (!paths.length) return;
    try {
      const infos = await ipc.inspectPaths(paths);
      // Chat-only (Phase 9): inline file *content* rather than sending paths
      // (there's no filesystem tool to hand a path to). Separate code path.
      if (!get().toolsEnabled) {
        for (const f of infos) {
          if (f.is_dir) continue;
          try {
            const content = await ipc.readTextFile(f.path);
            get().addPastedText(content, f.name);
          } catch (e) {
            set({ error: String(e) });
          }
        }
        return;
      }
      set((s) => {
        const seen = new Set(s.droppedFiles.map((f) => f.path));
        return { droppedFiles: [...s.droppedFiles, ...infos.filter((f) => !seen.has(f.path))] };
      });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  removeDroppedPath: (path: string) =>
    set((s) => ({ droppedFiles: s.droppedFiles.filter((f) => f.path !== path) })),

  setWorkingDir: async (folder: string) => {
    await get().newSession(folder);
  },

  addPastedText: (text: string, label?: string) =>
    set((s) => ({
      attachments: [
        ...s.attachments,
        {
          id: newId(),
          label: label ?? `Pasted text — ${text.trim().split(/\s+/).length} words`,
          content: text,
        },
      ],
    })),

  removeAttachment: (id: string) =>
    set((s) => ({ attachments: s.attachments.filter((a) => a.id !== id) })),

  branch: async (uiIndex: number) => {
    const { sessionId, cwd, title } = get();
    if (!sessionId || !cwd) return;
    try {
      // Keep history up to and including the clicked message, diverge after.
      const info = await ipc.forkSession(sessionId, cwd, uiIndex + 1);
      set({ title: title ? `Branch of ${title}` : 'Branch' });
      await get().loadSession(info.session_id, info.cwd, get().title ?? undefined);
    } catch (e) {
      set({ error: String(e) });
    }
  },

  regenerate: async (assistantIndex: number) => {
    const { sessionId, cwd, messages } = get();
    if (!sessionId || !cwd) return;
    // Find the user message preceding this assistant turn.
    let userIdx = assistantIndex - 1;
    while (userIdx >= 0 && messages[userIdx].role !== 'user') userIdx--;
    if (userIdx < 0) return;
    const userText = messages[userIdx].text;
    try {
      // Fork + truncate to just before the user turn so the original response is
      // preserved in the parent session; then resend.
      const info = await ipc.forkSession(sessionId, cwd, userIdx);
      await get().loadSession(info.session_id, info.cwd, get().title ?? undefined);
      await get().send(userText);
    } catch (e) {
      set({ error: String(e) });
    }
  },

  send: async (text: string) => {
    const trimmed = text.trim();
    const attachments = get().attachments;
    if ((!trimmed && attachments.length === 0) || get().busy) return;
    const firstMessage = get().messages.length === 0;
    const chatOnly = !get().toolsEnabled;
    const sessionId = await get().ensureSession();
    const files = get().droppedFiles;

    let promptText = trimmed;
    if (chatOnly && attachments.length) {
      // Inline document content directly (no filesystem tool in chat-only mode).
      const docs = attachments.map((a) => `--- ${a.label} ---\n${a.content}`).join('\n\n');
      promptText = `${docs}\n\n${trimmed}`.trim();
    } else if (files.length) {
      // Agentic: hand paths to the filesystem tools (CLAUDE.md §5).
      const block = 'Files provided by the user:\n' + files.map((f) => `- ${f.path}`).join('\n');
      promptText = `${block}\n\n${trimmed}`;
    }

    const userMsg: Message = {
      id: newId(),
      role: 'user',
      text: trimmed || (attachments.length ? `(${attachments.length} document(s))` : ''),
      reasoning: '',
      toolCalls: [],
      streaming: false,
      open: false,
    };
    set((s) => ({
      messages: [...s.messages, userMsg],
      droppedFiles: [],
      attachments: [],
      busy: true,
      error: null,
    }));
    try {
      await ipc.sendPrompt(sessionId, promptText);
      // Chat-only: auto-promote the *first* overlay message to the full window.
      if (chatOnly && firstMessage && windowLabel() === 'overlay') {
        const s = get();
        await ipc.setActiveSession({
          session_id: s.sessionId!,
          cwd: s.cwd ?? '',
          current_mode: s.mode ?? 'auto',
          available_modes: s.availableModes,
        });
        await ipc.openMain();
        await ipc.hideOverlay();
      }
    } catch (e) {
      set({ busy: false, error: String(e) });
    }
  },

  bindEvents: () => {
    if (bound) return;
    bound = true;

    const forActive = (sid: string) => get().sessionId === sid;

    // Append a text/reasoning chunk to the open message of `role`, opening a new
    // one (and closing any prior open message) when the turn changes.
    const appendChunk = (role: 'user' | 'assistant', field: 'text' | 'reasoning', text: string) =>
      set((s) => {
        const msgs = s.messages.slice();
        const last = msgs[msgs.length - 1];
        if (last && last.role === role && last.open) {
          msgs[msgs.length - 1] = { ...last, [field]: last[field] + text };
          return { messages: msgs };
        }
        const closed = closeOpen(msgs);
        closed.push({
          id: newId(),
          role,
          text: field === 'text' ? text : '',
          reasoning: field === 'reasoning' ? text : '',
          toolCalls: [],
          streaming: role === 'assistant',
          open: true,
        });
        return { messages: closed };
      });

    void onMessageDelta((e) => {
      if (forActive(e.session_id)) appendChunk('assistant', 'text', e.text);
    });
    void onReasoningDelta((e) => {
      if (forActive(e.session_id)) appendChunk('assistant', 'reasoning', e.text);
    });
    void onUserMessage((e) => {
      if (forActive(e.session_id)) appendChunk('user', 'text', e.text);
    });

    void onToolCall((e) => {
      if (!forActive(e.session_id)) return;
      const u: ToolCallUpdate = e.update;
      const id = String(u.toolCallId ?? '');
      const artifact = deriveArtifact(u);
      set((s) => {
        let msgs = s.messages.slice();
        let last = msgs[msgs.length - 1];
        if (!(last && last.role === 'assistant' && last.open)) {
          msgs = closeOpen(msgs);
          last = {
            id: newId(),
            role: 'assistant',
            text: '',
            reasoning: '',
            toolCalls: [],
            streaming: true,
            open: true,
          };
          msgs.push(last);
        } else {
          last = { ...last };
          msgs[msgs.length - 1] = last;
        }
        const toolCalls = last.toolCalls.slice();
        const existing = toolCalls.findIndex((t) => t.id === id);
        const prev = existing >= 0 ? toolCalls[existing] : undefined;
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
        last.toolCalls = toolCalls;

        const artifacts =
          artifact && !s.artifacts.some((a) => a.path === artifact.path)
            ? [...s.artifacts, artifact]
            : s.artifacts;
        return { messages: msgs, artifacts };
      });
    });

    void onSessionTitle((e) => {
      if (forActive(e.session_id)) set({ title: e.title });
    });
    void onMode((e) => {
      if (forActive(e.session_id)) set({ mode: e.mode });
    });

    void onProviderHealth((h) => {
      set({ providerOffline: !h.reachable, providerHost: h.host ?? get().providerHost });
    });
    void onApprovalNeeded((e) => {
      if (!forActive(e.session_id)) return;
      set((s) =>
        s.pendingApprovals.some((a) => a.tool_call_id === e.tool_call_id)
          ? {}
          : { pendingApprovals: [...s.pendingApprovals, e] }
      );
    });

    void onComplete((e) => {
      if (!forActive(e.session_id)) return;
      set((s) => ({ busy: false, pendingApprovals: [], messages: closeOpen(s.messages) }));
    });
    void onChatError((e) => {
      if (!forActive(e.session_id)) return;
      set((s) => ({
        busy: false,
        error: e.message,
        pendingApprovals: [],
        messages: closeOpen(s.messages),
      }));
    });
  },
}));
