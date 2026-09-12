import { useState } from 'react';

import { ipc } from '@/lib/ipc';
import { isAndroid } from '@/lib/platform';
import type { SubagentStatusEvent } from '@/lib/types';
import { useChatStore } from '@/stores/chatStore';

/** One chip standing for every specialist this turn delegated to.
 *
 *  Previously one chip per delegate, which a three-way fan-out turned into
 *  three chips wrapping onto multiple lines on a phone — permanently, since
 *  nothing removed them. The count is the part worth reading at a glance; the
 *  individual runs matter only when you want to watch one, which is what the
 *  desktop popover below is for. */
export function SubagentChip({ subagents }: { subagents: SubagentStatusEvent[] }) {
  const [open, setOpen] = useState(false);

  const working = subagents.filter((s) => s.status === 'started');
  const failed = subagents.filter((s) => s.status === 'failed');

  // Worst status wins, so a failure is never masked by a sibling that
  // succeeded — the chip is on screen for only a few seconds after the last
  // one finishes, and that is the window in which a failure has to register.
  const dot = failed.length > 0 ? 'bad' : working.length > 0 ? 'warn' : 'ok';

  const noun = subagents.length === 1 ? 'specialist' : 'specialists';
  const state =
    working.length > 0 ? 'working' : failed.length > 0 ? `${failed.length} failed` : 'done';

  // Watching a delegate means opening a second window, which Android's
  // single-window hub has nowhere to put (see `windows.rs`'s module docs).
  const canWatch = !isAndroid() && working.length > 0;

  const watch = async (childSessionId: string, specialist: string) => {
    setOpen(false);
    try {
      // The delegate runs in the app's own workspace, so this window's cwd and
      // mode carry over; only the session id differs. `spectate` is what makes
      // the new window read-only — see `ChatWorkspace`.
      const { cwd, mode, availableModes } = useChatStore.getState();
      await ipc.openNewChatWindow({
        session_id: childSessionId,
        // The store types both as nullable; a delegate inherits the app's
        // workspace, so an empty string just means 'no folder pill to show'.
        cwd: cwd ?? '',
        current_mode: mode ?? '',
        available_modes: availableModes,
        spectate: true,
        specialist,
      });
    } catch {
      // Opening a window is a convenience; failing to is not worth interrupting
      // a running turn over, and the delegate reports back regardless.
    }
  };

  const label = (
    <>
      <span className={`status-dot ${dot}`} />
      {subagents.length} {noun} · <span className="muted">{state}</span>
    </>
  );

  if (!canWatch) {
    // `title` carries the per-delegate detail the single chip drops, so it is
    // still recoverable on hover without spending a line of screen on it.
    return (
      <span className="subagent-chip" title={subagents.map(describe).join('\n')}>
        {label}
      </span>
    );
  }

  return (
    <span className="subagent-chip-wrap">
      <button
        type="button"
        className="subagent-chip is-clickable"
        aria-expanded={open}
        title="Watch a specialist while it works"
        onClick={() => {
          // One delegate needs no menu — open it.
          if (working.length === 1) {
            void watch(working[0].child_session_id, working[0].specialist);
            return;
          }
          setOpen((v) => !v);
        }}
      >
        {label}
      </button>
      {open && working.length > 1 && (
        <div className="subagent-popover" role="menu">
          {working.map((s) => (
            <button
              type="button"
              role="menuitem"
              key={s.child_session_id}
              onClick={() => void watch(s.child_session_id, s.specialist)}
            >
              <span className="status-dot warn" />
              {s.specialist}
            </button>
          ))}
        </div>
      )}
    </span>
  );
}

function describe(s: SubagentStatusEvent): string {
  const suffix = s.status === 'started' ? 'working' : s.status;
  return s.error ? `${s.specialist} — ${suffix}: ${s.error}` : `${s.specialist} — ${suffix}`;
}
