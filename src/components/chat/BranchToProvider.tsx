import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { needsHandoffGate, rememberedChoice, type HandoffChoice } from '@/lib/handoff';
import { TrustIcon } from '@/lib/provider_trust';
import { Dialog } from '@/components/shared/Dialog';
import { useChatStore } from '@/stores/chatStore';
import { useSessionStore } from '@/stores/sessionStore';
import type { ProviderView } from '@/lib/types';

function hostOf(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

/** Move the open chat onto another provider card. A chat stays on its own
    card (decision #24), so this is a branch: the conversation continues in a
    new chat on the target, and the original is left as it was. Moving to a
    less-trusted card goes through the handoff gate first (decision #16).
    `onDone` runs once the new chat is open. */
export function useBranchToProvider(onDone?: () => void) {
  const [gateFor, setGateFor] = useState<ProviderView | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const run = async (target: ProviderView, choice: HandoffChoice) => {
    const { sessionId, cwd, title } = useChatStore.getState();
    if (!sessionId || !cwd) return;
    setBusy(true);
    setError(null);
    try {
      const info = await ipc.branchToProvider(sessionId, cwd, target.id, choice === 'keep');
      await useChatStore
        .getState()
        .loadSession(
          info.session_id,
          info.cwd,
          choice === 'keep' && title ? `Branch of ${title}` : undefined,
          target.id,
          target.models[0]
        );
      void useSessionStore.getState().refresh();
      onDone?.();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const start = async (target: ProviderView, current: ProviderView | undefined) => {
    setError(null);
    if (!needsHandoffGate(current, target)) return run(target, 'keep');
    let remembered: HandoffChoice | null = null;
    try {
      remembered = rememberedChoice((await ipc.getConfig()).handoff_gate_choice);
    } catch {
      // Unreadable config: ask, which is the safe default.
    }
    if (remembered) return run(target, remembered);
    setGateFor(target);
  };

  const gate = gateFor ? (
    <HandoffGateDialog
      target={gateFor}
      onCancel={() => setGateFor(null)}
      onChoose={(choice, remember) => {
        const target = gateFor;
        setGateFor(null);
        if (remember) {
          void ipc.patchConfig({ handoff_gate_choice: choice }).catch((e) => setError(String(e)));
        }
        void run(target, choice);
      }}
    />
  ) : null;

  return { start, busy, error, clearError: () => setError(null), gate };
}

/** Blocking choice before a conversation goes somewhere less trusted: send
    it along, or start clean there. */
export function HandoffGateDialog({
  target,
  onChoose,
  onCancel,
}: {
  target: ProviderView;
  onChoose: (choice: HandoffChoice, remember: boolean) => void;
  onCancel: () => void;
}) {
  const [remember, setRemember] = useState(false);
  const host = hostOf(target.base_url);
  return (
    <Dialog title={`Send this conversation to ${host}?`} onClose={onCancel} alert>
      <p style={{ margin: 0 }}>
        <strong>{target.name}</strong> is less trusted than the provider this chat is on. Keeping
        the context sends the whole conversation so far — messages, pasted documents and tool
        results — to <strong>{host}</strong>. Starting clean opens an empty chat there instead.
        Either way this chat stays as it is.
      </p>
      <label className="check">
        <input type="checkbox" checked={remember} onChange={(e) => setRemember(e.target.checked)} />
        <span>Remember my choice (reset it in Settings → Advanced)</span>
      </label>
      <div className="modal-actions">
        <button onClick={onCancel}>Cancel</button>
        <button onClick={() => onChoose('clean', remember)}>Start clean</button>
        <button className="primary" onClick={() => onChoose('keep', remember)}>
          Keep context (send it)
        </button>
      </div>
    </Dialog>
  );
}

/** Pick a card to branch this chat onto — from the exhausted-credits error
    card, where the chat cannot continue where it is. */
export function BranchProviderDialog({ onClose }: { onClose: () => void }) {
  const [providers, setProviders] = useState<ProviderView[]>([]);
  const sessionProviderId = useChatStore((s) => s.sessionProviderId);
  const flow = useBranchToProvider(onClose);

  useEffect(() => {
    void ipc
      .listProviders()
      .then(setProviders)
      .catch(() => {});
  }, []);

  const current = providers.find((p) => p.id === sessionProviderId);
  const others = providers.filter((p) => p.id !== sessionProviderId && !p.disabled_reason);

  if (flow.gate) return flow.gate;
  return (
    <Dialog title="Continue on another provider" onClose={onClose}>
      <p className="muted" style={{ margin: 0 }}>
        The conversation continues in a new chat on the provider you pick. This chat stays as it is.
      </p>
      <div className="provider-choice-list">
        {others.map((p) => (
          <button
            key={p.id}
            className="provider-option"
            disabled={flow.busy}
            onClick={() => void flow.start(p, current)}
          >
            <TrustIcon tier={p.network_tier} isTrusted={p.is_trusted} />
            <span className="provider-option-name">{p.name || p.provider_type}</span>
          </button>
        ))}
        {others.length === 0 && <p className="muted">No other providers are set up.</p>}
      </div>
      {flow.error && <p className="error">{flow.error}</p>}
      <div className="modal-actions">
        <button onClick={onClose}>Cancel</button>
      </div>
    </Dialog>
  );
}
