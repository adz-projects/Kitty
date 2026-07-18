import { useChatStore } from '@/stores/chatStore';
import { FolderIcon } from '@/components/icons/FolderIcon';
import { DocumentIcon } from '@/components/icons/DocumentIcon';

/** Removable chips for dropped files/folders. Folders offer "Set as working
    directory". On send, the paths are prepended to the message (see chatStore). */
export function FileChips() {
  const droppedFiles = useChatStore((s) => s.droppedFiles);
  const removeDroppedPath = useChatStore((s) => s.removeDroppedPath);
  const setWorkingDir = useChatStore((s) => s.setWorkingDir);

  if (droppedFiles.length === 0) return null;

  return (
    <div className="file-chips">
      {droppedFiles.map((f) => (
        <span className="chip" key={f.path} title={f.path}>
          <span>{f.is_dir ? <FolderIcon /> : <DocumentIcon />}</span>
          <span className="chip-name">{f.name}</span>
          {f.is_dir && (
            <button
              className="chip-action"
              title="Set as working directory (starts a new session here)"
              onClick={() => void setWorkingDir(f.path)}
            >
              set cwd
            </button>
          )}
          <button className="chip-x" title="Remove" onClick={() => removeDroppedPath(f.path)}>
            ×
          </button>
        </span>
      ))}
    </div>
  );
}
