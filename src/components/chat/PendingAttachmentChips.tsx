import { useChatStore } from '@/stores/chatStore';
import { DocumentIcon } from '@/components/icons/DocumentIcon';

function basename(path: string) {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

/** Placeholder chip(s) shown the instant a file drop/pick is registered —
    before `addDroppedPaths` has finished inspecting/reading it — so a large
    file, or chat-only mode's binary-file path (which needs a real session
    created first), doesn't look like the drop did nothing for a few seconds.
    Each resolves into a real `FileChips`/`AttachmentChips`/`ClipboardImageChips`
    entry (or a surfaced error) once `addDroppedPaths` finishes for that path;
    not removable itself since there's no in-flight request to cancel. */
export function PendingAttachmentChips() {
  const pendingAttachments = useChatStore((s) => s.pendingAttachments);
  if (pendingAttachments.length === 0) return null;

  return (
    <div className="file-chips">
      {pendingAttachments.map((path) => (
        <span className="chip" key={path} title={`Attaching ${path}…`}>
          <span className="progress-pulse">
            <DocumentIcon />
          </span>
          <span className="chip-name progress-pulse">{basename(path)}</span>
        </span>
      ))}
    </div>
  );
}
