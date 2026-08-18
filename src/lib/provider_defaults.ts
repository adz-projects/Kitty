import type { ProviderType } from './types';

/** Default base URL per provider type. Shared by Settings → Providers'
    "Add Provider" form and the wizard's API-key step, so both read the same
    source of truth. */
export const DEFAULT_URL: Record<ProviderType, string> = {
  // The in-process engine has no URL — ProviderForm still shows the field for
  // it (only local/ollama/custom_openai do; every model-picker-flow type
  // hides it — see `usesModelPicker` — since those are fixed, well-known
  // endpoints the user never needs to see), so this stays an empty,
  // editable default rather than a value that would look meaningful.
  local: '',
  ollama: 'http://localhost:11434',
  openrouter: 'https://openrouter.ai/api/v1',
  anthropic: 'https://api.anthropic.com',
  openai: 'https://api.openai.com/v1',
  fireworks: 'https://api.fireworks.ai/inference/v1',
  deepinfra: 'https://api.deepinfra.com/v1/openai',
  custom_openai: '',
};

/** Sensible pre-filled model id per provider type, for the wizard's API-key
    step — free text, not a fetched live list (catalogs go stale fast and
    aren't worth a new network dependency for a first-run wizard). The user
    can always change it; this just avoids a blank, intimidating field. */
export const DEFAULT_MODEL: Partial<Record<ProviderType, string>> = {
  anthropic: 'claude-sonnet-5',
  openai: 'gpt-5.1',
  openrouter: 'anthropic/claude-sonnet-5',
};
