import type { ChatNoticeEvent } from '@/lib/types';

/** The line shown under a reply for a `chat://notice`. Written here rather
    than taken from the engine's own message so the wording is Kitty's, with
    the engine's text as the fallback for a reason Kitty does not know. */
export function noticeText(e: ChatNoticeEvent): string {
  if (e.kind === 'step_limit') {
    return 'This turn reached its step limit and stopped early. Ask Kitty to continue if it isn’t finished.';
  }
  const to = e.model ? `${e.model}` : 'another model';
  switch (e.reason) {
    case 'pinned_unavailable':
      return `This chat’s model wasn’t available, so ${to} answered instead.`;
    case 'no_tool_support':
      return `This chat’s model can’t use tools, so ${to} handled this step.`;
    case 'error_switch':
      return `This chat’s model failed, so Kitty switched to ${to}.`;
    default:
      return e.message ?? `Kitty switched to ${to} for this reply.`;
  }
}
