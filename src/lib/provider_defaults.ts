import type { ProviderType } from './types';

/** Default base URL per provider type. Shared by Settings → Providers'
    "Add Provider" form and the wizard's API-key step, so both read the same
    source of truth. */
export const DEFAULT_URL: Record<ProviderType, string> = {
  // The in-process engine has no URL — ProviderForm still shows the field for
  // it (only local/ollama/custom_openai do; every model-picker-flow type
  // hides it — see `hasFixedEndpoint` — since those are fixed, well-known
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
