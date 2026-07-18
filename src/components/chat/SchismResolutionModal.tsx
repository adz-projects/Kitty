import { useEffect, useState } from 'react';
import { ipc, onAdaptivePathwaySchism } from '@/lib/ipc';
import type { AdaptivePathwaySchismAlert } from '@/lib/types';
import { Modal } from '@/components/shared/Modal';

/** Shown when the Adaptive Pathway extension's ensemble splits into two
    factions that persistently disagree (Round-C Batch 4). Mounted once at
    the app root (not per-message) — a schism is a whole-system event, not
    tied to one conversation.

    Copy is plain-language (UX-simplification Batch 5) — "Kitty noticed two
    different patterns" rather than "ensemble/factions/agreement 0.42". Raw
    stats (faction sizes, exact agreement scores, detected-at) are real and
    kept, just tucked behind a "details" disclosure for power users — no
    fabricated "8/10 correct predictions" framing, since no per-model
    accuracy tracking exists anywhere in the extension to back that honestly. */
export function SchismResolutionModal() {
  const [alert, setAlert] = useState<AdaptivePathwaySchismAlert | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const [detailsOpen, setDetailsOpen] = useState(false);

  useEffect(() => {
    const un = onAdaptivePathwaySchism((payload) => {
      if (payload.state !== 'detected' && payload.state !== 'reviewing') return;
      setDismissed(false);
      void ipc
        .adaptivePathwayGetSchism()
        .then((result) => {
          if ('faction_a' in result) setAlert(result);
        })
        .catch(() => {});
    });
    return () => void un.then((fn) => fn());
  }, []);

  if (!alert || dismissed) return null;

  const resolve = async (keepFaction: 'a' | 'b' | 'both') => {
    try {
      await ipc.adaptivePathwayResolveSchism(keepFaction);
    } catch {
      /* the sidecar may have gone away between detection and resolution;
         dismissing either way avoids a stuck modal */
    }
    setAlert(null);
  };

  const aMoreConsistent = alert.within_a >= alert.within_b;

  return (
    <Modal title="Kitty's getting conflicting signals">
      <p>
        Two different patterns have emerged in what seems to work for you — Kitty can lean toward
        one, keep learning from both, or you can decide later.
      </p>
      <div className="row">
        <button className="primary" onClick={() => void resolve('a')}>
          Lean toward pattern A
        </button>
        <button onClick={() => void resolve('b')}>Lean toward pattern B</button>
        <button onClick={() => void resolve('both')}>Keep exploring both</button>
        <button onClick={() => setDismissed(true)}>Remind me later</button>
      </div>
      <p className="muted" style={{ fontSize: 12 }}>
        Pattern {aMoreConsistent ? 'A' : 'B'} has been the more internally consistent one so far.
      </p>

      <button type="button" className="disclosure-toggle" onClick={() => setDetailsOpen((o) => !o)}>
        {detailsOpen ? '▾' : '▸'} Details
      </button>
      {detailsOpen && (
        <div>
          <p>
            <strong>Pattern A</strong> — {alert.faction_a_models} signal
            {alert.faction_a_models > 1 ? 's' : ''}, {(alert.within_a * 100).toFixed(0)}% internal
            agreement
          </p>
          <p>
            <strong>Pattern B</strong> — {alert.faction_b_models} signal
            {alert.faction_b_models > 1 ? 's' : ''}, {(alert.within_b * 100).toFixed(0)}% internal
            agreement
          </p>
          <p>Agreement between the two patterns: {(alert.between * 100).toFixed(0)}%</p>
          {alert.detected_at && <p className="muted">First noticed: {alert.detected_at}</p>}
        </div>
      )}
    </Modal>
  );
}
