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
    // Q8_0, not q4_k_m. **Qwen never published a q4 for this model** — the
    // official repo has exactly two GGUFs, `Q8_0` and `f16`, so the q4_k_m
    // filename this used to name 404'd and the embedding download could
    // never have succeeded. Q8_0 over f16 because f16 is ~1.2 GB for no
    // recall benefit; going *below* Q8 on an embedder is the trade that
    // actually costs retrieval quality, which is the only reason this model
    // is here.
    file: 'Qwen3-Embedding-0.6B-Q8_0.gguf',
    label: 'Qwen3 Embedding · 0.6B',
    blurb: 'Gives the memory engine real semantic recall instead of keyword matching.',
    size_gb: 0.64,
    role: 'embedding',
  },
];

export const defaultFor = (role: CuratedModel['role']): CuratedModel | undefined =>
  CURATED_MODELS.find((m) => m.role === role);
