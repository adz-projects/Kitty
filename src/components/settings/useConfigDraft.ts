import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { Config } from '@/lib/types';

/** Shared hook for config-backed settings sections: load a draft, edit it, save
    the whole config back through `set_config`. */
export function useConfigDraft() {
  const [draft, setDraft] = useState<Config | null>(null);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    ipc
      .getConfig()
      .then(setDraft)
      .catch((e) => setError(String(e)));
  }, []);

  const update = (patch: Partial<Config>) => {
    setSaved(false);
    setDraft((d) => (d ? { ...d, ...patch } : d));
  };

  const save = async () => {
    if (!draft) return;
    setError(null);
    try {
      await ipc.setConfig(draft);
      setSaved(true);
    } catch (e) {
      setError(String(e));
    }
  };

  return { draft, update, save, saved, error };
}
