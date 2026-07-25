// Curated starter models for the first-run wizard, spanning entry-level to
// mid-range consumer GPUs. Tags are verified against ollama.com at build time
// and recorded in docs/VERSIONS.md.

export interface StarterModel {
  tag: string;
  label: string;
  blurb: string;
  size_gb: number;
}

export const STARTER_MODELS: StarterModel[] = [
  {
    tag: 'gemma4:e2b',
    label: 'Gemma 4 · e2b',
    blurb: 'The lightest option — comfortable on 4GB VRAM, still fine on 8GB.',
    size_gb: 7.2,
  },
  {
    tag: 'qwen3.5:4b',
    label: 'Qwen3.5 · 4B',
    blurb: 'Strong small model with good tool use — wants 8GB VRAM.',
    size_gb: 3.4,
  },
  {
    tag: 'gemma4:e4b',
    label: 'Gemma 4 · e4b',
    blurb: 'A step up in capability — wants 8GB VRAM.',
    size_gb: 9.6,
  },
  {
    tag: 'qwen3.5:9b',
    label: 'Qwen3.5 · 9B',
    blurb: 'The most capable of this set — needs 16GB VRAM.',
    size_gb: 6.6,
  },
];
