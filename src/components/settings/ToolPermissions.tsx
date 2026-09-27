import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { confirmDialog } from '@/components/shared/ConfirmDialog';
import type { AllowRule } from '@/lib/types';

const COMMAND_PATTERN = /^"command":"(.*)\(\?:"\|\\s\|\\\\\[nt\]\)$/;

/** What an "Always allow" rule covers, in words: the command it starts with
    for a shell rule (reversing `approvals::always_scope`'s pattern), else
    every use of the tool. Pure. */
export function ruleScope(rule: Pick<AllowRule, 'tool_name' | 'args_pattern'>): string {
  if (!rule.args_pattern) return `every use of ${rule.tool_name}`;
  const m = COMMAND_PATTERN.exec(rule.args_pattern);
  if (!m) return rule.args_pattern;
  try {
    const jsonBody = m[1].replace(/\\(.)/g, '$1');
    return `commands starting “${JSON.parse(`"${jsonBody}"`) as string}”`;
  } catch {
    return rule.args_pattern;
  }
}

/** Settings → Tool permissions (#8): every "Always allow" given, and a way to
    take each back. A revoked tool (or command) asks again next time. */
export function ToolPermissions() {
  const [rules, setRules] = useState<AllowRule[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = () =>
    ipc
      .listAllowRules()
      .then(setRules)
      .catch((e) => setError(String(e)));
  useEffect(() => void load(), []);

  const revoke = async (rule: AllowRule) => {
    const ok = await confirmDialog({
      title: 'Revoke this permission?',
      message: `Kitty will ask again for ${ruleScope(rule)}.`,
      confirmLabel: 'Revoke',
    });
    if (!ok) return;
    setError(null);
    try {
      await ipc.revokeAllowRule(rule.id);
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <section className="settings-section">
      <h1>Tool permissions</h1>
      <p className="muted">
        What you&apos;ve told Kitty to always allow. Everything else that needs a decision asks you
        first.
      </p>
      {error && <p className="error">{error}</p>}
      {rules === null && !error && <p className="muted">Loading…</p>}
      {rules?.length === 0 && <p className="muted">Nothing is always allowed.</p>}
      <div className="provider-list">
        {rules?.map((r) => (
          <div key={r.id} className="provider-row">
            <div className="provider-row-info">
              <div className="provider-name">
                <span className="provider-name-text">{r.tool_name}</span>
              </div>
              <div className="muted">Allowed: {ruleScope(r)}</div>
            </div>
            <div className="row provider-row-actions">
              <button onClick={() => void revoke(r)}>Revoke</button>
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}
