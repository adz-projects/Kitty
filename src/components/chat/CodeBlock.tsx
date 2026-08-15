import { isValidElement, useEffect, useRef, useState, type ReactNode } from 'react';

/** Recursively pull the plain text out of a react-markdown/rehype node tree
    (the fenced code content, minus the syntax-highlight <span> wrappers). */
function nodeText(node: ReactNode): string {
  if (typeof node === 'string') return node;
  if (typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(nodeText).join('');
  if (isValidElement(node)) return nodeText((node.props as { children?: ReactNode }).children);
  return '';
}

const EXT: Record<string, string> = {
  javascript: 'js',
  js: 'js',
  typescript: 'ts',
  ts: 'ts',
  jsx: 'jsx',
  tsx: 'tsx',
  python: 'py',
  py: 'py',
  rust: 'rs',
  bash: 'sh',
  shell: 'sh',
  sh: 'sh',
  json: 'json',
  html: 'html',
  css: 'css',
  c: 'c',
  cpp: 'cpp',
  go: 'go',
  java: 'java',
  yaml: 'yaml',
  yml: 'yaml',
  markdown: 'md',
  md: 'md',
  sql: 'sql',
  toml: 'toml',
};

/** Custom `pre` renderer for react-markdown (Round-2 item 12): fenced code blocks
    get a header showing the language plus Copy and Download buttons. Inline code
    is unaffected (it isn't wrapped in a <pre>). */
export function CodeBlock({ children }: { children?: ReactNode; node?: unknown }) {
  const [copied, setCopied] = useState(false);
  const copyTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Clear a pending "Copied" revert on unmount so it never fires across the
  // component boundary (setState-after-unmount warning + a node sitting in
  // the message list briefly looking copied). The mounted flag covers the
  // clipboard promise itself resolving after unmount — virtual-list rows
  // (and their code blocks) are recycled mid-write all the time.
  const mountedRef = useRef(true);
  useEffect(
    () => () => {
      mountedRef.current = false;
      if (copyTimerRef.current) clearTimeout(copyTimerRef.current);
    },
    []
  );
  const codeEl = isValidElement(children) ? children : null;
  const className = (codeEl?.props as { className?: string } | undefined)?.className ?? '';
  const lang = /language-([\w-]+)/.exec(className)?.[1] ?? '';
  const text = nodeText(children).replace(/\n$/, '');

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      if (!mountedRef.current) return;
      setCopied(true);
      if (copyTimerRef.current) clearTimeout(copyTimerRef.current);
      copyTimerRef.current = setTimeout(() => setCopied(false), 1200);
    } catch {
      /* clipboard may be unavailable */
    }
  };

  const download = () => {
    const ext = EXT[lang.toLowerCase()] ?? 'txt';
    const url = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
    const a = document.createElement('a');
    a.href = url;
    a.download = `snippet.${ext}`;
    a.click();
    // Defer the revoke instead of doing it synchronously: Chromium fetches
    // the blob URL asynchronously from the click, so an immediate revoke can
    // intermittently kill the download before it starts (same reason
    // VisualizationCard's openInNewWindow defers its revoke).
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  };

  return (
    <div className="code-block">
      <div className="code-block-head">
        <span className="code-lang">{lang || 'text'}</span>
        <div className="code-actions">
          <button onClick={() => void copy()}>{copied ? 'Copied' : 'Copy'}</button>
          <button onClick={download}>Download</button>
        </div>
      </div>
      <pre>{children}</pre>
    </div>
  );
}
