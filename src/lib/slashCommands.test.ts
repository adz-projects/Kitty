import { describe, expect, it } from 'vitest';
import {
  completeSlashCommand,
  isExactSlashCommand,
  matchSlashCommands,
  SLASH_COMMANDS,
} from './slashCommands';

const names = (text: string) => matchSlashCommands(text).map((c) => c.name);

describe('matchSlashCommands', () => {
  it('offers every command for a bare slash', () => {
    expect(names('/')).toEqual(SLASH_COMMANDS.map((c) => c.name));
  });

  it('narrows as the command word is typed, ignoring case', () => {
    expect(names('/d')).toEqual(['deepresearch', 'debate']);
    expect(names('/De')).toEqual(['deepresearch', 'debate']);
    expect(names('/dee')).toEqual(['deepresearch']);
    expect(names('/c')).toEqual(['critique', 'compact']);
    expect(names('/compact')).toEqual(['compact']);
  });

  it('stops once the command word ends, or for anything else', () => {
    expect(names('/debate ')).toEqual([]);
    expect(names('/debate three personas')).toEqual([]);
    expect(names('/zzz')).toEqual([]);
    expect(names('hello /d')).toEqual([]);
    expect(names(' /d')).toEqual([]);
    expect(names('')).toEqual([]);
    expect(names('/path/to/file')).toEqual([]);
  });
});

describe('completing', () => {
  it('leaves room for text only where the command takes it', () => {
    const by = (n: string) => SLASH_COMMANDS.find((c) => c.name === n)!;
    expect(completeSlashCommand(by('deepresearch'))).toBe('/deepresearch ');
    expect(completeSlashCommand(by('compact'))).toBe('/compact');
  });

  it('knows a finished command name', () => {
    expect(isExactSlashCommand('/compact')).toBe(true);
    expect(isExactSlashCommand('/comp')).toBe(false);
    expect(isExactSlashCommand('/compact ')).toBe(false);
  });
});
