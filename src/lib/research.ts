// Deep research (`/deepresearch`): parsing the command, rendering a proposed
// plan for approval, and reading the engine's progress detail. Pure, so the
// rules the UI follows are tested without a window.

/** The tool whose call is a research plan waiting for the user's decision. */
export const RESEARCH_PLAN_TOOL = 'submit_research_plan';

/** Turns when the user names none: two waves, the critique, the write-up. */
export const DEFAULT_RESEARCH_TURNS = 4;
export const MIN_RESEARCH_TURNS = 3;
export const MAX_RESEARCH_TURNS = 20;

export interface ResearchCommand {
  /** What to research and produce, as the user wrote it. */
  brief: string;
  /** The turn budget the command set, or `null` to let the brief (or the
      default) decide. */
  maxTurns: number | null;
}

/** A leading `/deepresearch` command, or `null` when the text is not one.
    An optional `turns=N` (or `turns:N`, `--turns N`) right after the command
    sets the budget, clamped to 3–20. Matched as a whole word, so
    "/deepresearcher" stays an ordinary message. */
export function parseResearchCommand(text: string): ResearchCommand | null {
  const m = /^\/deepresearch(?:\s+([\s\S]*))?$/.exec(text.trim());
  if (!m) return null;
  let rest = (m[1] ?? '').trim();
  let maxTurns: number | null = null;
  const budget = /^(?:--turns\s+|turns\s*[=:]\s*)(\d+)\b\s*/i.exec(rest);
  if (budget) {
    maxTurns = clampTurns(Number(budget[1]));
    rest = rest.slice(budget[0].length).trim();
  }
  return { brief: rest, maxTurns };
}

export function clampTurns(n: number): number {
  if (!Number.isFinite(n)) return DEFAULT_RESEARCH_TURNS;
  return Math.min(MAX_RESEARCH_TURNS, Math.max(MIN_RESEARCH_TURNS, Math.round(n)));
}

/** Research waves a budget allows: everything but the critique and the
    write-up. */
export function wavesFor(turns: number): number {
  return Math.max(1, turns - 2);
}

interface PlanSection {
  id?: string;
  title?: string;
  description?: string;
}
interface PlanCell {
  id?: string;
  label?: string;
}
export interface ResearchPlan {
  title?: string;
  objective?: string;
  deliverable?: { description?: string; format?: string; allow_unverified?: boolean };
  sections?: PlanSection[];
  coverage?: PlanCell[];
  questions?: string[];
  acceptance?: string[];
  boundaries?: string[];
  turns?: number;
}

/** The plan a `submit_research_plan` call proposes, from its arguments. */
export function planFromArgs(args: unknown): ResearchPlan | null {
  if (!args || typeof args !== 'object') return null;
  const plan = (args as { plan?: unknown }).plan;
  if (!plan || typeof plan !== 'object') return null;
  return plan as ResearchPlan;
}

const strs = (xs: unknown): string[] =>
  Array.isArray(xs) ? xs.filter((x): x is string => typeof x === 'string' && x.trim() !== '') : [];

/** The plan as Markdown, for the approval card. */
export function planMarkdown(plan: ResearchPlan): string {
  const turns = clampTurns(plan.turns ?? DEFAULT_RESEARCH_TURNS);
  const waves = wavesFor(turns);
  const format = plan.deliverable?.format === 'docx' ? 'Markdown and Word' : 'Markdown';
  const lines: string[] = [];
  if (plan.title) lines.push(`### ${plan.title}`, '');
  if (plan.objective) lines.push(`**Objective.** ${plan.objective}`, '');
  if (plan.deliverable?.description) {
    lines.push(`**Deliverable** (${format}). ${plan.deliverable.description}`, '');
  }
  lines.push(
    `**Budget.** ${turns} turns: ${waves} research wave${waves === 1 ? '' : 's'}, a critique, the write-up.`,
    ''
  );
  const list = (title: string, items: string[]) => {
    if (items.length === 0) return;
    lines.push(`**${title}**`, ...items.map((i) => `- ${i}`), '');
  };
  list(
    'Outline',
    (plan.sections ?? []).map((s) => [s.title, s.description].filter(Boolean).join(': '))
  );
  list(
    'Coverage',
    (plan.coverage ?? []).map((c) => c.label || c.id || '').filter((x) => x !== '')
  );
  list('First questions', strs(plan.questions));
  list('Acceptance criteria', strs(plan.acceptance));
  list('Boundaries', strs(plan.boundaries));
  return lines.join('\n').trim();
}

