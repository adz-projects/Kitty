// `/tidy`: parsing the command and reading a proposed tidy list for the card.
// Pure, so the rules the card follows are tested without a window.

/** The tool whose call is a tidy list waiting for the user's decision. */
export const TIDY_TOOL = 'propose_tidy';

/** What the user sends when `/tidy` comes with no wishes. */
export const DEFAULT_TIDY_TEXT = 'Tidy my chats.';

/** A leading `/tidy`: the user's wishes after it (possibly empty), or `null`
    when the text is not the command. Whole word, so "/tidyup" is a message. */
export function parseTidyCommand(text: string): string | null {
  const m = /^\/tidy(?:\s+([\s\S]*))?$/.exec(text.trim());
  if (!m) return null;
  return (m[1] ?? '').trim();
}

/** One candidate as Kitty graded it (`bigtiny::tidy::Candidate`). */
export interface TidyCandidate {
  ref: string;
  session_ids: string[];
  title: string;
  user_turns: number;
  last_activity: string | null;
  files: number;
  folder: string | null;
  background: number;
  runs: number | null;
  suggestion: 'delete' | 'unsure' | 'keep';
  score: number;
  reasons: string[];
}

export interface TidyState {
  candidates: TidyCandidate[];
  readable: string[];
}

/** What the model proposed (`propose_tidy`'s arguments). */
export interface TidyProposal {
  delete: string[];
  unsure: string[];
  reasons: Record<string, string>;
  summary: string | null;
}

const strs = (xs: unknown): string[] =>
  Array.isArray(xs) ? xs.filter((x): x is string => typeof x === 'string') : [];

export function proposalFromArgs(args: unknown): TidyProposal {
  const a = (args && typeof args === 'object' ? args : {}) as Record<string, unknown>;
  const reasons: Record<string, string> = {};
  if (a.reasons && typeof a.reasons === 'object') {
    for (const [k, v] of Object.entries(a.reasons as Record<string, unknown>)) {
      if (typeof v === 'string' && v.trim()) reasons[k] = v.trim();
    }
  }
  return {
    delete: strs(a.delete),
    unsure: strs(a.unsure),
    reasons,
    summary: typeof a.summary === 'string' && a.summary.trim() ? a.summary.trim() : null,
  };
}

export interface TidyRow extends TidyCandidate {
  /** The model's reason, where it gave one. */
  note: string | null;
}

export interface TidyGroups {
  delete: TidyRow[];
  unsure: TidyRow[];
  keep: TidyRow[];
}

/** The candidates in the groups the model proposed: a ref it listed in
    neither `delete` nor `unsure` is kept, and a ref it made up is ignored.
    A ref in both counts as unsure. */
export function groupCandidates(candidates: TidyCandidate[], proposal: TidyProposal): TidyGroups {
  const groups: TidyGroups = { delete: [], unsure: [], keep: [] };
  for (const c of candidates) {
    const row: TidyRow = { ...c, note: proposal.reasons[c.ref] ?? null };
    if (proposal.unsure.includes(c.ref)) groups.unsure.push(row);
    else if (proposal.delete.includes(c.ref)) groups.delete.push(row);
    else groups.keep.push(row);
  }
  return groups;
}

/** How many chats deleting `refs` removes, background chats included. */
export function chatsDeleted(candidates: TidyCandidate[], refs: Iterable<string>): number {
  const chosen = new Set(refs);
  return candidates.filter((c) => chosen.has(c.ref)).reduce((n, c) => n + c.session_ids.length, 0);
}

/** A row's facts, in words. */
export function rowFacts(c: TidyCandidate): string {
  return c.reasons.join(' · ');
}
