import { useEffect, useState } from 'react';
import { ipc, onModelProgress, onModelsChanged } from '@/lib/ipc';
import { curatedModelsFor } from '@/lib/curated_models';
import { isAndroid } from '@/lib/platform';
import { TrashIcon } from '@/components/icons/TrashIcon';
import type { DownloadProgress, LocalModel } from '@/lib/types';
import { confirmDialog } from '@/components/shared/ConfirmDialog';

/** Bytes as a short human string. Exported for testing — the repo has no
    component-render tests, so display logic is only reachable this way. */
export function humanBytes(bytes: number): string {
  if (!bytes) return '';
  const gb = bytes / 1e9;
  return gb >= 1 ? `${gb.toFixed(1)} GB` : `${(bytes / 1e6).toFixed(0)} MB`;
}

/** Warn below this much free space (§5.2). */
const LOW_SPACE_BYTES = 2e9;

/** Percentage complete, or null when the total isn't known yet. */
export function downloadPercent(p: DownloadProgress): number | null {
  if (!p.total || p.total <= 0) return null;
  return Math.min(100, Math.round((p.received / p.total) * 100));
}

/** Local GGUF management: what's installed, download with progress, delete. */
export function HelperModels() {
  const [models, setModels] = useState<LocalModel[]>([]);
  const [downloads, setDownloads] = useState<Record<string, DownloadProgress>>({});
  const [free, setFree] = useState<number | null>(null);
  const [error, setError] = useState('');
  // Per-file HuggingFace tokens for gated repos, held in memory only for this
  // render — never persisted, never sent anywhere but the download request.
  const [tokens, setTokens] = useState<Record<string, string>>({});
  // Unfinished downloads left on disk (#70): they resume, or can be deleted.
  const [partials, setPartials] = useState<{ file: string; size_bytes: number }[]>([]);

  const refresh = () => {
    void ipc
      .listPartialDownloads()
      .then(setPartials)
      .catch(() => setPartials([]));
    void ipc
      .listLocalModels()
      .then(setModels)
      .catch((e) => setError(String(e)));
    void ipc
      .getModelsDiskFree()
      .then(setFree)
      .catch(() => setFree(null));
  };

  useEffect(() => {
    refresh();
    // Downloads already running (started here earlier, in the wizard, or in
    // another window) — progress survives leaving this page (#70).
    void ipc
      .listDownloads()
      .then((list) =>
        setDownloads((cur) => {
          const next = { ...cur };
          for (const p of list) next[p.download_id] = next[p.download_id] ?? p;
          return next;
        })
      )
      .catch(() => {});
    // This panel is conditionally rendered, so it mounts and unmounts every
    // time the user switches tabs — listeners and their cleanup timers must be
    // torn down or each revisit stacks another one on the last.
    const timers = new Set<ReturnType<typeof setTimeout>>();
    const unlistenProgress = onModelProgress((p) => {
      setDownloads((cur) => ({ ...cur, [p.download_id]: p }));
      if (p.done) {
        refresh();
        if (p.error) setError(p.error);
        const t = setTimeout(() => {
          timers.delete(t);
          setDownloads((cur) => {
            const next = { ...cur };
            delete next[p.download_id];
            return next;
          });
        }, 4000);
        timers.add(t);
      }
    });
    const unlistenChanged = onModelsChanged(refresh);
    return () => {
      void unlistenProgress.then((fn) => fn());
      void unlistenChanged.then((fn) => fn());
      timers.forEach(clearTimeout);
    };
  }, []);

  // One list, not two: a curated entry's row shows whichever state applies —
  // its Download button just reads "Installed" once it's on disk, so the
  // installed/available split repeated the same information twice.
  const installedByFile = new Map(models.map((m) => [m.file.toLowerCase(), m]));
  const curated = curatedModelsFor(isAndroid());
  const curatedFiles = new Set(curated.map((c) => c.file.toLowerCase()));
  // Defensive: a model on disk that no longer matches anything in the
  // curated catalog (e.g. after a future catalog change) still needs a way
  // to be deleted, so it gets its own row rather than silently disappearing.
  const uncuratedInstalled = models.filter((m) => !curatedFiles.has(m.file.toLowerCase()));

  const start = async (repo: string, file: string, gated?: boolean) => {
    setError('');
    try {
      await ipc.downloadModel(
        repo,
        file,
        undefined,
        undefined,
        gated ? tokens[file]?.trim() || undefined : undefined
      );
      // Drop the token now the request is away.
      setTokens((cur) => {
        const next = { ...cur };
        delete next[file];
        return next;
      });
    } catch (e) {
      setError(String(e));
    }
  };

  const remove = async (m: LocalModel) => {
    const ok = await confirmDialog({
      title: `Delete ${m.id}?`,
      message: 'The file is removed from disk. You can download it again later.',
      confirmLabel: 'Delete',
      danger: true,
    });
    if (!ok) return;
    setError('');
    try {
      await ipc.deleteLocalModel(m.id);
    } catch (e) {
      setError(String(e));
    }
  };

  const active = Object.values(downloads);

  return (
    <div className="settings-section">
      {/* The nav entry already says which section this is ("Helper Models" on
          both platforms), so no hardcoded title is repeated here — only the
          body copy still differs, since what these models actually do for
          you differs by platform (D18: chat never runs locally on Android). */}
      <p className="muted">
        {isAndroid()
          ? 'Kitty runs this itself, in the background, to power memory. Chat — and summarizing long chats — runs through the provider you connect. Downloads come from Hugging Face.'
          : 'Models run inside Kitty — no separate server to install or keep running. Downloads come from Hugging Face.'}
      </p>

      {error && <div className="chat-error">{error}</div>}

      {free !== null && (
        <p className={free < LOW_SPACE_BYTES ? 'chat-error' : 'muted'}>
          {humanBytes(free)} free on this drive
          {free < LOW_SPACE_BYTES && ' — that may not be enough for another model.'}
        </p>
      )}

      {active.length > 0 && (
        <div className="model-list">
          {active.map((p) => {
            const pct = downloadPercent(p);
            return (
              <div key={p.download_id} className="pull-row">
                <div className="pull-head">
                  <span className="model-name">{p.model}</span>
                  <span className="muted">
                    {p.error
                      ? p.error
                      : p.done
                        ? 'Done'
                        : pct !== null
                          ? `${pct}%`
                          : humanBytes(p.received)}
                  </span>
                  {!p.done && (
                    <button
                      className="link"
                      onClick={() =>
                        void ipc.cancelDownload(p.download_id).catch((e) => setError(String(e)))
                      }
                    >
                      Cancel
                    </button>
                  )}
                </div>
                <div className="progress">
                  {/* An unknown total gets a fixed-width bar rather than a
                      fake percentage — better to show motion than a number
                      we'd have to invent. */}
                  <div
                    className="progress-bar"
                    style={{ width: pct !== null ? `${pct}%` : '30%' }}
                  />
                </div>
              </div>
            );
          })}
        </div>
      )}

      {partials.length > 0 && (
        <>
          <h2>Unfinished downloads</h2>
          <p className="muted">
            Downloading one of these again picks up where it stopped. Delete one to free the space.
          </p>
          <div className="model-list">
            {partials.map((p) => (
              <div key={p.file} className="model-row">
                <div>
                  <div className="model-name">{p.file}</div>
                  <div className="muted">{humanBytes(p.size_bytes)} so far</div>
                </div>
                <button
                  onClick={() =>
                    void ipc
                      .deletePartialDownload(p.file)
                      .then(refresh)
                      .catch((e) => setError(String(e)))
                  }
                  title="Delete"
                  aria-label={`Delete the unfinished ${p.file}`}
                >
                  <TrashIcon />
                </button>
              </div>
            ))}
          </div>
        </>
      )}

      <h2>Models</h2>
      <div className="model-list">
        {curated.map((c) => {
          const have = installedByFile.get(c.file.toLowerCase());
          const busy = active.some((p) => p.model === c.file && !p.done);
          return (
            <div key={c.file} className="model-item">
              <div className="model-row">
                <div>
                  <div className="model-name">{c.label}</div>
                  <div className="muted">
                    {have ? (
                      <>
                        {humanBytes(have.size_bytes)}
                        {have.info?.quantization && ` · ${have.info.quantization}`}
                        {have.info?.context_length &&
                          ` · ${Math.round(have.info.context_length / 1024)}k context`}
                      </>
                    ) : (
                      <>
                        {c.blurb} · {c.size_gb} GB
                      </>
                    )}
                  </div>
                </div>
                <div className="row">
                  <button
                    disabled={!!have || busy}
                    onClick={() => void start(c.repo, c.file, c.gated)}
                    className={have ? undefined : 'primary'}
                  >
                    {have ? 'Installed' : busy ? 'Downloading…' : 'Download'}
                  </button>
                  {have && (
                    <button
                      onClick={() => void remove(have)}
                      title="Delete"
                      aria-label={`Delete ${c.label}`}
                    >
                      <TrashIcon />
                    </button>
                  )}
                </div>
              </div>
              {c.gated && !have && (
                <div className="gated-token">
                  <label className="muted" htmlFor={`hf-token-${c.file}`}>
                    Gemma-licensed: accept it on the{' '}
                    <a href={`https://huggingface.co/${c.repo}`} target="_blank" rel="noreferrer">
                      model page
                    </a>{' '}
                    and paste a HuggingFace token (read scope). Used only for this download, never
                    stored.
                  </label>
                  <input
                    id={`hf-token-${c.file}`}
                    type="password"
                    autoComplete="off"
                    placeholder="hf_…"
                    value={tokens[c.file] ?? ''}
                    disabled={busy}
                    onChange={(e) => setTokens((cur) => ({ ...cur, [c.file]: e.target.value }))}
                  />
                </div>
              )}
            </div>
          );
        })}
        {uncuratedInstalled.map((m) => (
          <div key={m.id} className="model-row">
            <div>
              <div className="model-name">{m.id}</div>
              <div className="muted">
                {humanBytes(m.size_bytes)}
                {m.info?.quantization && ` · ${m.info.quantization}`}
                {m.info?.context_length &&
                  ` · ${Math.round(m.info.context_length / 1024)}k context`}
              </div>
            </div>
            <button onClick={() => void remove(m)} title="Delete" aria-label={`Delete ${m.id}`}>
              <TrashIcon />
            </button>
          </div>
        ))}
      </div>
    </div>
  );
}
