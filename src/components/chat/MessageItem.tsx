import { memo, useMemo, type MouseEvent } from 'react';
import {
  isVisualizationToolCall,
  stripInternalMarkers,
  stripPromptPreamble,
  type Message,
} from '@/stores/chatStore';
import { useMobileUiStore } from '@/stores/mobileUiStore';
import { isAndroid } from '@/lib/platform';
import { ThinkingBox } from './ThinkingBox';
import { PreviousAttemptBox } from './PreviousAttemptBox';
import { MessageActions } from './MessageActions';
import { MessageAttachmentChips } from './MessageAttachmentChips';
import { VisualizationCard } from './VisualizationCard';
import { MarkdownBlocks } from './MarkdownBlocks';

/** Taps on these do their own thing and must not also toggle the message's
    actions: links, buttons (code-block copy, the Thinking toggle, the actions
    themselves), and interactive cards. */
const TAP_IGNORE = 'a, button, input, textarea, select, summary, label, .viz-card, .msg-actions';

/** One chat message. User turns render as a plain bubble; assistant turns render
    markdown, with an optional collapsible reasoning block and tool cards. Actions
    (Branch, Export, Regenerate, Copy, info) live in `MessageActions`: hover-revealed
    on desktop, tap-revealed on Android.

    Memoized (Round-7 perf fix): a session replay/live stream re-renders the
    parent list on every incoming event, but only ever changes one message at
    a time — without this, every already-rendered historical message (incl.
    its full markdown/syntax-highlight pass) re-executed on every single
    unrelated event too, an O(n²) cost across a long replay. */
export const MessageItem = memo(function MessageItem({
  message,
  index,
}: {
  message: Message;
  index: number;
}) {
  const toggleReveal = useMobileUiStore((s) => s.toggleRevealMessage);

  // Two fresh arrays per render, and this component is the one that genuinely
  // does re-render every animation frame — it is the streaming message. Keyed
  // on `toolCalls` identity, which only changes when a tool-call event lands.
  // Declared up here, above the early returns below, because hooks have to run
  // in the same order on every render.
  const { vizCalls, otherToolCalls } = useMemo(
    () => ({
      vizCalls: message.toolCalls.filter(isVisualizationToolCall),
      otherToolCalls: message.toolCalls.filter((c) => !isVisualizationToolCall(c)),
    }),
    [message.toolCalls]
  );

  // Android: tapping a message shows its actions below it. Skipped while a
  // long-press text selection is active — lifting the finger after selecting
  // text shouldn't also pop the actions open.
  const onTap = isAndroid()
    ? (e: MouseEvent<HTMLDivElement>) => {
        if ((e.target as Element).closest(TAP_IGNORE)) return;
        if (window.getSelection()?.toString()) return;
        toggleReveal(message.id);
      }
    : undefined;

  const actions = <MessageActions message={message} index={index} />;

  if (message.role === 'user') {
    // Defensive: the live-typed bubble and the replay path both already keep
    // this clean (see chatStore.ts's stripPromptPreamble), but stripping
    // again here at the single render chokepoint costs nothing on already-
    // clean text (the wrapper regexes simply won't match) and guarantees the
    // raw <system>/transcript preamble can never surface in the chat, no
    // matter which code path a message's text came from.
    const displayText = stripInternalMarkers(
      index === 0 ? stripPromptPreamble(message.text) : message.text
    );
    return (
      <div className="msg msg-user" onClick={onTap}>
        <div className="bubble">{displayText}</div>
        <MessageAttachmentChips files={message.attachedFiles} />
        {actions}
      </div>
    );
  }

  if (message.superseded) {
    // A regenerated-away-from answer: collapsed, no actions — regenerating,
    // branching, etc. don't make sense against a superseded turn.
    return (
      <div className="msg msg-assistant">
        <PreviousAttemptBox message={message} />
      </div>
    );
  }

  // Visualizations render as their own always-visible card, the same way a
  // fenced code block renders inline rather than behind a click — everything
  // else stays in the collapsed Thinking tray.

  return (
    <div className="msg msg-assistant" onClick={onTap}>
      {(message.reasoning || message.draftText || otherToolCalls.length > 0) && (
        <ThinkingBox
          reasoning={message.reasoning}
          toolCalls={otherToolCalls}
          streaming={message.streaming}
          hasAnswer={message.text.length > 0}
          draft={message.draftText}
        />
      )}
      {vizCalls.map((call) => (
        <VisualizationCard key={call.id} call={call} />
      ))}
      {message.text && (
        <div className="bubble markdown">
          <MarkdownBlocks text={message.text} />
        </div>
      )}
      {actions}
    </div>
  );
});
