import { Fragment, useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useRouteStore } from '@/stores/routeStore';
import { isAndroid } from '@/lib/platform';
import { KittyIcon } from '@/components/icons/KittyIcon';
import { General } from '@/components/settings/General';
import { Providers } from '@/components/settings/Providers';
import { HelperModels } from '@/components/settings/HelperModels';
import { McpServers } from '@/components/settings/McpServers';
import { NotificationsSection } from '@/components/settings/NotificationsSection';
import { Appearance } from '@/components/settings/Appearance';
import { Advanced } from '@/components/settings/Advanced';
import { AdaptivePathway } from '@/components/settings/AdaptivePathway';
import { ScheduledTasks } from '@/components/settings/ScheduledTasks';
import { Recipes } from '@/components/settings/Recipes';

function sectionLabels(): Record<string, string> {
  return {
    general: 'General',
    providers: 'Providers',
    local_models: 'Helper Models',
    mcp_servers: 'MCP Servers',
    scheduled_tasks: 'Scheduled Tasks',
    recipes: 'Recipes',
    adaptive_pathway: 'Adaptive Pathway',
    notifications: 'Notifications',
    appearance: 'Appearance',
    advanced: 'Advanced',
  };
}

/** Three groups, in nav order (settings IA overhaul). Graph Health and Domain
    Profiles are no longer separate tabs (release-fixes items 23/24): Graph
    Health rolled into Adaptive Pathway as a section, Domain Profiles is
    hidden entirely (nothing to configure there — see DomainProfiles.tsx's
    own doc comment). Local Models is always present: even an API-key user
    needs an embedding model for the memory engine, and it's where a "no
    model downloaded" status deep-links to. */
function buildGroups(): { label: string; sections: string[] }[] {
  return [
    {
      label: 'Essentials',
      // Notifications are the OS's job on Android: the system Settings app
      // owns per-channel control there, and a second in-app copy would be a
      // set of toggles that the OS can silently override.
      sections: [
        // General is desktop-only: on Android everything in it is either
        // desktop-only (hotkeys/clipboard/autostart) or relocated — the chats
        // folder defaults to app-private storage, and "Clear all chat history"
        // moves to Advanced. Rendering an almost-empty General is just clutter.
        ...(isAndroid() ? [] : ['general']),
        'providers',
        'local_models',
        'appearance',
        ...(isAndroid() ? [] : ['notifications']),
      ],
    },
    { label: 'Automation & extensions', sections: ['mcp_servers', 'scheduled_tasks', 'recipes'] },
    { label: 'Advanced', sections: ['advanced', 'adaptive_pathway'] },
  ];
}

export function SettingsView() {
  // Deep-link target comes from the route, not from this component's own IPC:
  // `routeStore` owns both the `route://goto` subscription and the one-shot
  // initial read, so a "Fix this" button that opens Settings and a tab switch
  // that lands on it go through exactly one path. Local `section` state layers
  // the user's own clicks on top of whatever the route asked for.
  const routedSection = useRouteStore((s) => s.settingsSection);
  const routedHighlight = useRouteStore((s) => s.settingsHighlight);
  const goto = useRouteStore((s) => s.goto);
  // General doesn't exist on Android (removed above), so it can't be the
  // default landing section there — start on Providers instead.
  const [section, setSection] = useState<string>(
    routedSection ?? (isAndroid() ? 'providers' : 'general'),
  );
  const [highlight, setHighlight] = useState<string | null>(routedHighlight);
  const [recoveryNotice, setRecoveryNotice] = useState<string | null>(null);

  useEffect(() => {
    void ipc.getConfigRecoveryNotice().then((msg) => {
      if (msg) setRecoveryNotice(msg);
    });
  }, []);

  // Follow later deep links. Guarded on a non-null section so a plain
  // "open Settings" doesn't yank the user off whichever tab they just picked.
  useEffect(() => {
    if (!routedSection) return;
    setSection(routedSection);
    setHighlight(routedHighlight);
  }, [routedSection, routedHighlight]);

  const groups = buildGroups();
  const labels = sectionLabels();

  return (
    <div className="settings-window">
      {recoveryNotice && (
        <div className="settings-recovery-notice" role="alert">
          <span>{recoveryNotice}</span>
          <button type="button" onClick={() => setRecoveryNotice(null)} aria-label="Dismiss">
            ×
          </button>
        </div>
      )}
      <nav className="settings-nav">
        {/* The mark doubles as the way out. Settings used to be its own
            window, so closing it was the window chrome's job; as a route it
            still needs an escape hatch or a desktop user is stranded with no
            path back to their conversation (Android has the tab bar, desktop
            has nothing else).
            This carried the logo alone for a while, on the theory that
            click-the-logo-to-go-home is a convention strong enough not to need
            labelling. It isn't here: the logo is also just the app's mark at
            the top of a nav, which reads as decoration rather than a control,
            and the only hint otherwise was a tooltip you had to hover to find.
            The label is desktop-only because Android navigates by tab bar and
            never renders this nav as an escape hatch. */}
        <button
          className="settings-nav-home"
          onClick={() => goto('chat')}
          title="Back to chat"
          aria-label="Back to chat"
        >
          <KittyIcon />
          {!isAndroid() && <span>Return to chat</span>}
        </button>
        {groups.map((g) => (
          <Fragment key={g.label}>
            <div className="settings-nav-group-label">{g.label}</div>
            {g.sections.map((id) => (
              <button
                key={id}
                className={id === section ? 'active' : ''}
                onClick={() => {
                  setSection(id);
                  setHighlight(null);
                }}
              >
                {labels[id]}
              </button>
            ))}
          </Fragment>
        ))}
      </nav>
      <main className="settings-main">
        {section === 'general' && <General />}
        {section === 'providers' && <Providers highlight={highlight} />}
        {section === 'local_models' && <HelperModels />}
        {section === 'mcp_servers' && <McpServers />}
        {section === 'scheduled_tasks' && <ScheduledTasks />}
        {section === 'recipes' && <Recipes />}
        {section === 'adaptive_pathway' && <AdaptivePathway />}
        {section === 'notifications' && <NotificationsSection />}
        {section === 'appearance' && <Appearance />}
        {section === 'advanced' && <Advanced />}
      </main>
    </div>
  );
}
