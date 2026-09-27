import { useEffect, useRef, useState } from 'react';
import { ipc, onConfigChanged } from '@/lib/ipc';
import type { Config } from '@/lib/types';

type Fields = Record<string, unknown>;

/** The top-level fields `draft` changed from `base`. Pure. */
export function configDiff(base: Fields, draft: Fields): Fields {
  const out: Fields = {};
  for (const key of Object.keys(draft)) {
    if (JSON.stringify(base[key]) !== JSON.stringify(draft[key])) out[key] = draft[key];
  }
  return out;
}

/** A config written elsewhere, with this page's unsaved edits kept on top.
    Pure. */
export function rebase<T extends object>(base: T, draft: T, fresh: T): T {
  return { ...fresh, ...configDiff(base as Fields, draft as Fields) } as T;
}

/** Shared hook for config-backed settings sections: load a draft, edit it,
    save only what changed (`patch_config`, #73). Saving the whole snapshot
    used to put back anything written since the page opened — a scheduler,
    MCP or provider change, even a one-shot task that then fired again.
    Follows `config://changed`, keeping unsaved edits. */
export function useConfigDraft() {
  const [draft, setDraft] = useState<Config | null>(null);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // The config as last read: what the draft's edits are measured against.
  const base = useRef<Config | null>(null);
  const draftRef = useRef<Config | null>(null);
  draftRef.current = draft;

  useEffect(() => {
    const load = () =>
      ipc
        .getConfig()
        .then((fresh) => {
          const b = base.current;
          const d = draftRef.current;
          base.current = fresh;
          setDraft(b && d ? rebase(b, d, fresh) : fresh);
        })
        .catch((e) => setError(String(e)));
    void load();
    const un = onConfigChanged(() => void load());
    return () => void un.then((f) => f());
  }, []);

  const update = (patch: Partial<Config>) => {
    setSaved(false);
    setDraft((d) => (d ? { ...d, ...patch } : d));
  };

  /** Resolves with what was saved (the changed fields), or null on failure. */
  const save = async (): Promise<Partial<Config> | null> => {
    if (!draft || !base.current) return null;
    setError(null);
    const patch = configDiff(base.current as unknown as Fields, draft as unknown as Fields);
    try {
      if (Object.keys(patch).length > 0) await ipc.patchConfig(patch);
      base.current = draft;
      setSaved(true);
      return patch as Partial<Config>;
    } catch (e) {
      setError(String(e));
      return null;
    }
  };

  return { draft, update, save, saved, error };
}
