import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type {
  DeciderDefaults,
  DeciderFeature,
  DeciderFeatureStats,
  DeciderProbe,
  DeciderSettings,
} from '@/lib/types';
import {
  DECIDER_FEATURES,
  DECIDER_THRESHOLDS,
  modelChanged,
  parseThreshold,
  statLine,
} from './deciderHelpers';

const DIALECT_LABELS: Record<string, string> = {
  systemone: 'Liquid d1 (system_one)',
};

/** Settings → Helper Models → Decision model.

    An optional small classifier (e.g. Liquid d1 on a llama.cpp server) that the
    engine asks quick yes/no and rating questions: which tools a chat needs,
    whether a stretch of conversation is worth learning from, whether a scraped
    page is a bot challenge… Every use fails open — with no model, or with the
    server down, Kitty behaves exactly as it does without one.

    The model is swappable: change the endpoint (or the dialect, once there is
    more than one), Test, and Save. Thresholds are tuned per model, so the
    user's overrides are reset when the model changes. */
export function DecisionModel() {
  const [saved, setSaved] = useState<DeciderSettings | null>(null);
  const [draft, setDraft] = useState<DeciderSettings | null>(null);
  const [defaults, setDefaults] = useState<DeciderDefaults | null>(null);
  const [probe, setProbe] = useState<DeciderProbe | null>(null);
  const [testing, setTesting] = useState(false);
  const [stats, setStats] = useState<Partial<Record<DeciderFeature, DeciderFeatureStats>>>({});
  const [thresholdText, setThresholdText] = useState<Record<string, string>>({});
  const [thresholdsOpen, setThresholdsOpen] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');

  useEffect(() => {
    void ipc
      .getDeciderSettings()
      .then((s) => {
        setSaved(s);
        setDraft(s);
        setThresholdText(textFor(s.thresholds));
      })
      .catch((e) => setError(String(e)));
    void ipc
      .getDeciderDefaults()
      .then(setDefaults)
      .catch(() => {});
    const loadStats = () =>
      void ipc
        .getDeciderStats()
        .then((r) => setStats(r.features))
        .catch(() => {});
    loadStats();
    const t = setInterval(loadStats, 10_000);
    return () => clearInterval(t);
  }, []);

  if (!draft || !saved) {
    return (
      <>
        <h2>Decision model</h2>
        {error && <div className="chat-error">{error}</div>}
      </>
    );
  }

  const dialects = defaults?.dialects?.length ? defaults.dialects : [draft.dialect];
  const endpointChanged = draft.url.trim() !== saved.url.trim() || draft.dialect !== saved.dialect;
  const swapped = probe?.ok ? modelChanged(saved, probe.model) : false;
  const hasOverrides = Object.keys(saved.thresholds).length > 0;

  const test = async () => {
    setTesting(true);
    setError('');
    setProbe(null);
    try {
      setProbe(await ipc.testDecider(draft.dialect, draft.url));
    } catch (e) {
      setError(String(e));
    } finally {
      setTesting(false);
    }
  };

  const save = async () => {
    setError('');
    setNotice('');
    const thresholds: Record<string, number> = {};
    for (const { key } of DECIDER_THRESHOLDS) {
      const v = parseThreshold(thresholdText[key] ?? '');
      if (v === undefined) {
        setError('Thresholds are probabilities between 0 and 1 (or empty for the default).');
        return;
      }
      if (v !== null) thresholds[key] = v;
    }
    const next: DeciderSettings = {
      ...draft,
      url: draft.url.trim(),
      // A tested endpoint names its model; an untested change of endpoint
      // leaves the model unknown rather than keeping a stale name.
      model: probe?.ok ? probe.model : endpointChanged ? null : draft.model,
      thresholds,
    };
    try {
      const stored = await ipc.setDeciderSettings(next);
      if (swapped && hasOverrides && Object.keys(stored.thresholds).length === 0) {
        setNotice('The model changed, so your threshold overrides were reset to its defaults.');
      } else {
        setNotice('Saved.');
      }
      setSaved(stored);
      setDraft(stored);
      setThresholdText(textFor(stored.thresholds));
      setProbe(null);
    } catch (e) {
      setError(String(e));
    }
  };

  const remove = async () => {
    setDraft({ ...draft, url: '' });
    setProbe(null);
  };

  return (
    <>
      <h2>Decision model</h2>
      <p className="muted">
        A small, fast classifier Kitty can ask quick yes/no questions — which tools a chat needs,
        whether a page is a bot check, what is worth remembering, whether a shell command needs your
        approval. It runs on a server you point Kitty at, not inside Kitty. Optional: with no model,
        or when its server can&apos;t be reached, everything works exactly as before.
      </p>

      <label className="field">
        <span>Type</span>
        <select
          value={draft.dialect}
          onChange={(e) => setDraft({ ...draft, dialect: e.target.value })}
        >
          {dialects.map((d) => (
            <option key={d} value={d}>
              {DIALECT_LABELS[d] ?? d}
            </option>
          ))}
        </select>
      </label>
      <label className="field">
        <span>Endpoint</span>
        <input
          type="text"
          placeholder="http://127.0.0.1:8080"
          value={draft.url}
          onChange={(e) => {
            setDraft({ ...draft, url: e.target.value });
            setProbe(null);
          }}
        />
      </label>
      <p className="muted" style={{ marginTop: 4 }}>
        For Liquid d1, the root of a <code>llama-server</code> running the GGUF (it answers on{' '}
        <code>/v1/systemone</code>). Reachable from this device — over Tailscale works on Android.
      </p>
      <div className="row">
        <button onClick={() => void test()} disabled={testing || !draft.url.trim()}>
          {testing ? 'Testing…' : 'Test'}
        </button>
        {saved.url && (
          <button onClick={() => void remove()} disabled={!draft.url}>
            Remove
          </button>
        )}
        {!probe && saved.model && !endpointChanged && (
          <span className="muted">Current model: {saved.model}</span>
        )}
      </div>
      {probe &&
        (probe.ok ? (
          <p className="muted">
            Connected{probe.model ? ` to ${probe.model}` : ''} · {probe.rtt_ms} ms per question.
            {swapped &&
              (hasOverrides
                ? ' This is a different model: your threshold overrides will be reset to its defaults when you save.'
                : ' This is a different model; its own default thresholds will be used.')}
          </p>
        ) : (
          <div className="chat-error">Couldn&apos;t use this endpoint: {probe.error}</div>
        ))}

      <p className="muted">
        Each use can be switched on or off on its own. Saving applies them at once, with no restart.
      </p>
      {DECIDER_FEATURES.map((f) => {
        const line = statLine(f.id, stats[f.id]);
        return (
          <div className="field" key={f.id}>
            <label className="check">
              <input
                type="checkbox"
                checked={draft.features[f.id]}
                disabled={!draft.url.trim()}
                onChange={(e) =>
                  setDraft({
                    ...draft,
                    features: { ...draft.features, [f.id]: e.target.checked },
                  })
                }
              />
              {f.label}
            </label>
            <p className="muted" style={{ margin: 0 }}>
              {f.blurb}
            </p>
            {line && (
              <p className="muted" style={{ margin: 0, fontSize: 12 }}>
                {line}
              </p>
            )}
          </div>
        );
      })}

      <button
        type="button"
        className="disclosure-toggle"
        onClick={() => setThresholdsOpen((o) => !o)}
      >
        {thresholdsOpen ? '▾' : '▸'} <strong>Thresholds</strong>
      </button>
      {thresholdsOpen && (
        <>
          <p className="muted">
            Probability cutoffs, tuned for the model. Leave a field empty to use the default.
          </p>
          {DECIDER_THRESHOLDS.map((t) => (
            <label className="field" key={t.key}>
              <span>{t.label}</span>
              <input
                type="text"
                inputMode="decimal"
                placeholder={
                  defaults?.default_thresholds?.[t.key] != null
                    ? String(defaults.default_thresholds[t.key])
                    : ''
                }
                value={thresholdText[t.key] ?? ''}
                onChange={(e) => setThresholdText({ ...thresholdText, [t.key]: e.target.value })}
              />
            </label>
          ))}
          <div className="row">
            <button onClick={() => setThresholdText({})}>Reset to model defaults</button>
          </div>
        </>
      )}

      {error && <div className="chat-error">{error}</div>}
      <div className="row">
        <button className="primary" onClick={() => void save()}>
          Save
        </button>
        {notice && <span className="muted">{notice}</span>}
      </div>
    </>
  );
}

function textFor(thresholds: Record<string, number>): Record<string, string> {
  return Object.fromEntries(Object.entries(thresholds).map(([k, v]) => [k, String(v)]));
}
