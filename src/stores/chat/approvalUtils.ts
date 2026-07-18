// Chat-mode ("thought-partner") tool-approval policy: allow tools, but confine
// path-based file ops to the session's own chat folder.

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

/** Whether `target` sits under Goose's own internal cache directory
    (`.../Block/goose/cache/...`, e.g. `computercontroller`'s scraped-page
    cache). These are the tool's own working storage, not a file the model is
    saving for the user, so they're out of scope for the chat-folder boundary
    entirely — rejecting them just breaks the tool (e.g. web fetch) without
    protecting anything. Lexical, matching `pathWithinDir`'s no-fs-access
    style. */
export function isGooseInternalCachePath(target: string): boolean {
  return /(^|\/)block\/goose\/cache(\/|$)/i.test(target.replace(/\\/g, '/'));
}

/** The ACP permission options confirmed live are `allow_always`/`allow_once`/
    `reject_once`/`reject_always` (docs/acp-protocol.md) — pick the reject
    variant so an auto-declined tool call reads as a real decline, not a
    cancellation. `null` (cancel) as a fallback if none match. */
export const pickRejectOption = (options: { optionId: string }[]): string | null =>
  options.find((o) => /reject/i.test(o.optionId))?.optionId ?? null;

/** Pick the "allow once" variant (never `allow_always`, so approval never
    silently persists) for auto-approving a scoped chat-mode tool call. */
export const pickAllowOption = (options: { optionId: string }[]): string | null =>
  options.find((o) => o.optionId === 'allow_once')?.optionId ??
  options.find((o) => /allow/i.test(o.optionId))?.optionId ??
  null;

/** Decide how to answer a tool-approval request while in chat ("thought-
    partner") mode (Round-5, owner decision): tools are allowed, but a path-
    based file op is confined to the session's chat folder (`cwd`). Returns the
    ACP `optionId` to respond with, plus a `warning` to surface when a request
    is declined for reaching outside the folder. A tool with no structured path
    (notably `shell`, which produces docx/xlsx via Python) is allowed — a soft
    boundary, since shell isn't sandboxed. */
export function decideChatApproval(
  rawInput: unknown,
  cwd: string | null,
  options: { optionId: string }[]
): { optionId: string | null; warning?: string } {
  const input = (rawInput ?? {}) as { path?: string; file_path?: string; paths?: string[] };
  const p =
    input.path ?? input.file_path ?? (Array.isArray(input.paths) ? input.paths[0] : undefined);
  if (
    typeof p === 'string' &&
    p !== '' &&
    !!cwd &&
    !pathWithinDir(cwd, p) &&
    !isGooseInternalCachePath(p)
  ) {
    return {
      optionId: pickRejectOption(options),
      warning:
        `Declined a file operation outside this chat's folder (${p}). In thought-partner ` +
        `mode the model can only touch files inside the chat's own folder.`,
    };
  }
  return { optionId: pickAllowOption(options) };
}
