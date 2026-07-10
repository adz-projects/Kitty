import type { Message } from '@/stores/chatStore';

const ICON: Record<'file' | 'document' | 'image', string> = {
  file: '📄',
  document: '📝',
  image: '🖼',
};

/** Read-only chips showing what was attached to a sent user turn (Round-7 fix)
    — a snapshot taken at send() time (see chatStore.ts), since the composer's
    own removable chips (FileChips/AttachmentChips/ClipboardImageChips) clear
    once the message is sent and otherwise leave no trace of the attachment. */
export function MessageAttachmentChips({ files }: { files: Message['attachedFiles'] }) {
  if (!files || files.length === 0) return null;
  return (
    <div className="file-chips">
      {files.map((f, i) => (
        <span className="chip" key={`${f.name}-${i}`} title={f.name}>
          <span>{ICON[f.kind]}</span>
          <span className="chip-name">{f.name}</span>
        </span>
      ))}
    </div>
  );
}
