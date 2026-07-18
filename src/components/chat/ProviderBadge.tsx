import { useEffect, useState } from 'react';
import { ipc, onProviderActivated } from '@/lib/ipc';
import { TrustIcon } from '@/lib/provider_trust';
import type { ProviderView } from '@/lib/types';
import { usePopoverPosition } from '@/lib/usePopoverPosition';
import { SettingsGearIcon } from '@/components/icons/SettingsGearIcon';

/** Active-provider badge with a click-to-switch popover (Round-2 item 9), shown
    in both the overlay and full window. Switching calls activate_provider, which
    health-gates the target first (rejects and stays on the old provider if it
    isn't reachable/authenticated) then respawns goosed and emits
    provider://activated — the store re-syncs from that.
    Note: switching mid-conversation restarts goosed; use "New chat" to continue. */
export function ProviderBadge() {
  const [providers, setProviders] = useState<ProviderView[]>([]);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [switchError, setSwitchError] = useState<string | null>(null);
  const { triggerRef, popoverRef, style } = usePopoverPosition(open, () => setOpen(false));

  const load = () =>
    ipc
      .listProviders()
      .then(setProviders)
      .catch(() => {});
  useEffect(() => {
    void load();
    const un = onProviderActivated(() => void load());
    return () => void un.then((fn) => fn());
  }, []);

  const active = providers.find((p) => p.active);
  const label = active ? active.name || active.provider_type : 'Goose default';
  const icon = active ? (
    <TrustIcon tier={active.network_tier} isTrusted={active.is_trusted} />
  ) : (
    <SettingsGearIcon />
  );

  const switchTo = async (id: string | null) => {
    setOpen(false);
    setBusy(true);
    setSwitchError(null);
    try {
      await ipc.activateProvider(id);
    } catch (e) {
      // A real, actionable failure (e.g. the health-gate rejected the switch) —
      // surface it here instead of swallowing it silently.
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
        title="Provider — click to switch (restarts the agent)"
        disabled={busy}
      >
        {icon} <span className="provider-badge-label">{busy ? 'switching…' : label}</span> ▾
      </button>
      {open && (
        <div ref={popoverRef} className="mode-popover" role="menu" style={style}>
          {providers.map((p) => (
            <button
              key={p.id}
              role="menuitemradio"
              aria-checked={p.active}
              className={p.active ? 'active' : ''}
              title={p.base_url}
              onClick={() => void switchTo(p.id)}
            >
              <TrustIcon tier={p.network_tier} isTrusted={p.is_trusted} />{' '}
              {p.name || p.provider_type}
            </button>
          ))}
          {providers.some((p) => p.active) && (
            <button onClick={() => void switchTo(null)}>
              <SettingsGearIcon /> Goose default
            </button>
          )}
          {providers.length === 0 && <span className="muted">No providers configured</span>}
        </div>
      )}
      {switchError && (
        <div
          className="chat-error"
          role="alert"
          style={{ position: 'absolute', top: '100%', right: 0, zIndex: 20 }}
        >
          {switchError}{' '}
          <button className="link" onClick={() => setSwitchError(null)}>
            Dismiss
          </button>
        </div>
      )}
    </div>
  );
}
