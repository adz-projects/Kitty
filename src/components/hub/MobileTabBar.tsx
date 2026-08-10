import { MOBILE_TABS, useRouteStore, type HubView } from '@/stores/routeStore';

/** Bottom tab bar for the Android shell (docs/ANDROID.md §8.2).
 *
 * Rendered on every platform and hidden by CSS above the mobile breakpoint,
 * rather than branched on in JS. That keeps one component tree across both
 * targets — the point of the hub — and means a narrow desktop window behaves
 * predictably instead of hitting a code path only phones ever run.
 *
 * The wizard is not a tab (see `MOBILE_TABS`), so while it is showing, no tab
 * is active. That is correct: first run should not offer an escape hatch into
 * a half-configured app. */
export function MobileTabBar() {
  const view = useRouteStore((s) => s.view);
  const goto = useRouteStore((s) => s.goto);

  return (
    <nav className="mobile-tabs" aria-label="Main">
      {MOBILE_TABS.map((t) => (
        <button
          key={t.view}
          className={t.view === view ? 'active' : ''}
          aria-current={t.view === view ? 'page' : undefined}
          onClick={() => goto(t.view as HubView)}
        >
          {t.label}
        </button>
      ))}
    </nav>
  );
}
