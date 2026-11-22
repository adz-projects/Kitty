import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';

/** Settings → Specialists → critic → Edit → About your writing: the optional
    author context a `/critique` carries (`config::CritiqueSettings`,
    `bigtiny::critique`). */
export function CritiquePanel() {
  const [text, setText] = useState('');
  const [saved, setSaved] = useState('');
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void ipc
      .getConfig()
      .then((c) => {
        const value = c.critique?.author_context ?? '';
        setText(value);
        setSaved(value);
      })
      .catch(() => {});
  }, []);

  const save = async () => {
    setError(null);
    try {
      await ipc.patchConfig({ critique: { author_context: text } });
      setSaved(text);
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <>
      <div className="settings-subhead">
        <h3>
          <label htmlFor="critique-author-context">About your writing</label>
        </h3>
        <p className="muted">
          Optional, for every <code>/critique</code>: sentences that sound like you, habits to
          catch, suggestions you always reject. Used to weigh findings, never reused as text.
        </p>
      </div>
      {error && <div className="chat-error">{error}</div>}
      <textarea
        id="critique-author-context"
        rows={6}
        value={text}
        onChange={(e) => setText(e.target.value)}
        placeholder={
          'Voice: "A sentence of yours that sounds like you."\nHabit to watch: the institutional "we".\nI reject: lyrical endings.'
        }
      />
      <div className="row">
        <button disabled={text === saved} onClick={() => void save()}>
          Save notes
        </button>
      </div>
    </>
  );
}
