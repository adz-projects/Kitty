// Curated starter models for the first-run wizard — all ≤4B params so they run
// on essentially any hardware. Tags are verified against ollama.com at build
// time and recorded in docs/VERSIONS.md.

export interface StarterModel {
  tag: string;
  label: string;
  blurb: string;
  size_gb: number;
}

export const STARTER_MODELS: StarterModel[] = [
  {
    tag: 'llama3.2:1b',
    label: 'Llama 3.2 · 1B',
    blurb: 'Tiny and fast — runs on almost anything.',
    size_gb: 1.3,
  },
  {
    tag: 'llama3.2:3b',
    label: 'Llama 3.2 · 3B',
    blurb: 'A small, capable general assistant.',
    size_gb: 2.0,
  },
  {
    tag: 'qwen2.5:3b',
    label: 'Qwen2.5 · 3B',
    blurb: 'Strong small model with good tool use.',
    size_gb: 1.9,
  },
  {
    tag: 'gemma2:2b',
    label: 'Gemma 2 · 2B',
    blurb: 'Compact and efficient.',
    size_gb: 1.6,
  },
];
