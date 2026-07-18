import { ipc } from '@/lib/ipc';
import { findHintToolCall, parseHintOutput, useChatStore, type Message } from '@/stores/chatStore';
import { ThumbUpIcon } from '@/components/icons/ThumbUpIcon';
import { ThumbDownIcon } from '@/components/icons/ThumbDownIcon';
import { LightbulbIcon } from '@/components/icons/LightbulbIcon';
import { RefreshIcon } from '@/components/icons/RefreshIcon';

const FEEDBACK_BUTTONS: { Icon: typeof ThumbUpIcon; type: string; title: string }[] = [
  { Icon: ThumbUpIcon, type: 'keep_this', title: 'Keep suggesting this' },
  { Icon: ThumbDownIcon, type: 'dont_do_again', title: "Don't suggest this again" },
  { Icon: LightbulbIcon, type: 'explore_alternative', title: 'Explore an alternative' },
  { Icon: RefreshIcon, type: 'retry_same_intent', title: 'Retry with the same intent' },
];

/** Feedback on an Adaptive Pathway hint (Round-C) — added into the existing
    hover-only `.msg-actions` row, alongside Branch/Regenerate/Copy.
    v1 simplification: feedback applies to the message's first hint's edge
    (a single `decide` call typically surfaces a small handful of hints for
    the same decision point) rather than per-hint feedback. */
export function HintFeedbackButtons({ message }: { message: Message }) {
  const sessionId = useChatStore((s) => s.sessionId);
  const call = findHintToolCall(message);
  const parsed = parseHintOutput(call);
  if (!parsed || !sessionId) return null;
  const edgeId = parsed.hints[0]?.edge_id;

  const send = (type: string) => {
    void ipc.adaptivePathwayRecordAnnotation(sessionId, type, edgeId ?? null, null, 0.8);
  };

  return (
    <>
      {FEEDBACK_BUTTONS.map(({ Icon, type, title }) => (
        <button key={type} title={title} onClick={() => send(type)}>
          <Icon />
        </button>
      ))}
    </>
  );
}
