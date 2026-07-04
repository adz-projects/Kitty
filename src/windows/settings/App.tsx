import { useEffect, useState } from 'react';
import { ipc, onSettingsNavigate } from '@/lib/ipc';
import { General } from '@/components/settings/General';
import { Providers } from '@/components/settings/Providers';
import { OllamaModels } from '@/components/settings/OllamaModels';
import { Extensions } from '@/components/settings/Extensions';
import { NotificationsSection } from '@/components/settings/NotificationsSection';
import { Appearance } from '@/components/settings/Appearance';
import { Advanced } from '@/components/settings/Advanced';
import { SetupRepair } from '@/components/settings/SetupRepair';

const SECTIONS = [
  { id: 'general', label: 'General' },
  { id: 'providers', label: 'Providers' },
  { id: 'ollama', label: 'Ollama Models' },
  { id: 'extensions', label: 'Extensions' },
  { id: 'notifications', label: 'Notifications' },
  { id: 'appearance', label: 'Appearance' },
  { id: 'advanced', label: 'Advanced' },
  { id: 'setup', label: 'Setup & Repair' },
] as const;

export function App() {
  const [section, setSection] = useState<string>('general');
  const [highlight, setHighlight] = useState<string | null>(null);

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

  return (
    <div className="settings-window">
      <nav className="settings-nav">
        {SECTIONS.map((s) => (
          <button
            key={s.id}
            className={s.id === section ? 'active' : ''}
            onClick={() => {
              setSection(s.id);
              setHighlight(null);
            }}
          >
            {s.label}
          </button>
        ))}
      </nav>
      <main className="settings-main">
        {section === 'general' && <General />}
        {section === 'providers' && <Providers highlight={highlight} />}
        {section === 'ollama' && <OllamaModels />}
        {section === 'extensions' && <Extensions />}
        {section === 'notifications' && <NotificationsSection />}
        {section === 'appearance' && <Appearance />}
        {section === 'advanced' && <Advanced />}
        {section === 'setup' && <SetupRepair />}
      </main>
    </div>
  );
}
