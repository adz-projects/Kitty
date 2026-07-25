import { useEffect, useRef, useState } from 'react';
import { isChatMode, useChatStore } from '@/stores/chatStore';
import { ipc, onRecipesChanged, pickFiles } from '@/lib/ipc';
import { matchRecipeCommand, primaryParameter, recipeNeedsAttention } from '@/lib/recipes';
import type { Recipe } from '@/lib/types';
import { usePopoverPosition } from '@/lib/usePopoverPosition';
import { useRecipeAutocomplete } from '@/lib/useRecipeAutocomplete';
import { UploadIcon } from '@/components/icons/UploadIcon';
import { CameraIcon } from '@/components/icons/CameraIcon';
import { WarningIcon } from '@/components/icons/WarningIcon';
import { supportsImages } from '@/lib/vision_models';

// Pastes larger than this (chat-only mode) collapse into a document attachment.
const PASTE_THRESHOLD = 500;
// Matches `.composer textarea`'s max-height in base.css — the textarea only
// gets a scrollbar once content actually grows past this cap.
const MAX_TEXTAREA_HEIGHT = 160;
const DEFAULT_PLACEHOLDER = 'Ask Kitty…';

/** Message composer: Enter sends, Shift+Enter inserts a newline. While a reply
    streams, sending is blocked and a Stop button cancels the turn. In chat-only
    mode, large pastes collapse into an inlined document attachment. Typing
    `/` at the start shows a recipe-command dropdown (Goose recipes,
    client-side-interpreted — see `chatStore.ts`'s `sendWithRecipe`); accepting
    one inserts `/slug ` and shows a guidance hint (above the composer) telling
    the user what to type after the slug.

    `concluded` (chatStore's `sessionConcluded`, see `loadSession`) locks the
    composer entirely — this session's provider profile was deleted since it
    was last used, and there's no ACP mechanism to keep chatting on a
    provider Kitty can no longer restore. History still shows above; only
    new input is blocked. */
