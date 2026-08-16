// Curated models offered in the first-run wizard and Settings → Local Models.
//
// As of the LiteRT migration these are **LiteRT** artifacts, not GGUFs: Kitty's
// local engine is LiteRT (see the repo plan "Replace llama.cpp with LiteRT").
//   • embedding — EmbeddingGemma `.tflite` (both platforms; the shared vector
//     space for adaptive-pathway memory).
//   • chat — the generative summarizer `.litertlm`, run by LiteRT-LM on
//     Windows only (Android offloads compaction to the remote chat model, so it
//     downloads only the embedding model).
//
// Repos/filenames are case-sensitive and verified against huggingface.co at the
// time of writing — record changes in docs/ANDROID.md §9.
//
// The Gemma `tokenizer.json` the embedder needs is **bundled as an app
// resource**, not downloaded here (the LiteRT repo ships only
// `sentencepiece.model`; the canonical `tokenizer.json` is converted/bundled at
// build time). See docs/RELEASE.md.

export interface CuratedModel {
  /** Hugging Face repo, `owner/name`. */
  repo: string;
  /** Exact filename in that repo (`.tflite` or `.litertlm`). */
  file: string;
  label: string;
  blurb: string;
  size_gb: number;
  /** What this model is for — the wizard offers one of each. */
  role: 'chat' | 'embedding';
  /**
   * Repo is gated under a license (e.g. Gemma) and needs an accepted license +
   * an HF access token to download. The wizard/downloader prompts for a token
   * when this is set.
   */
  gated?: boolean;
  /**
   * Only usable on desktop. The generative summarizer runs via LiteRT-LM on
   * Windows only — Android offloads compaction to the remote chat model, so no
   * generative model runs on the phone. Such models must not be offered for
   * download on Android (Settings or the wizard).
   */
  desktopOnly?: boolean;
}

/** Curated models offered on the current platform — drops desktop-only models
    (the Windows LiteRT-LM summarizer) on Android. */
export const curatedModelsFor = (android: boolean): CuratedModel[] =>
  CURATED_MODELS.filter((m) => !(android && m.desktopOnly));

export const CURATED_MODELS: CuratedModel[] = [
  {
    repo: 'litert-community/gemma-4-E2B-it-litert-lm',
    file: 'gemma-4-E2B-it.litertlm',
    label: 'Gemma 4 · E2B Instruct (LiteRT-LM)',
    blurb: 'Windows only. Runs compaction and summarising locally on the desktop.',
    size_gb: 2.59,
    role: 'chat',
    desktopOnly: true, // Android offloads compaction to the remote chat model.
  },
  {
    // EmbeddingGemma ships only `.tflite` variants (+ `sentencepiece.model`).
    // The generic `seq256` mixed-precision build runs on CPU on any device; a
    // per-SoC NPU variant (e.g. `...google.tensor_g5.tflite`) is an
    // optimisation layered on later.
    repo: 'litert-community/embeddinggemma-300m',
    file: 'embeddinggemma-300M_seq256_mixed-precision.tflite',
    label: 'EmbeddingGemma · 300M',
    blurb: 'Gives the memory engine real semantic recall instead of keyword matching.',
    size_gb: 0.18,
    role: 'embedding',
    gated: true, // Gemma license: needs an accepted license + HF token.
  },
];

export const defaultFor = (role: CuratedModel['role']): CuratedModel | undefined =>
  CURATED_MODELS.find((m) => m.role === role);
