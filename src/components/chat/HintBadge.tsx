import { useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { AdaptivePathwayEdge } from '@/lib/types';
import { findHintToolCall, parseHintOutput, type Message } from '@/stores/chatStore';
import { usePopoverPosition } from '@/lib/usePopoverPosition';
import { LightbulbIcon } from '@/components/icons/LightbulbIcon';

type EdgeDetailState =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'ok'; edge: AdaptivePathwayEdge };

const SOURCE_MODEL_LABEL: Record<string, string> = {
  ig: 'worth exploring',
  pc: 'a different angle',
  wildcard: 'untested angle',
};

/** Plain-language read of an edge's real, available stats — no fabricated
    "used N times" claim, since no per-edge usage count exists anywhere in
    the extension (UX-simplification Batch 5: humanize, don't invent). */
function describeEdge(edge: AdaptivePathwayEdge): string {
  const pct = Math.round(edge.confidence * 100);
  if (edge.status === 'active' && pct >= 70)
    return `Worked well in similar situations (${pct}% confidence).`;
  if (pct >= 40) return `Worked reasonably well so far (${pct}% confidence).`;
  return `Still being tested — ${pct}% confidence so far.`;
}

/** `standard` renders no badge at all (hollow — nothing to call out); the
    other three get a small labeled pill. */
function SourceModelBadge({ sourceModel }: { sourceModel?: string }) {
  if (!sourceModel || sourceModel === 'standard') return null;
  const label = SOURCE_MODEL_LABEL[sourceModel] ?? sourceModel;
  return (
    <span className={`hint-source-badge hint-source-${sourceModel}`}>
      {sourceModel === 'wildcard' && <LightbulbIcon />} {label}
    </span>
  );
}

/** Inline badge under an assistant message that used the Adaptive Pathway
    extension's `decide` hint (Round-C). Same popover-badge pattern as
    `ModeBadge`/`ProviderBadge` — always visible when hints exist (this is
    informational content, not a hover-only action like `.msg-actions`). */
export function HintBadge({ message }: { message: Message }) {
  const [open, setOpen] = useState(false);
  const [edgeDetail, setEdgeDetail] = useState<Record<string, EdgeDetailState>>({});
  const { triggerRef, popoverRef, style } = usePopoverPosition(open, () => setOpen(false));

  const call = findHintToolCall(message);
  const parsed = parseHintOutput(call);
  if (!parsed) return null;
  const { hints } = parsed;

  const showWhy = async (edgeId: string) => {
    setEdgeDetail((prev) => ({ ...prev, [edgeId]: { status: 'loading' } }));
    try {
      const edge = await ipc.adaptivePathwayGetEdge(edgeId);
      setEdgeDetail((prev) => ({ ...prev, [edgeId]: { status: 'ok', edge } }));
    } catch (e) {
      setEdgeDetail((prev) => ({ ...prev, [edgeId]: { status: 'error', message: String(e) } }));
    }
  };

  return (
    <div style={{ position: 'relative', display: 'inline-block' }}>
      <button
        ref={triggerRef as React.Ref<HTMLButtonElement>}
        className="status-badge hint-badge"
        onClick={() => setOpen((o) => !o)}
        title="Suggestions from Adaptive Pathway"
      >
        <LightbulbIcon />{' '}
        <span className="hint-badge-label">
          {hints.length} hint{hints.length > 1 ? 's' : ''}
        </span>
      </button>
      {open && (
        <div ref={popoverRef} className="mode-popover hint-popover" role="menu" style={style}>
          {hints.map((hint, i) => {
            const detail = hint.edge_id ? edgeDetail[hint.edge_id] : undefined;
            return (
              <div key={i} className="hint-popover-item">
                <SourceModelBadge sourceModel={hint.source_model} />
                <div>{hint.text}</div>
                {hint.edge_id && (
                  <button className="link" onClick={() => void showWhy(hint.edge_id!)}>
                    why?
                  </button>
                )}
                {detail?.status === 'loading' && <div className="muted">Loading…</div>}
                {detail?.status === 'error' && <div className="muted">{detail.message}</div>}
                {detail?.status === 'ok' && (
                  <div className="hint-edge-detail">
                    <div>{describeEdge(detail.edge)}</div>
                    <div className="muted" style={{ fontSize: 11 }}>
                      {detail.edge.semantic_primitive} · {detail.edge.tier} tier ·{' '}
                      {detail.edge.status}
                    </div>
                  </div>
                )}
                {i < hints.length - 1 && <hr className="mode-popover-sep" />}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
