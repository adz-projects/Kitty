import type { Message } from '@/stores/chatStore';
import { DocumentIcon } from '@/components/icons/DocumentIcon';
import { ImageIcon } from '@/components/icons/ImageIcon';

const ICON: Record<'file' | 'document' | 'image', typeof DocumentIcon> = {
  file: DocumentIcon,
  document: DocumentIcon,
  image: ImageIcon,
};

/** Read-only chips showing what was attached to a sent user turn (Round-7 fix)
    — a snapshot taken at send() time (see chatStore.ts), since the composer's
    own removable chips (FileChips/AttachmentChips/ClipboardImageChips) clear
    once the message is sent and otherwise leave no trace of the attachment. */
export function MessageAttachmentChips({ files }: { files: Message['attachedFiles'] }) {
  if (!files || files.length === 0) return null;
  return (
    <div className="file-chips">
      {files.map((f, i) => {
        const Icon = ICON[f.kind];
        return (
          <span className="chip" key={`${f.name}-${i}`} title={f.name}>
            <span>
              <Icon />
            </span>
            <span className="chip-name">{f.name}</span>
          </span>
        );
      })}
    </div>
  );
}