/** A run's state as the engine reports it (`research_progress`'s detail and
    `GET /api/research/{id}`). Every field optional: the shape may grow. */
export interface ResearchDetail {
  id: string;
  session_id?: string | null;
  status: ResearchStatus;
  title?: string | null;
  brief?: string;
  max_turns?: number;
  turns_used?: number;
  waves?: number;
  tokens_used?: number;
  findings?: number;
  sources?: number;
  open_questions?: number;
  open_leads?: number;
  tangents_paid_off?: number;
  coverage?: { id: string; label: string; findings: number; corroborated: number }[];
  steps?: {
    turn: number;
    phase: 'plan' | 'wave' | 'critic' | 'write';
    session_id?: string | null;
    status: 'running' | 'succeeded' | 'failed' | 'skipped';
    summary?: string | null;
  }[];
  step?: {
    step?: string;
    wave?: number;
    tasks?: number;
    gaps?: number;
    /** Researchers that have reported so far this wave, of `of`. */
    done?: number;
    of?: number;
  };
  /** What the run is doing right now (live runs only). */
  activity?: ResearchActivity;
  output_path?: string | null;
  error?: string | null;
}

/** One planner or researcher at work. */
export interface ResearchWorker {
  session_id: string;
  label: string;
  /** What it last did, e.g. "searching: …" or "reading sos.ca.gov". */
  doing?: string | null;
  started_ms: number;
  last_ms: number;
}

export interface ResearchActivity {
  step_started_ms?: number | null;
  /** The last moment anything happened in the run. */
  last_activity_ms?: number | null;
  /** Silence longer than this means the run may be stuck. */
  stall_after_secs?: number;
  workers?: ResearchWorker[];
}

export type ResearchStatus =
  'planning' | 'researching' | 'critiquing' | 'writing' | 'done' | 'failed' | 'cancelled';

export function isLive(status: ResearchStatus): boolean {
  return (
    status === 'planning' ||
    status === 'researching' ||
    status === 'critiquing' ||
    status === 'writing'
  );
}

/** One line saying where a run is. */
export function phaseLabel(d: ResearchDetail): string {
  const waves = d.waves ?? wavesFor(d.max_turns ?? DEFAULT_RESEARCH_TURNS);
  switch (d.status) {
    case 'planning':
      return 'Waiting for the plan to be approved';
    case 'researching': {
      const wave = d.step?.wave ?? Math.min(waves, (d.turns_used ?? 0) + 1);
      const s = d.step?.step;
      const progress =
        s === 'researching' && d.step?.of ? ` (${d.step.done ?? 0} of ${d.step.of} reported)` : '';
      const doing =
        s === 'planning'
          ? 'planning'
          : s === 'researching'
            ? `researching${progress}`
            : s === 'wave_done'
              ? 'done'
              : '';
      return `Wave ${wave} of ${waves}${doing ? `: ${doing}` : ''}`;
    }
    case 'critiquing':
      return d.step?.step === 'filling_gaps' ? 'Critique: filling the gaps it found' : 'Critique';
    case 'writing':
      return d.step?.step === 'verifying' ? 'Writing: checking citations' : 'Writing the report';
    case 'done':
      return 'Done';
    case 'failed':
      return d.error ? `Failed: ${d.error}` : 'Failed';
    case 'cancelled':
      return d.error ?? 'Stopped';
  }
}

/** "12s", "3m 05s", "1h 02m": a duration for the card. */
export function formatDuration(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, '0')}s`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, '0')}m`;
}

/** Whether a live run has been silent for longer than it should be. */
export function isStalled(d: ResearchDetail, now: number): boolean {
  const last = d.activity?.last_activity_ms;
  const after = d.activity?.stall_after_secs;
  if (!isLive(d.status) || d.status === 'planning' || !last || !after) return false;
  return now - last > after * 1000;
}

/** A key for the milestone a progress update marks (a wave finished, the
    critique finished), or `null`. The engine posts a note in the chat at
    each, so the chat reloads once per key. */
export function milestoneKey(d: ResearchDetail): string | null {
  const s = d.step?.step;
  if (s === 'wave_done') return `${d.id}:wave:${d.step?.wave ?? 0}`;
  if (s === 'critique_done') return `${d.id}:critique`;
  return null;
}

/** Coverage as a fraction of cells with at least one finding. */
export function coverageFraction(d: ResearchDetail): number {
  const cells = d.coverage ?? [];
  if (cells.length === 0) return 0;
  return cells.filter((c) => c.findings > 0).length / cells.length;
}
