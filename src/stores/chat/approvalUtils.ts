// Lexical path containment. The approval policy that used to live here
// (auto-allow inside the chat's folders, prompt otherwise) runs in Rust now,
// once for every chat (`src-tauri/src/approvals.rs`), and its tests moved
// with it.

const normPath = (p: string): string => p.replace(/\\/g, '/').replace(/\/+$/, '').toLowerCase();

/** Lexically (no fs access) decide whether `target` is inside `base`. Absolute
    targets keep their drive/root; relative ones resolve against `base`; `.`/`..`
    are collapsed. Case-insensitive (Windows). This backs the chat-mode "keep
    file ops inside the chat folder" soft boundary — a lexical check is
    proportionate since shell tools (also allowed in chat mode) aren't
    sandboxed anyway; it hard-confines only the path-based ops Kitty can
    actually inspect. */
export function pathWithinDir(base: string, target: string): boolean {
  const b = normPath(base);
  if (!b) return false;
  let t = target.replace(/\\/g, '/');
  const isAbsolute = /^[a-z]:\//i.test(t) || t.startsWith('/');
  if (!isAbsolute) t = `${b}/${t}`;
  const hasDrive = /^[a-z]:/i.test(t);
  const drive = hasDrive ? t.slice(0, 2) : '';
  const stack: string[] = [];
  for (const seg of (hasDrive ? t.slice(2) : t).split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') stack.pop();
    else stack.push(seg);
  }
  const resolved = normPath(`${drive}/${stack.join('/')}`);
  return resolved === b || resolved.startsWith(`${b}/`);
}
