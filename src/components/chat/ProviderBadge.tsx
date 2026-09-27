import { useEffect, useState } from 'react';
import { ipc, onProviderActivated } from '@/lib/ipc';
import { TrustIcon } from '@/lib/provider_trust';
import type { ProviderView } from '@/lib/types';
import { usePopoverPosition } from '@/lib/usePopoverPosition';
import { SettingsGearIcon } from '@/components/icons/SettingsGearIcon';
import { useChatStore } from '@/stores/chatStore';
import { useBranchToProvider } from './BranchToProvider';

/** This chat's provider card, with a popover to change it (shown in both the
    overlay and the full window). Only this chat changes — the default for new
    chats is set in Settings (decision #50).

    An empty chat just switches (`set_chat_provider`, which checks the card
    works first). A chat with history stays on its card (decision #24): the
    popover offers to continue it on another card instead, as a branch, going
    through the handoff gate when the target is less trusted. */
export function ProviderBadge() {
  const [providers, setProviders] = useState<ProviderView[]>([]);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [switchError, setSwitchError] = useState<string | null>(null);
  const { triggerRef, popoverRef, style } = usePopoverPosition(open, () => setOpen(false));
  const locked = useChatStore((s) => s.messages.length > 0);
  const branchFlow = useBranchToProvider();
  const sessionId = useChatStore((s) => s.sessionId);
  const sessionProviderId = useChatStore((s) => s.sessionProviderId);

  const load = () =>
    // Best-effort: a failure just leaves the switch-provider dropdown empty
    // until the popover is reopened.
    ipc
      .listProviders()
      .then(setProviders)
      .catch(() => {});
  useEffect(() => {
    void load();
    const un = onProviderActivated(() => void load());
    return () => void un.then((fn) => fn());
  }, []);

  // Same resolution as chatStore.refreshProvider (release-fixes item 1): a
  // window with a live session shows *that session's* stamped provider, not
  // the global `p.active` flag other windows' switches also flip — otherwise
  // switching providers in one window visibly bled into every other open
  // window's badge.
  const active =
    sessionId !== null && sessionProviderId
      ? (providers.find((p) => p.id === sessionProviderId) ?? providers.find((p) => p.active))
      : providers.find((p) => p.active);
  const activeId = active?.id;
  const label = active ? active.name || active.provider_type : 'No provider';
  const icon = active ? (
    <TrustIcon tier={active.network_tier} isTrusted={active.is_trusted} />
  ) : (
    <SettingsGearIcon />
  );

  const switchTo = async (id: string) => {
    setOpen(false);
    if (locked) {
      const target = providers.find((p) => p.id === id);
      if (target) void branchFlow.start(target, active);
      return;
    }
    setBusy(true);
    setSwitchError(null);
    try {
      // A blank chat may not exist on the engine yet; make it, so the card
      // is this chat's own and not the default's.
      const sid = await useChatStore.getState().ensureSession();
      await ipc.setChatProvider(sid, id);
      const target = providers.find((p) => p.id === id);
      useChatStore.setState({ sessionProviderId: id, sessionModelId: target?.models[0] ?? null });
      void useChatStore.getState().refreshProvider();
    } catch (e) {
      // e.g. the card failed its connection check: say so, stay put.
      setSwitchError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div style={{ position: 'relative' }}>
      <button
        ref={triggerRef as React.Ref<HTMLButtonElement>}
        className="status-badge provider-badge"
        onClick={() => setOpen((o) => !o)}
        title={
          locked
            ? 'This chat’s provider — click to continue the conversation on another one'
            : 'This chat’s provider — click to switch'
        }
        disabled={busy || branchFlow.busy}
      >
        {icon}{' '}
        <span className="provider-badge-label">
          {busy ? 'switching…' : branchFlow.busy ? 'branching…' : label}
        </span>{' '}
        ▾
      </button>
      {open && (
        <div ref={popoverRef} className="mode-popover provider-popover" role="menu" style={style}>
          {locked && <div className="muted popover-heading">Continue this chat on…</div>}
          {providers
            .filter((p) => !p.disabled_reason && !(locked && p.id === activeId))
            .map((p) => (
              <button
                key={p.id}
                role="menuitemradio"
                aria-checked={p.id === activeId}
                className={`provider-option${p.id === activeId ? ' active' : ''}`}
                title={p.base_url}
                onClick={() => void switchTo(p.id)}
              >
                {/* Icon and name are separate flex children rather than one run
                  of inline content, so a name too long for the row wraps to a
                  hanging indent under the name instead of under the icon
                  (see `.provider-option` in base.css). */}
                <TrustIcon tier={p.network_tier} isTrusted={p.is_trusted} />
                <span className="provider-option-name">{p.name || p.provider_type}</span>
              </button>
            ))}
          {providers.length === 0 && <span className="muted">No providers configured</span>}
        </div>
      )}
      {branchFlow.gate}
      {(switchError ?? branchFlow.error) && (
        <div
          className="chat-error"
          role="alert"
          style={{ position: 'absolute', top: '100%', right: 0, zIndex: 20 }}
        >
          {switchError ?? branchFlow.error}{' '}
          <button
            className="link"
            onClick={() => {
              setSwitchError(null);
              branchFlow.clearError();
            }}
          >
            Dismiss
          </button>
        </div>
      )}
    </div>
  );
}
