import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { isAndroid } from '@/lib/platform';
import { useChatStore } from '@/stores/chatStore';
import { ToolsIcon } from '@/components/icons/ToolsIcon';

/** Whether the per-chat Specialists switch belongs in the menu at all:
    only on desktop (Android has no specialists) and only while specialists
    are enabled in Settings — with them off globally there is nothing for a
    chat to switch. */
export function showSpecialistsToggle(android: boolean, enabledInSettings: boolean): boolean {
  return !android && enabledInSettings;
}

/** Per-chat on/off for specialists, in the chat's ⋯ menu beside the
    incognito switch. On by default. Off writes `specialists_disabled` into the
    engine session's metadata, and the engine then stops offering
    `call_specialist`, `await_specialists` and `list_specialists` to this
    chat's model. Read back from the chat whenever it changes, so a resumed
    chat — and a branch or fork, which copy the metadata — says what it is. */
export function SpecialistsToggle() {
  const sessionId = useChatStore((s) => s.sessionId);
  const android = isAndroid();
  const [enabledInSettings, setEnabledInSettings] = useState(false);
  const [on, setOn] = useState(true);

  useEffect(() => {
    if (android) return;
    let live = true;
    void ipc
      .getSpecialistsEnabled()
      .then((v) => {
        if (live) setEnabledInSettings(v);
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [android]);

  // The chat's own state: a new chat starts on, a resumed one says.
  useEffect(() => {
    setOn(true);
    if (!sessionId || android) return;
    let live = true;
    void ipc
      .getSessionSpecialists(sessionId)
      .then((v) => {
        if (live) setOn(v);
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [sessionId, android]);

  if (!showSpecialistsToggle(android, enabledInSettings)) return null;

  const toggle = async () => {
    if (!sessionId) return;
    const next = !on;
    setOn(next);
    try {
      await ipc.setSessionSpecialists(sessionId, next);
    } catch {
      setOn(!next);
    }
  };

  return (
    <button
      className="status-badge ap-toggle"
      role="menuitemcheckbox"
      aria-checked={on}
      onClick={() => void toggle()}
      disabled={!sessionId}
      title={
        on
          ? 'Specialists on — the model may hand self-contained work to a specialist. Click to turn off for this chat.'
          : 'Specialists off — this chat does all its work itself. Click to turn on.'
      }
    >
      <ToolsIcon /> <span className="ap-toggle-label">{on ? 'Specialists' : 'No specialists'}</span>
    </button>
  );
}
