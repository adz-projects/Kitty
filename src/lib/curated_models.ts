// Curated GGUFs offered in the first-run wizard and Settings → Local Models.
//
// Successor to `starter_models.ts`, which listed Ollama tags. These name a
// Hugging Face repo and file, since Kitty downloads the weights itself now.
// Repos and filenames are case-sensitive and verified against huggingface.co
// at the time of writing — record changes in docs/ANDROID.md §9.
//
// Deliberately short. Every extra entry is another multi-gigabyte decision to
// put in front of someone on their first run, and the two roles Kitty actually
// needs filled are chat and embeddings.

export interface CuratedModel {
  /** Hugging Face repo, `owner/name`. */
  repo: string;
  /** Exact filename in that repo, including `.gguf`. */
  file: string;
  label: string;
  blurb: string;
  size_gb: number;
  /** What this model is for — the wizard offers one of each. */
  role: 'chat' | 'embedding';
}

export const CURATED_MODELS: CuratedModel[] = [
  {
    repo: 'LiquidAI/LFM2.5-1.2B-Instruct-GGUF',
    file: 'LFM2.5-1.2B-Instruct-Q4_K_M.gguf',
    label: 'LFM2.5 · 1.2B Instruct',
    blurb: 'The default. Fast on CPU, and what compaction and summarising use.',
    size_gb: 0.73,
    role: 'chat',
  },
  {
    repo: 'Qwen/Qwen3-Embedding-0.6B-GGUF',
    file: 'Qwen3-Embedding-0.6B-q4_k_m.gguf',
    label: 'Qwen3 Embedding · 0.6B',
    blurb: 'Gives the memory engine real semantic recall instead of keyword matching.',
    size_gb: 0.38,
    role: 'embedding',
  },
];

export const defaultFor = (role: CuratedModel['role']): CuratedModel | undefined =>
  CURATED_MODELS.find((m) => m.role === role);
