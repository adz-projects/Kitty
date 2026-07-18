/** Folder — working-directory pill, real chat folders, folder-type file
    chips. `variant="tray"` is a dashed-outline variant for the app-side
    "Uncategorized" bucket, which isn't a real filesystem folder — visually
    distinguishes it from a real one without needing a second glyph the way
    📁 vs 🗂 did. Replaces 📁/🗂. */
export function FolderIcon({ variant = 'folder' }: { variant?: 'folder' | 'tray' }) {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path
        d="M1.5 3.5h4l1.2 1.5h7.3v8H1.5v-9.5Z"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinejoin="round"
        strokeDasharray={variant === 'tray' ? '2 1.6' : undefined}
      />
    </svg>
  );
}
