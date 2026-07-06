import { useChatStore } from '@/stores/chatStore';

/** Removable chip(s) for image(s) attached via the clipboard hotkey/tray item
    (Round-4). Shown regardless of chat/agentic mode — sent as native ACP
    image content blocks on the next message (see chatStore.ts's `send()`). */
export function ClipboardImageChips() {
  const pendingImages = useChatStore((s) => s.pendingImages);
  const removePendingImage = useChatStore((s) => s.removePendingImage);

  if (pendingImages.length === 0) return null;

  return (
    <div className="file-chips">
      {pendingImages.map((p) => (
        <span className="chip" key={p.id} title="Clipboard image">
          <img className="chip-thumb" src={p.data_url} alt="" />
          <span className="chip-name">Clipboard image</span>
          <button className="chip-x" title="Remove" onClick={() => removePendingImage(p.id)}>
            ×
          </button>
        </span>
      ))}
    </div>
  );
}
