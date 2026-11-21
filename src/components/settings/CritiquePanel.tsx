import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';

/** Settings → Specialists → Writing critique: the optional author context a
    `/critique` carries (`config::CritiqueSettings`, `bigtiny::critique`). */
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
      <h2>Writing critique</h2>
      <p className="muted">
        Start a message with <code>/critique</code> and attach or paste a draft. Your chat&apos;s
        model reads it and proposes a brief (genre, reader, stage, and what to protect), and runs
        the critique once you confirm it. Each lens runs on a model from the debate panel above, or
        on your setups when it has no seats. Add <code>factcheck</code> to check the draft&apos;s
        facts on the web.
      </p>
      <label htmlFor="critique-author-context">About your writing (optional)</label>
      <p className="muted">
        Sentences of your own that sound like you, habits you want caught, and suggestions you
        always reject. Every critique uses these to weigh its findings and to calibrate its
        sketches, never as material to reuse.
      </p>
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
      <button className="primary" disabled={text === saved} onClick={() => void save()}>
        Save
      </button>
    </>
  );
}
