// Files pasted into the composer (#27). The webview hands a paste over as
// `File` objects with no path on disk, so they can't take the drop route
// (which gives the model a path); an image becomes an inline image and a
// text file becomes an inlined document, like a long paste.

/** Text files larger than this are refused rather than inlined. */
export const MAX_PASTED_TEXT_BYTES = 200 * 1024;

const TEXT_EXTENSIONS =
  /\.(txt|md|markdown|csv|tsv|json|jsonl|xml|ya?ml|toml|ini|log|html?|css|js|jsx|ts|tsx|py|rs|go|java|c|cc|cpp|h|hpp|cs|rb|php|sh|ps1|bat|sql)$/i;

export type PastedFileKind = 'image' | 'text' | 'too_large' | 'unsupported';

/** What a pasted file can become. Pure. */
export function pastedFileKind(file: { name: string; type: string; size: number }): PastedFileKind {
  if (file.type.startsWith('image/')) return 'image';
  const texty = file.type.startsWith('text/') || TEXT_EXTENSIONS.test(file.name);
  if (!texty) return 'unsupported';
  return file.size > MAX_PASTED_TEXT_BYTES ? 'too_large' : 'text';
}

export function readAsDataUrl(file: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error ?? new Error('could not read the pasted file'));
    reader.readAsDataURL(file);
  });
}
