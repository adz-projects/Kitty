import { describe, expect, it } from 'vitest';
import { vizFileName } from './VisualizationCard';

describe('vizFileName', () => {
  it('turns a title into a plain file name', () => {
    expect(vizFileName('Quarterly revenue, FY 2025')).toBe('quarterly-revenue-fy-2025.html');
  });

  it('leaves nothing that could read as a path', () => {
    expect(vizFileName('..\\..\\evil/../name')).toBe('evil-name.html');
    expect(vizFileName('../')).toBe('figure.html');
  });

  it('falls back when the title has no usable characters, and caps the length', () => {
    expect(vizFileName('—')).toBe('figure.html');
    expect(vizFileName('a'.repeat(200))).toBe(`${'a'.repeat(60)}.html`);
  });
});
