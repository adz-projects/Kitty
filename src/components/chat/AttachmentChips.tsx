import { useChatStore } from '@/stores/chatStore';

/** Inlined document attachments (large pastes / dropped text in chat-only mode).
    Their content is sent inline with the next message. */
export function AttachmentChips() {
  const attachments = useChatStore((s) => s.attachments);
  const remove = useChatStore((s) => s.removeAttachment);
  if (attachments.length === 0) return null;
  return (
    <div className="file-chips">
      {attachments.map((a) => (
        <details className="chip pasted-chip" key={a.id}>
          <summary>
            📝 {a.label}
            <button
              className="chip-x"
              title="Remove"
              onClick={(e) => {
                e.preventDefault();
                remove(a.id);
              }}
            >
              ×
            </button>
          </summary>
          <pre className="pasted-preview">{a.content.slice(0, 2000)}</pre>
        </details>
      ))}
    </div>
  );
}
