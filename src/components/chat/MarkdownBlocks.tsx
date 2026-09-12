import { memo, useRef, type ReactNode } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';
import 'highlight.js/styles/github.css';
import { CodeBlock } from './CodeBlock';
import { splitIncremental, type SplitCache } from './markdownSplit';
import { ipc } from '@/lib/ipc';

/** Open markdown links in the OS default browser instead of navigating the
    Kitty window itself — Tauri's webview otherwise treats a bare `<a href>`
    as in-window navigation, replacing the app with the page.

    `openUrl`, not `openPath`: the latter does nothing at all on Android (the
    opener plugin's mobile `open_path` sends a payload its own Kotlin side
    cannot parse), which is why links in chat were dead there. It rejects
    anything that is not http(s), so a link a model was talked into emitting
    cannot launch an intent or a local file. */
function ExternalLink({ href, children }: { href?: string; children?: ReactNode }) {
  return (
    <a
      href={href}
      onClick={(e) => {
        e.preventDefault();
        if (href)
          void ipc.openUrl(href).catch(() => {
            /* a non-web scheme is refused Rust-side; nothing useful to show */
          });
      }}
    >
      {children}
    </a>
  );
}

// Module constants, not inline literals: a fresh array or object here would be
// a new prop identity every render and would defeat `MarkdownBlock`'s memo
// entirely, which is the whole point of splitting.
export const MARKDOWN_COMPONENTS = { pre: CodeBlock, a: ExternalLink };
export const REMARK_PLUGINS = [remarkGfm];
export const REHYPE_PLUGINS = [rehypeHighlight];

/** One top-level markdown block. Memoized on its source text, so a streaming
    message only re-parses (and re-highlights) the block currently being
    written — every settled block above it is a memo hit. */
const MarkdownBlock = memo(function MarkdownBlock({ source }: { source: string }) {
  return (
    <ReactMarkdown
      remarkPlugins={REMARK_PLUGINS}
      rehypePlugins={REHYPE_PLUGINS}
      components={MARKDOWN_COMPONENTS}
    >
      {source}
    </ReactMarkdown>
  );
});

/** Drop-in replacement for a single whole-text `<ReactMarkdown>`.

    react-markdown renders into a `Fragment`, which emits no DOM node, so the
    element list this produces is identical to the single-instance version —
    see `markdownSplit.ts` for the full argument and `markdownSplit.test.ts`
    for the differential test that holds it to that.

    The split cache lives in a ref and is updated during render. That is safe
    here because it is a pure memo of the current props: recomputing it (as
    StrictMode's double render does) yields the same result, and a cache miss
    only costs a cold re-scan. */
export function MarkdownBlocks({ text }: { text: string }) {
  const cacheRef = useRef<SplitCache | null>(null);
  const cache = splitIncremental(cacheRef.current, text);
  cacheRef.current = cache;

  return (
    <>
      {cache.blocks.map((source, i) => (
        // Index keys are correct here specifically because the partition is
        // append-only: a block's index never changes once it has settled.
        <MarkdownBlock key={i} source={source} />
      ))}
    </>
  );
}
