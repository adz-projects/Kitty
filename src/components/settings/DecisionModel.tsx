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
  systemone: 'Self-hosted (System One, e.g. local d1)',
  'systemone-url': 'Hosted endpoint + API key (Cloudflare, gateways)',
};

/** Dialects that need a bearer API key and a model id (hosted endpoints). */
function isHosted(dialect: string): boolean {
  return dialect === 'systemone-url';
}

type PS = { batch_size?: number } | undefined;

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
  const [secondaryProbe, setSecondaryProbe] = useState<DeciderProbe | null>(null);
  const [secondaryTesting, setSecondaryTesting] = useState(false);
  // API keys: undefined = unchanged (leave the stored key), '' = clear, value = set.
  const [mainKey, setMainKey] = useState<string | undefined>(undefined);
  const [secondaryKey, setSecondaryKey] = useState<string | undefined>(undefined);
  const [keySet, setKeySet] = useState<[boolean, boolean]>([false, false]);
  const [chunking, setChunking] = useState(false);
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
        setChunking(((s.paragraph_screen as PS)?.batch_size ?? 1) > 1);
      })
      .catch((e) => setError(String(e)));
    void ipc
      .getDeciderKeySet()
      .then(setKeySet)
      .catch(() => {});
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
  const secondaryEndpointChanged =
    (draft.secondary?.url.trim() ?? '') !== (saved.secondary?.url.trim() ?? '') ||
    (draft.secondary?.dialect ?? '') !== (saved.secondary?.dialect ?? '');
  const swapped = probe?.ok ? modelChanged(saved, probe.model) : false;
  const hasOverrides = Object.keys(saved.thresholds).length > 0;

  const test = async () => {
    setTesting(true);
    setError('');
    setProbe(null);
    try {
      setProbe(await ipc.testDecider(draft.dialect, draft.url, mainKey, draft.model ?? undefined));
    } catch (e) {
      setError(String(e));
    } finally {
      setTesting(false);
    }
  };

  const addSecondary = () => {
    setDraft({ ...draft, secondary: { dialect: draft.dialect, url: '', model: null } });
    setSecondaryProbe(null);
  };
  const removeSecondary = () => {
    setDraft({ ...draft, secondary: null });
    setSecondaryProbe(null);
  };
  const testSecondary = async () => {
    if (!draft.secondary) return;
    setSecondaryTesting(true);
    setError('');
    setSecondaryProbe(null);
    try {
      setSecondaryProbe(
        await ipc.testDecider(
          draft.secondary.dialect,
          draft.secondary.url,
          secondaryKey,
          draft.secondary.model ?? undefined
        )
      );
    } catch (e) {
      setError(String(e));
    } finally {
      setSecondaryTesting(false);
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
    // Drop an empty fallback; otherwise name its model from a successful Test,
    // exactly as the primary does.
    const secondary =
      draft.secondary && draft.secondary.url.trim()
        ? {
            ...draft.secondary,
            url: draft.secondary.url.trim(),
            // Hosted: the model is the id the user typed. Self-hosted: named
            // by a successful Test, else cleared on an endpoint change.
            model: isHosted(draft.secondary.dialect)
              ? draft.secondary.model?.trim() || null
              : secondaryProbe?.ok
                ? secondaryProbe.model
                : secondaryEndpointChanged
                  ? null
                  : draft.secondary.model,
          }
        : null;
    const next: DeciderSettings = {
      ...draft,
      url: draft.url.trim(),
      model: isHosted(draft.dialect)
        ? draft.model?.trim() || null
        : probe?.ok
          ? probe.model
          : endpointChanged
            ? null
            : draft.model,
      thresholds,
      secondary,
    };
    try {
      // Pass the key edits (undefined = unchanged, '' = clear). A removed
      // fallback also clears its stored key.
      const stored = await ipc.setDeciderSettings(next, mainKey, secondary ? secondaryKey : '');
      setMainKey(undefined);
      setSecondaryKey(undefined);
      void ipc
        .getDeciderKeySet()
        .then(setKeySet)
        .catch(() => {});
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
        {isHosted(draft.dialect) ? (
          <>
            The full System One endpoint. For Cloudflare Clef, paste{' '}
            <code>
              https://api.cloudflare.com/client/v4/accounts/&lt;id&gt;/ai/run/@cf/cloudflare/clef
            </code>{' '}
            with your API token and model <code>clef</code>. For a hosted gateway (Jev, Drex, d1
            aggregators), paste its endpoint, key, and a model id like <code>drex-2.0</code>.
          </>
        ) : (
          <>
            For self-hosted d1, the root of a <code>llama-server</code> running the GGUF (it answers
            on <code>/v1/systemone</code>). Reachable from this device — over Tailscale works on
            Android.
          </>
        )}
      </p>
      {isHosted(draft.dialect) && (
        <>
          <label className="field">
            <span>Model</span>
            <input
              type="text"
              placeholder="clef, drex-2.0, d1-ultra…"
              value={draft.model ?? ''}
              onChange={(e) => {
                setDraft({ ...draft, model: e.target.value });
                setProbe(null);
              }}
            />
          </label>
          <label className="field">
            <span>API key</span>
            <input
              type="password"
              placeholder={
                keySet[0] && mainKey === undefined ? '•••••••• (key set)' : 'Bearer token'
              }
              value={mainKey ?? ''}
              onChange={(e) => {
                setMainKey(e.target.value);
                setProbe(null);
              }}
            />
          </label>
        </>
      )}
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

      {/* Optional fallback model: used only when the primary fails a call. */}
      {!draft.secondary ? (
        <div className="row" style={{ marginTop: 8 }}>
          <button onClick={() => addSecondary()} disabled={!draft.url.trim()}>
            Add a fallback model
          </button>
          <span className="muted">Used only if the model above can&apos;t be reached.</span>
        </div>
      ) : (
        <div className="field" style={{ marginTop: 8 }}>
          <span>
            <strong>Fallback model</strong>
          </span>
          <p className="muted" style={{ marginTop: 0 }}>
            Tried only when the model above fails to answer a question (its server is down, errors,
            or replies in an unexpected shape). Thresholds stay those of the primary.
          </p>
          <label className="field">
            <span>Type</span>
            <select
              value={draft.secondary.dialect}
              onChange={(e) =>
                setDraft({
                  ...draft,
                  secondary: { ...draft.secondary!, dialect: e.target.value },
                })
              }
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
              placeholder="http://127.0.0.1:8081"
              value={draft.secondary.url}
              onChange={(e) => {
                setDraft({ ...draft, secondary: { ...draft.secondary!, url: e.target.value } });
                setSecondaryProbe(null);
              }}
            />
          </label>
          {isHosted(draft.secondary.dialect) && (
            <>
              <label className="field">
                <span>Model</span>
                <input
                  type="text"
                  placeholder="clef, drex-2.0, d1-ultra…"
                  value={draft.secondary.model ?? ''}
                  onChange={(e) => {
                    setDraft({
                      ...draft,
                      secondary: { ...draft.secondary!, model: e.target.value },
                    });
                    setSecondaryProbe(null);
                  }}
                />
              </label>
              <label className="field">
                <span>API key</span>
                <input
                  type="password"
                  placeholder={
                    keySet[1] && secondaryKey === undefined ? '•••••••• (key set)' : 'Bearer token'
                  }
                  value={secondaryKey ?? ''}
                  onChange={(e) => {
                    setSecondaryKey(e.target.value);
                    setSecondaryProbe(null);
                  }}
                />
              </label>
            </>
          )}
          <div className="row">
            <button
              onClick={() => void testSecondary()}
              disabled={secondaryTesting || !draft.secondary.url.trim()}
            >
              {secondaryTesting ? 'Testing…' : 'Test'}
            </button>
            <button onClick={() => removeSecondary()}>Remove</button>
            {!secondaryProbe && draft.secondary.model && !secondaryEndpointChanged && (
              <span className="muted">Current model: {draft.secondary.model}</span>
            )}
          </div>
          {secondaryProbe &&
            (secondaryProbe.ok ? (
              <p className="muted">
                Connected{secondaryProbe.model ? ` to ${secondaryProbe.model}` : ''} ·{' '}
                {secondaryProbe.rtt_ms} ms per question.
              </p>
            ) : (
              <div className="chat-error">
                Couldn&apos;t use this endpoint: {secondaryProbe.error}
              </div>
            ))}
        </div>
      )}

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

      <div className="field">
        <label className="check">
          <input
            type="checkbox"
            checked={chunking}
            disabled={!draft.url.trim()}
            onChange={(e) => {
              const on = e.target.checked;
              setChunking(on);
              void ipc
                .setDeciderChunking(on)
                .then(() => setNotice(`Chunking turned ${on ? 'on' : 'off'}.`))
                .catch((err) => {
                  setChunking(!on);
                  setError(String(err));
                });
            }}
          />
          Chunk the memory intake screen
        </label>
        <p className="muted" style={{ margin: 0 }}>
          When Kitty decides which pasted/attached passages are worth remembering, it asks the
          decision model about each passage. <strong>On</strong>, it bundles several passages into
          one call — cheaper and faster, but on a quick test it let through a bit more filler.{' '}
          <strong>Off</strong> (the default) asks about one passage at a time, the most precise.
          Only affects what gets saved to memory, not the four checks above.
        </p>
      </div>

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
