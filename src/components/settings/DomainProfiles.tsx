import { useEffect, useMemo, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { PathwayBelief } from '@/lib/types';

interface DomainSummary {
  domain: string;
  count: number;
  tested: number;
  avgConfidence: number;
}

const UNTAGGED = '(no domain)';

function summarize(beliefs: PathwayBelief[]): DomainSummary[] {
  const byDomain = new Map<string, PathwayBelief[]>();
  for (const b of beliefs) {
    const key = b.domain ?? UNTAGGED;
    const list = byDomain.get(key);
    if (list) list.push(b);
    else byDomain.set(key, [b]);
  }
  return Array.from(byDomain.entries())
    .map(([domain, list]) => ({
      domain,
      count: list.length,
      tested: list.filter((b) => b.tested).length,
      avgConfidence: list.reduce((sum, b) => sum + b.confidence, 0) / list.length,
    }))
    .sort((a, b) => b.count - a.count);
}

/** Read-only belief-count-by-domain breakdown — replaces the old per-domain
    DPP-diversity-weight/novelty-lambda editor, which doesn't have an
    equivalent in the belief model: diversity weight (`DppConfig`) and
    diffusion tuning are global engine config now, not tunable per domain.
    Domains themselves are inferred at recall time from whichever existing
    belief a query most resembles (`domains::infer_query_domain` in
    `adaptive-pathway_rust`) — there's no separate domains table to edit.

    Hidden from the Settings nav (release-fixes item 24) since there was
    genuinely nothing here to configure — "What it remembers" in Adaptive
    Pathway covers the same beliefs more usefully. Left in place, just
    unreferenced, rather than deleted. */
export function DomainProfiles() {
  const [beliefs, setBeliefs] = useState<PathwayBelief[] | null>(null);
  const [error, setError] = useState('');

  useEffect(() => {
    void ipc
      .getPathwayBeliefs()
      .then((r) => setBeliefs(r.beliefs))
      .catch((e) => setError(String(e)));
  }, []);

  const domains = useMemo(() => summarize(beliefs ?? []), [beliefs]);

  return (
    <section className="settings-section">
      <h1>Domain Profiles</h1>
      <p className="muted">
        What Kitty has learned, grouped by topic area — so preferences from one kind of work don&apos;t
        bleed into another. Domains are inferred automatically as beliefs form; there&apos;s nothing to
        configure here.
      </p>
      {error && <div className="chat-error">{error}</div>}
      {beliefs == null && !error && <p className="muted">Loading…</p>}
      {beliefs != null && domains.length === 0 && <p className="muted">No domains learned yet.</p>}
      <div className="ext-list">
        {domains.map((d) => (
          <div className="row" key={d.domain} style={{ alignItems: 'center' }}>
            <div style={{ flex: 1 }}>
              <div>{d.domain}</div>
              <div className="muted" style={{ fontSize: 13 }}>
                {d.count} belief{d.count === 1 ? '' : 's'} · {d.tested} tested · average confidence{' '}
                {Math.round(d.avgConfidence * 100)}%
              </div>
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}
