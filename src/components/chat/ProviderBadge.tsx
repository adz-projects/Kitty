import { useEffect, useState } from 'react';
import { ipc, onProviderActivated } from '@/lib/ipc';
import { trustIcon } from '@/lib/provider_trust';
import type { ProviderView } from '@/lib/types';

/** Active-provider badge with a click-to-switch popover (Round-2 item 9), shown
    in both the overlay and full window. Switching calls activate_provider, which
    respawns goosed and emits provider://activated — the store re-syncs from that.
    Note: switching mid-conversation restarts goosed; use "New chat" to continue. */
export function ProviderBadge() {
  const [providers, setProviders] = useState<ProviderView[]>([]);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);

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
  const icon = active ? trustIcon(active.network_tier, active.is_trusted) : '⚙';

  const switchTo = async (id: string | null) => {
    setOpen(false);
    setBusy(true);
    try {
      await ipc.activateProvider(id);
    } catch {
      /* surfaced elsewhere; keep the badge quiet */
    } finally {
      setBusy(false);
    }
  };

  return (
    <div style={{ position: 'relative' }}>
      <button
        className="status-badge"
        onClick={() => setOpen((o) => !o)}
        title="Provider — click to switch (restarts the agent)"
        disabled={busy}
      >
        {icon} {busy ? 'switching…' : label} ▾
      </button>
      {open && (
        <div className="mode-popover" role="menu">
          {providers.map((p) => (
            <button
              key={p.id}
              role="menuitemradio"
              aria-checked={p.active}
              className={p.active ? 'active' : ''}
              title={p.base_url}
              onClick={() => void switchTo(p.id)}
            >
              {trustIcon(p.network_tier, p.is_trusted)} {p.name || p.provider_type}
            </button>
          ))}
          {providers.some((p) => p.active) && (
            <button onClick={() => void switchTo(null)}>⚙ Goose default</button>
          )}
          {providers.length === 0 && <span className="muted">No providers configured</span>}
        </div>
      )}
    </div>
  );
}
