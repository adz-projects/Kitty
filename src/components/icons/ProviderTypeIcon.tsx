import type { ProviderType } from '@/lib/types';

/** Compact per-type glyph for a provider profile card (release-fixes-2 item:
    "Providers should use icons rather than words so each card stays on one
    line"). Deliberately abstract/generic rather than a reproduction of any
    company's actual logo — these just need to be glanceable and distinct
    from each other at 14px, not brand-accurate. Pair with a `title` on the
    wrapping element for the type name, since the icon alone isn't
    self-explanatory. */
export function ProviderTypeIcon({ type }: { type: ProviderType }) {
  switch (type) {
    case 'anthropic':
      // Simple peak/mark.
      return (
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <path
            d="M8 3 3 13h2.2l1-2.4h3.6l1 2.4H13L8 3Zm0 3.6 1.2 3H6.8L8 6.6Z"
            fill="currentColor"
          />
        </svg>
      );
    case 'openai':
      // Interlocking-loop knot, abstracted — not the real mark.
      return (
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <circle cx="6" cy="6" r="3" stroke="currentColor" strokeWidth="1.2" />
          <circle cx="10" cy="10" r="3" stroke="currentColor" strokeWidth="1.2" />
        </svg>
      );
    case 'openrouter':
      // Routing/branching nodes.
      return (
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <circle cx="3.5" cy="8" r="1.6" fill="currentColor" />
          <circle cx="12.5" cy="3.5" r="1.6" fill="currentColor" />
          <circle cx="12.5" cy="12.5" r="1.6" fill="currentColor" />
          <path
            d="M5 8h2.5c1 0 1.5-.5 1.5-1.5S9 5 9 5m-4 3h2.5c1 0 1.5.5 1.5 1.5S9 11 9 11"
            stroke="currentColor"
            strokeWidth="1.1"
          />
        </svg>
      );
    case 'custom_openai':
      // Plug — a hand-configured, OpenAI-compatible endpoint.
      return (
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <path
            d="M6 2v3M10 2v3M4.5 5h7v2.5a3.5 3.5 0 0 1-7 0V5ZM8 9.5V13"
            stroke="currentColor"
            strokeWidth="1.2"
            strokeLinecap="round"
          />
        </svg>
      );
    case 'ollama':
      // Small server/rack — a self-hosted endpoint the user runs.
      return (
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <rect x="3" y="3" width="10" height="4" rx="1" stroke="currentColor" strokeWidth="1.1" />
          <rect x="3" y="9" width="10" height="4" rx="1" stroke="currentColor" strokeWidth="1.1" />
          <circle cx="5" cy="5" r="0.6" fill="currentColor" />
          <circle cx="5" cy="11" r="0.6" fill="currentColor" />
        </svg>
      );
    case 'fireworks':
      // Simple flame — abstracted, not the real mark.
      return (
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <path
            d="M8 2c1 2-1.5 2.7-1.5 4.6C6.5 5.8 5.4 5.2 5.4 5.2 4 6.5 3.5 8 3.5 9.4a4.5 4.5 0 0 0 9 0c0-2-1.2-3.4-2.3-4.6.2 1-.5 1.6-1 1.6C8.6 5 8.6 3 8 2Z"
            fill="currentColor"
          />
        </svg>
      );
    case 'qwen_cloud':
      // Cloud outline — a hosted, cloud-run endpoint.
      return (
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <path
            d="M5 11.5a3 3 0 0 1-.5-5.96 3.5 3.5 0 0 1 6.7-1.2A3 3 0 0 1 11.5 11.5H5Z"
            stroke="currentColor"
            strokeWidth="1.2"
            strokeLinejoin="round"
          />
        </svg>
      );
    case 'deepinfra':
      // Stacked layers — infrastructure the vendor runs underneath the API.
      return (
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <path
            d="m8 2.5 5.5 3L8 8.5 2.5 5.5 8 2.5Z"
            stroke="currentColor"
            strokeWidth="1.1"
            strokeLinejoin="round"
          />
          <path
            d="m2.5 8 5.5 3 5.5-3M2.5 10.5 8 13.5l5.5-3"
            stroke="currentColor"
            strokeWidth="1.1"
            strokeLinejoin="round"
          />
        </svg>
      );
    case 'local':
    default:
      // Chip/device — runs in-process on this machine.
      return (
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <rect x="5" y="5" width="6" height="6" rx="1" stroke="currentColor" strokeWidth="1.2" />
          <path
            d="M8 2v2M8 12v2M2 8h2M12 8h2M4.5 4.5l1 1M10.5 10.5l1 1M11.5 4.5l-1 1M5.5 10.5l-1 1"
            stroke="currentColor"
            strokeWidth="1"
            strokeLinecap="round"
          />
        </svg>
      );
  }
}

const TYPE_LABEL: Record<ProviderType, string> = {
  anthropic: 'Anthropic',
  openai: 'OpenAI',
  openrouter: 'OpenRouter',
  fireworks: 'Fireworks',
  qwen_cloud: 'QwenCloud',
  deepinfra: 'DeepInfra',
  custom_openai: 'Custom (OpenAI-compatible)',
  ollama: 'Ollama (self-hosted)',
  local: 'On this device',
};

export function providerTypeLabel(type: ProviderType): string {
  return TYPE_LABEL[type] ?? type;
}
