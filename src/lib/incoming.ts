// Shares and notification taps from outside the app (Android, A2–A4): the
// Rust side queues them (`commands::take_incoming`), and the hub collects them
// on start and whenever it comes back to the foreground.

import { ipc } from './ipc';
import { useChatStore } from '@/stores/chatStore';
import { useRouteStore } from '@/stores/routeStore';

export type Incoming =
  | { kind: 'share'; text: string; subject: string; paths: string[]; failed: number }
  | { kind: 'open_chat'; session_id: string };

/** The composer text a share brings: its subject and text, without
    repeating one inside the other. Pure. */
export function shareText(i: { text: string; subject: string }): string {
  const text = i.text.trim();
  const subject = i.subject.trim();
  if (!subject || text.includes(subject)) return text;
  return text ? `${subject}\n\n${text}` : subject;
}

/** Act on one incoming intent: a share opens a new chat with it attached; a
    notification opens the chat it was about. */
export async function handleIncoming(item: Incoming): Promise<void> {
  const chat = useChatStore.getState();
  useRouteStore.getState().goto('chat');
  if (item.kind === 'open_chat') {
    if (chat.sessionId !== item.session_id) await chat.loadSession(item.session_id, '');
    return;
  }
  await chat.newSession();
  if (item.paths.length > 0) await useChatStore.getState().addDroppedPaths(item.paths);
  const text = shareText(item);
  if (text) useChatStore.getState().setComposerPrefill(text);
  if (item.failed > 0) {
    useChatStore.setState({
      warning: `${item.failed} shared file${item.failed === 1 ? '' : 's'} couldn't be read from the app that shared ${item.failed === 1 ? 'it' : 'them'}.`,
    });
  }
}

let draining = false;

/** Collect and handle everything queued; while a share's files are still
    copying, ask again shortly. */
export async function drainIncoming(): Promise<void> {
  if (draining) return;
  draining = true;
  try {
    for (let attempt = 0; attempt < 40; attempt++) {
      const batch = await ipc.takeIncoming();
      for (const item of batch.intents as Incoming[]) await handleIncoming(item);
      if (!batch.copying) break;
      await new Promise((r) => setTimeout(r, 500));
    }
  } catch (e) {
    console.warn('takeIncoming failed', e);
  } finally {
    draining = false;
  }
}
