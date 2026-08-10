import { Fragment, useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useRouteStore } from '@/stores/routeStore';
import { General } from '@/components/settings/General';
import { Providers } from '@/components/settings/Providers';
import { LocalModels } from '@/components/settings/LocalModels';
import { McpServers } from '@/components/settings/McpServers';
import { NotificationsSection } from '@/components/settings/NotificationsSection';
import { Appearance } from '@/components/settings/Appearance';
import { Advanced } from '@/components/settings/Advanced';
import { SetupRepair } from '@/components/settings/SetupRepair';
import { AdaptivePathway } from '@/components/settings/AdaptivePathway';
import { GraphHealth } from '@/components/settings/GraphHealth';
import { DomainProfiles } from '@/components/settings/DomainProfiles';
import { ScheduledTasks } from '@/components/settings/ScheduledTasks';
import { Recipes } from '@/components/settings/Recipes';

const SECTION_LABELS: Record<string, string> = {
  general: 'General',
  providers: 'Providers',
  local_models: 'Local Models',
  mcp_servers: 'MCP Servers',
  scheduled_tasks: 'Scheduled Tasks',
  recipes: 'Recipes',
  adaptive_pathway: 'Adaptive Pathway',
  ap_graph_health: 'Graph Health',
  ap_domains: 'Domain Profiles',
  notifications: 'Notifications',
  appearance: 'Appearance',
  advanced: 'Advanced',
  setup: 'Setup & Repair',
};

/** Three groups, in nav order (settings IA overhaul). Graph Health / Domain
    Profiles only appear once Adaptive Pathway is actually enabled — no point
    showing tabs for a feature that's off. Local Models is always present:
    even an API-key user needs an embedding model for the memory engine, and
    it's where a "no model downloaded" status deep-links to. */
function buildGroups(apEnabled: boolean): { label: string; sections: string[] }[] {
  return [
    {
      label: 'Essentials',
      sections: ['general', 'providers', 'local_models', 'appearance', 'notifications'],
    },
    { label: 'Automation & extensions', sections: ['mcp_servers', 'scheduled_tasks', 'recipes'] },
    {
      label: 'Advanced',
      sections: [
        'advanced',
        'setup',
        'adaptive_pathway',
        ...(apEnabled ? ['ap_graph_health', 'ap_domains'] : []),
      ],
    },
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
  const [section, setSection] = useState<string>(routedSection ?? 'general');
  const [highlight, setHighlight] = useState<string | null>(routedHighlight);
  const [apEnabled, setApEnabled] = useState(false);
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

  useEffect(() => {
    void ipc.getConfig().then((c) => {
      setApEnabled(c.adaptive_pathway_enabled);
    });
  }, [section]);

  const groups = buildGroups(apEnabled);

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
        {/* Settings used to be its own window, so closing it was the window
            chrome's job. As a route it needs its own way out, or the user is
            stranded with no path back to their conversation. */}
        <button className="settings-nav-back" onClick={() => goto('chat')}>
          ‹ Back to chat
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
                {SECTION_LABELS[id]}
              </button>
            ))}
          </Fragment>
        ))}
      </nav>
      <main className="settings-main">
        {section === 'general' && <General />}
        {section === 'providers' && <Providers highlight={highlight} />}
        {section === 'local_models' && <LocalModels />}
        {section === 'mcp_servers' && <McpServers />}
        {section === 'scheduled_tasks' && <ScheduledTasks />}
        {section === 'recipes' && <Recipes />}
        {section === 'adaptive_pathway' && <AdaptivePathway />}
        {section === 'ap_graph_health' && <GraphHealth />}
        {section === 'ap_domains' && <DomainProfiles />}
        {section === 'notifications' && <NotificationsSection />}
        {section === 'appearance' && <Appearance />}
        {section === 'advanced' && <Advanced />}
        {section === 'setup' && <SetupRepair />}
      </main>
    </div>
  );
}
