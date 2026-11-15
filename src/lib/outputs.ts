// The chat's `outputs` folder: where the model puts what the user asked it to
// make, and the only thing the artifacts drawer lists. The name is mirrored in
// Rust (`OUTPUTS_DIR_NAME` in `commands/session/mod.rs`, and the instruction
// in `config/providers/mod.rs`).

import type { FileEntry } from './types';
import type { OutputRef } from '@/stores/chat/types';

export const OUTPUTS_DIR = 'outputs';

const slashes = (p: string): string => p.replace(/\\/g, '/');
const trimEnd = (p: string): string => p.replace(/\/+$/, '');

/** A Windows-style path (drive letter or UNC): the file system compares those
    case-insensitively, so this module does too. Anything else is exact. */
function isWindowsPath(p: string): boolean {
  return /^[a-z]:[\\/]/i.test(p) || p.startsWith('\\\\');
}

/** `<cwd>/outputs`, forward-slashed like every other path the app builds. */
export function outputsDir(cwd: string): string {
  return `${trimEnd(slashes(cwd))}/${OUTPUTS_DIR}`;
}

/** The top-level entry of the outputs directory that `path` is, or lies inside
    (a file in `outputs/reports/` belongs to the entry `reports`). `null` for a
    path outside the outputs directory, and for the directory itself. */
export function outputEntryOf(
  path: string,
  cwd: string
): { name: string; path: string; nested: boolean } | null {
  const dir = outputsDir(cwd);
  const p = slashes(path);
  const ci = isWindowsPath(dir) || isWindowsPath(p);
  const fold = (s: string) => (ci ? s.toLowerCase() : s);
  const prefix = `${dir}/`;
  if (!fold(p).startsWith(fold(prefix))) return null;
  const rest = p.slice(prefix.length).replace(/^\/+/, '');
  const segments = rest.split('/').filter((s) => s !== '');
  if (segments.length === 0) return null;
  const name = segments[0];
  return { name, path: `${dir}/${name}`, nested: segments.length > 1 };
}

/** The identity of an entry within one outputs directory. */
export function outputKey(name: string): string {
  return name.toLowerCase();
}

/** Entries in `after` that were not in `before`, or whose modification time or
    size changed: what a turn made or changed. Generic over the entry shape so it
    can diff the recursive `OutputSnapshotEntry` signatures (whose `modified`/
    `size` aggregate each entry's whole subtree, so a nested edit counts) as well
    as a flat listing. */
export function changedEntries<
  T extends { name: string; modified: number; size: number; is_dir: boolean },
>(before: T[], after: T[]): T[] {
  const was = new Map(before.map((e) => [outputKey(e.name), e]));
  return after.filter((e) => {
    const old = was.get(outputKey(e.name));
    return !old || old.modified !== e.modified || old.size !== e.size || old.is_dir !== e.is_dir;
  });
}

/** Newest first, then by name — the order the drawer lists them in. */
export function sortEntries(entries: FileEntry[]): FileEntry[] {
  return entries.slice().sort((a, b) => b.modified - a.modified || a.name.localeCompare(b.name));
}

export function toOutputRef(e: {
  name: string;
  path: string;
  is_dir: boolean;
  size: number;
}): OutputRef {
  return { name: e.name, path: e.path, isDir: e.is_dir, size: e.size };
}

const TYPE_LABELS: Record<string, string> = {
  docx: 'Word document',
  doc: 'Word document',
  dotx: 'Word template',
  xlsx: 'Excel workbook',
  xlsm: 'Excel workbook',
  xls: 'Excel workbook',
  csv: 'CSV file',
  tsv: 'TSV file',
  pptx: 'PowerPoint deck',
  ppt: 'PowerPoint deck',
  pdf: 'PDF',
  md: 'Markdown document',
  markdown: 'Markdown document',
  txt: 'Text file',
  rtf: 'Rich text document',
  odt: 'OpenDocument text',
  ods: 'OpenDocument spreadsheet',
  odp: 'OpenDocument presentation',
  json: 'JSON file',
  jsonl: 'JSON Lines file',
  yaml: 'YAML file',
  yml: 'YAML file',
  toml: 'TOML file',
  xml: 'XML file',
  html: 'Web page',
  htm: 'Web page',
  svg: 'SVG image',
  png: 'PNG image',
  jpg: 'JPEG image',
  jpeg: 'JPEG image',
  gif: 'GIF image',
  webp: 'WebP image',
  bmp: 'Bitmap image',
  mp3: 'Audio file',
  wav: 'Audio file',
  mp4: 'Video file',
  mov: 'Video file',
  zip: 'Zip archive',
  py: 'Python script',
  js: 'JavaScript file',
  ts: 'TypeScript file',
  tsx: 'TypeScript file',
  rs: 'Rust file',
  sql: 'SQL file',
  ipynb: 'Notebook',
  sh: 'Shell script',
  ps1: 'PowerShell script',
  bat: 'Batch file',
};

/** A short, human label for what kind of thing an entry is. */
export function fileTypeLabel(name: string, isDir: boolean): string {
  if (isDir) return 'Folder';
  const dot = name.lastIndexOf('.');
  if (dot <= 0 || dot === name.length - 1) return 'File';
  const ext = name.slice(dot + 1).toLowerCase();
  return TYPE_LABELS[ext] ?? `${ext.toUpperCase()} file`;
}

/** `1.4 MB`, `820 KB`, `12 B`. Binary units, labelled the way file managers do. */
export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return '';
  if (n < 1024) return `${Math.round(n)} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v >= 10 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

/** The one-line description under a card's name: "Word document · 24 KB". A
    folder's size is not known cheaply, so it is left off. */
export function describeOutput(ref: { name: string; isDir?: boolean; size?: number }): string {
  const type = fileTypeLabel(ref.name, ref.isDir === true);
  if (ref.isDir || ref.size === undefined) return type;
  const size = formatBytes(ref.size);
  return size ? `${type} · ${size}` : type;
}
