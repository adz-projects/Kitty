// The composer's slash commands, and which to suggest while one is typed.
// Pure, so what the menu offers is tested without a window. Each command's
// own parsing lives with it (`parseDebateCommand`, `parseCritiqueCommand`,
// `parseResearchCommand`, the composer's exact `/compact`); this is only the
// catalog the suggestion menu draws from.

export interface SlashCommand {
  /** Without the slash. */
  name: string;
  /** What follows the command, shown after its name. Empty: nothing. */
  args: string;
  /** One line on what it does. */
  description: string;
  /** Whether it takes text after it: completing it then adds a space. */
  takesText: boolean;
}

export const SLASH_COMMANDS: SlashCommand[] = [
  {
    name: 'deepresearch',
    args: '[turns=N] what to research and produce',
    description: 'Research in waves to a sourced report. You approve the plan first.',
    takesText: true,
  },
  {
    name: 'critique',
    args: '[genre=…] [stage=…] with the draft attached',
    description: 'A writing critique of a draft, after you confirm the brief.',
    takesText: true,
  },
  {
    name: 'debate',
    args: 'personas, rounds, and what to examine',
    description: 'Personas debate, each on its own model; this chat moderates.',
    takesText: true,
  },
  {
    name: 'compact',
    args: '',
    description: "Fold this chat's older messages into memory to free up context.",
    takesText: false,
  },
];

/** The commands to suggest for `text`: while it is a slash followed only by
    the start of a command word, every command beginning with that word,
    case-insensitively. Nothing once a space is typed (the command's own hint
    takes over) or for text that is not a command at all. */
export function matchSlashCommands(text: string): SlashCommand[] {
  const m = /^\/([A-Za-z]*)$/.exec(text);
  if (!m) return [];
  const typed = m[1].toLowerCase();
  return SLASH_COMMANDS.filter((c) => c.name.startsWith(typed));
}

/** The composer text after choosing `command`. */
export function completeSlashCommand(command: SlashCommand): string {
  return `/${command.name}${command.takesText ? ' ' : ''}`;
}

/** Whether `text` is already exactly a command name, so Enter should act on
    it rather than complete it. */
export function isExactSlashCommand(text: string): boolean {
  return SLASH_COMMANDS.some((c) => text === `/${c.name}`);
}