export function Composer({
  onSend,
  onStop,
  disabled,
  concluded,
  sendBlocked,
}: {
  onSend: (text: string) => void;
  onStop: () => void;
  disabled: boolean;
  concluded?: boolean;
  /** Blocks submission without swapping the button to the Stop/streaming UI
      (unlike `disabled`, which implies a response is actively streaming) —
      used while the stack is still warming up at startup. */
  sendBlocked?: boolean;
}) {
  const [text, setText] = useState('');
  const ref = useRef<HTMLTextAreaElement>(null);
  const chatOnly = useChatStore(isChatMode);
  const addPastedText = useChatStore((s) => s.addPastedText);
  const addDroppedPaths = useChatStore((s) => s.addDroppedPaths);
  const addPendingImage = useChatStore((s) => s.addPendingImage);
  const model = useChatStore((s) => s.model);
  const sendWithRecipe = useChatStore((s) => s.sendWithRecipe);
  const stopPhase = useChatStore((s) => s.stopPhase);
  const forceStop = useChatStore((s) => s.forceStop);

  const [recipes, setRecipes] = useState<Recipe[]>([]);
  useEffect(() => {
    const load = () =>
      void ipc
        .listRecipes()
        .then(setRecipes)
        .catch(() => {});
    load();
    const un = onRecipesChanged(load);
    return () => void un.then((fn) => fn());
  }, []);

  const { open, matches, selectedIndex, setSelectedIndex, dismiss } = useRecipeAutocomplete(
    text,
    recipes
  );
  const { triggerRef, popoverRef, style } = usePopoverPosition(open, dismiss);

  // A fully-matched `/slug` in the composer. Drives the guidance hint below —
  // NOT the textarea placeholder: a placeholder only renders when the field is
  // empty, but by the time a slug is matched the field always has `/slug …` in
  // it, so a placeholder would never actually show. The dropdown covers the
  // typing-the-slug phase; this hint covers the after-acceptance phase, where
  // the user needs to know what to type after the slug.
  const recipeMatch = matchRecipeCommand(text, recipes);
  const recipeHint = recipeMatch ? primaryParameter(recipeMatch.recipe)?.description : undefined;

  const resetTextareaHeight = () => {
    if (resizeRaf.current) cancelAnimationFrame(resizeRaf.current);
    if (!ref.current) return;
    ref.current.style.height = 'auto';
    ref.current.style.overflowY = 'hidden';
  };

  // Auto-grow the textarea to fit its content, capped at MAX_TEXTAREA_HEIGHT.
  // Coalesced into a single rAF so a burst of keystrokes forces at most one
  // layout per frame instead of a synchronous reflow inside every `onChange`
  // (which added perceptible typing latency on slower machines).
  const resizeRaf = useRef<number | null>(null);
  const scheduleResize = () => {
    if (resizeRaf.current) cancelAnimationFrame(resizeRaf.current);
    resizeRaf.current = requestAnimationFrame(() => {
      resizeRaf.current = null;
      const el = ref.current;
      if (!el) return;
      el.style.height = 'auto';
      el.style.height = `${Math.min(el.scrollHeight, MAX_TEXTAREA_HEIGHT)}px`;
      el.style.overflowY = el.scrollHeight > MAX_TEXTAREA_HEIGHT ? 'auto' : 'hidden';
    });
  };

  // Cancel any pending resize frame on unmount.
  useEffect(
    () => () => {
      if (resizeRaf.current) cancelAnimationFrame(resizeRaf.current);
    },
    []
  );

  const acceptRecipe = (recipe: Recipe) => {
    const next = `/${recipe.slug} `;
    setText(next);
    requestAnimationFrame(() => {
      ref.current?.setSelectionRange(next.length, next.length);
      ref.current?.focus();
    });
  };

  const submit = () => {
    const value = text.trim();
    if (!value || disabled || concluded || sendBlocked) return;
    const match = matchRecipeCommand(value, recipes);
    if (match) {
      void sendWithRecipe(match.recipe, match.primaryText);
    } else {
      onSend(value);
    }
    setText('');
    resetTextareaHeight();
  };

  // Button-triggered file attach — same pipeline as OS drag-drop (Round-5).
  const attachFiles = async () => {
    const paths = await pickFiles();
    if (paths.length) await addDroppedPaths(paths);
  };

  // Screenshot region capture (Feature 3): opens a full-desktop selection
  // overlay and, once the user drags a region, attaches the resulting crop
  // through the exact same pending-image pipeline a clipboard-pasted image
  // already uses (`addPendingImage`) — no separate attachment type needed.
  // Checked against the active model *before* opening the capture UI (not
  // just relying on `addPendingImage`'s own gate) so a doomed capture never
  // shows the full-screen overlay in the first place.
  const captureScreenshot = async () => {
    if (!supportsImages(model)) {
      useChatStore.setState({
        warning: "The active model doesn't support images — screenshot not attached.",
      });
      return;
    }
    try {
      const { mime, data_url } = await ipc.captureScreenshotRegion();
      addPendingImage(mime, data_url);
    } catch {
      // Cancelled (Escape) — nothing to surface, same as declining a file picker.
    }
  };

  return (
    <div className="composer">
      {recipeMatch && !open && (
        <div className="composer-recipe-hint">
          <strong>/{recipeMatch.recipe.slug}</strong>
          {recipeHint ? ` — ${recipeHint}` : ` — ${recipeMatch.recipe.title}`}
        </div>
      )}
      <button
        className="composer-attach"
        onClick={() => void attachFiles()}
        title="Attach files"
        aria-label="Attach files"
        disabled={concluded}
      >
        <UploadIcon />
      </button>
      <button
        className="composer-attach"
        onClick={() => void captureScreenshot()}
        title="Capture a screenshot region"
        aria-label="Capture a screenshot region"
        disabled={concluded}
      >
        <CameraIcon />
      </button>
      <textarea
        disabled={concluded}
        ref={(el: HTMLTextAreaElement | null) => {
          // Both `ref` (this component's own, for `setSelectionRange`/height
          // adjustments) and `usePopoverPosition`'s `triggerRef` come back
          // from `useRef` typed as read-only-looking `RefObject`s (matching
          // how other callers just hand one straight to a `ref` prop, e.g.
          // ProviderBadge) — this textarea needs both pointed at the same
          // node, so both assignments are cast; it's a type-level formality,
          // not a different underlying object.
          (ref as React.MutableRefObject<HTMLTextAreaElement | null>).current = el;
          (triggerRef as React.MutableRefObject<HTMLElement | null>).current = el;
        }}
        rows={1}
        autoFocus
        value={text}
        placeholder={concluded ? 'Chat concluded.' : DEFAULT_PLACEHOLDER}
        onChange={(e) => {
          setText(e.target.value);
          // Resize is coalesced into a rAF (see scheduleResize) so typing never
          // forces a synchronous reflow on the keystroke path.
          scheduleResize();
        }}
        onKeyDown={(e) => {
          if (open) {
            if (e.key === 'ArrowDown') {
              e.preventDefault();
              setSelectedIndex((i) => (i + 1) % matches.length);
              return;
            }
            if (e.key === 'ArrowUp') {
              e.preventDefault();
              setSelectedIndex((i) => (i - 1 + matches.length) % matches.length);
              return;
            }
            if (e.key === 'Escape') {
              e.preventDefault();
              dismiss();
              return;
            }
            if ((e.key === 'Enter' || e.key === 'Tab') && matches[selectedIndex]) {
              e.preventDefault();
              acceptRecipe(matches[selectedIndex]);
              return;
            }
          }
          if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            submit();
          }
        }}
        onPaste={(e) => {
          if (!chatOnly) return; // agentic mode keeps native paste
          const pasted = e.clipboardData.getData('text');
          if (pasted.length > PASTE_THRESHOLD) {
            e.preventDefault();
            addPastedText(pasted);
          }
        }}
      />
      {open && (
        <div ref={popoverRef} className="mode-popover recipe-popover" role="listbox" style={style}>
          {matches.map((r, i) => (
            <button
              key={r.id}
              role="option"
              aria-selected={i === selectedIndex}
              className={i === selectedIndex ? 'active' : ''}
              onClick={() => acceptRecipe(r)}
            >
              <span className="recipe-option-title">
                {recipeNeedsAttention(r).length > 0 && <WarningIcon />}/{r.slug} — {r.title}
              </span>
              {primaryParameter(r)?.description && (
                <span className="recipe-option-hint muted">{primaryParameter(r)?.description}</span>
              )}
            </button>
          ))}
        </div>
      )}
      {disabled ? (
        stopPhase === 'forceable' ? (
          <button
            className="force-stop"
            onClick={forceStop}
            title="Force stop — reset now (Kitty may still be finishing in the background)"
          >
            Force stop
          </button>
        ) : stopPhase === 'stopping' ? (
          <button disabled title="Waiting for Kitty to stop…">
            Stopping…
          </button>
        ) : (
          <button onClick={onStop} title="Stop the current response">
            Stop
          </button>
        )
      ) : (
        <button
          className="primary"
          onClick={submit}
          disabled={!text.trim() || concluded || sendBlocked}
        >
          Send
        </button>
      )}
    </div>
  );
}
