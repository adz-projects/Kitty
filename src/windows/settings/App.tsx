import { Fragment, useEffect, useState } from 'react';
import { ipc, onSettingsNavigate } from '@/lib/ipc';
import { General } from '@/components/settings/General';
import { Providers } from '@/components/settings/Providers';
import { OllamaModels } from '@/components/settings/OllamaModels';
import { Extensions } from '@/components/settings/Extensions';
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
  ollama: 'Ollama Models',
  extensions: 'Extensions',
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
    showing tabs for a feature that's off. "Ollama Models" only appears when
    local inference is opted into (wizard redesign — a user who picked the
    API-key path has nothing to manage there; they can turn it back on from
    Advanced). */
function buildGroups(
  apEnabled: boolean,
  ollamaEnabled: boolean
): { label: string; sections: string[] }[] {
  return [
    {
      label: 'Essentials',
      sections: [
        'general',
        'providers',
        ...(ollamaEnabled ? ['ollama'] : []),
        'appearance',
        'notifications',
      ],
    },
    { label: 'Automation & extensions', sections: ['extensions', 'scheduled_tasks', 'recipes'] },
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

export function App() {
  const [section, setSection] = useState<string>('general');
  const [highlight, setHighlight] = useState<string | null>(null);
  const [apEnabled, setApEnabled] = useState(false);
  const [ollamaEnabled, setOllamaEnabled] = useState(true);

  useEffect(() => {
    void (async () => {
      const t = await ipc.getSettingsTarget();
      if (t?.section) {
        setSection(t.section);
        setHighlight(t.highlight);
      }
    })();
    const un = onSettingsNavigate((t) => {
      setSection(t.section);
      setHighlight(t.highlight);
    });
    return () => void un.then((fn) => fn());
  }, []);

  useEffect(() => {
    void ipc.getConfig().then((c) => {
      setApEnabled(c.adaptive_pathway_enabled);
      setOllamaEnabled(c.ollama_enabled);
    });
  }, [section]);

  const groups = buildGroups(apEnabled, ollamaEnabled);

  return (
    <div className="settings-window">
      <nav className="settings-nav">
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
        {section === 'ollama' && ollamaEnabled && <OllamaModels />}
        {section === 'extensions' && <Extensions />}
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
