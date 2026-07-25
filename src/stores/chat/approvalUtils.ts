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

/** Whether `target` sits under a tool's own internal cache directory
    (`.../Block/goose/cache/...`, a legacy path some tools' scraped-page
    caches still use). These are the tool's own working storage, not a file
    the model is saving for the user, so they're out of scope for the
    chat-folder boundary entirely — rejecting them just breaks the tool (e.g.
    web fetch) without protecting anything. Lexical, matching
    `pathWithinDir`'s no-fs-access style. */
export function isInternalToolCachePath(target: string): boolean {
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

// Commands whose blast radius (remote access, destructive filesystem/network
// changes, privilege escalation) is high enough that "auto-allow because it's
// shell and shell isn't sandboxed anyway" is the wrong default — these should
// surface to the user instead of running unattended, even in chat mode.
// Matched case-insensitively against the whole command string (not just argv0)
// so a command reached via a wrapper (e.g. `cmd /c rm -rf ...`) still matches.
const SECURITY_SENSITIVE_RE =
  /\b(ssh|scp|sftp|sudo|su|rm\s+-rf|chmod|chown|curl\s+-o|wget\s+-O|netsh|iptables|shutdown|format|diskpart|nc|ncat|telnet|taskkill)\b/i;

/** Whether a shell command's blast radius is high enough to require an
    explicit user decision rather than an automatic allow (see
    `SECURITY_SENSITIVE_RE`). */
export function isSecuritySensitiveCommand(command: string): boolean {
  return SECURITY_SENSITIVE_RE.test(command);
}

/** Decide how to answer a tool-approval request (Round-5, owner decision;
    tri-stated in Round-7 item 3; extended to agentic mode alongside the
    directory-sandboxing feature — BigTiny is the authoritative containment
    check there, this is purely a round-trip-avoidance nicety, never the
    security boundary): tools are allowed, but a path-based file op is
    confined to one of `dirs` (the session's chat folder in chat mode; chat
    folder + current working directory in agentic mode, since "Set as
    working directory" can diverge the two), and a security-sensitive shell
    command always needs an explicit user decision. Returns `decision`:
      - `'allow'`: safe — respond with the allow option immediately.
      - `'reject'`: currently unused (kept for callers that want to
        auto-decline something with no ambiguity — e.g. the tool-loop guard),
        respond with the reject option immediately.
      - `'prompt'`: ambiguous enough to need a human — queue it to
        `pendingApprovals` instead of auto-responding.
    A tool with no structured path and a non-sensitive command (notably most
    `shell` calls, which is how the model produces docx/xlsx via Python) is
    allowed — a soft boundary, since shell isn't sandboxed beyond this check. */
export function decideChatApproval(
  rawInput: unknown,
  dirs: (string | null)[],
  options: { optionId: string }[]
): { decision: 'allow' | 'reject' | 'prompt'; optionId: string | null; warning?: string } {
  const input = (rawInput ?? {}) as {
    path?: string;
    file_path?: string;
    paths?: string[];
    command?: string;
  };
  const p =
    input.path ?? input.file_path ?? (Array.isArray(input.paths) ? input.paths[0] : undefined);
  const bases = dirs.filter((d): d is string => !!d);
  if (
    typeof p === 'string' &&
    p !== '' &&
    bases.length > 0 &&
    !bases.some((base) => pathWithinDir(base, p)) &&
    !isInternalToolCachePath(p)
  ) {
    return {
      decision: 'prompt',
      optionId: pickRejectOption(options),
      warning: `A file operation wants to reach outside this chat's folder (${p}).`,
    };
  }
  if (typeof input.command === 'string' && isSecuritySensitiveCommand(input.command)) {
    return {
      decision: 'prompt',
      optionId: pickRejectOption(options),
      warning: `"${input.command}" looks security-sensitive and needs your say-so before it runs.`,
    };
  }
  return { decision: 'allow', optionId: pickAllowOption(options) };
}
