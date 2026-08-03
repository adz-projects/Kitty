import { useState } from 'react';
import { useChatStore } from '@/stores/chatStore';
import { DocumentIcon } from '@/components/icons/DocumentIcon';

/** Inlined document attachments (large pastes / dropped text in chat-only mode).
    Their content is sent inline with the next message. Never uses native
    <details> — this app's WebView2/Chromium build doesn't actually hide
    <details> content when `open` is false, so the preview would stay visible
    even "collapsed". Explicit state + conditional render instead, matching
    ErrorDetail.tsx / SessionList.tsx. */
export function AttachmentChips() {
  const attachments = useChatStore((s) => s.attachments);
  const remove = useChatStore((s) => s.removeAttachment);
  const [openId, setOpenId] = useState<string | null>(null);
  if (attachments.length === 0) return null;
  return (
    <div className="file-chips">
      {attachments.map((a) => {
        const open = openId === a.id;
        return (
          <div className="chip pasted-chip" key={a.id}>
            <div className="pasted-chip-summary">
              <button
                type="button"
                className="pasted-chip-toggle"
                onClick={() => setOpenId(open ? null : a.id)}
              >
                <DocumentIcon /> {a.label}
              </button>
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
            </div>
            {open && <pre className="pasted-preview">{a.content.slice(0, 2000)}</pre>}
          </div>
        );
      })}
    </div>
  );
}
