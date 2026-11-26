import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import {
  ipc,
  onClipboardAttach,
  onConfigChanged,
  onEffortDetected,
  onFileDrop,
  onOverlayShown,
  onOverlaySubmitFailed,
  onProviderActivated,
  pickFiles,
} from '@/lib/ipc';
import { useStackStore, selectBooting } from '@/stores/stackStore';
import { TrustIcon } from '@/lib/provider_trust';
import { modelAcceptsImages } from '@/lib/vision_models';
import { pastedFileKind, readAsDataUrl } from '@/lib/pasteFiles';
import { effortLabel, selectedEffort } from '@/lib/effort';
import { isImageFileName } from '@/stores/chat/messageUtils';
import { setupTitle } from '@/components/settings/setups/setupUtils';
import { UploadIcon } from '@/components/icons/UploadIcon';
import { DocumentIcon } from '@/components/icons/DocumentIcon';
import { FolderIcon } from '@/components/icons/FolderIcon';
import { SettingsGearIcon } from '@/components/icons/SettingsGearIcon';
import { SparkIcon, SparkOffIcon } from '@/components/icons/SparkIcon';
import { ContextMenu } from '@/components/shared/ContextMenu';
import { useEscapeLayer } from '@/hooks/useEscapeLayer';
import type {
  EffortOption,
  OverlayPromptRequest,
  PathInfo,
  ProviderView,
  StackStatus,
  ThinkingEffort,
} from '@/lib/types';

const DEGRADED: StackStatus[] = ['backend_down'];
const PASTE_THRESHOLD = 500;
const MAX_TEXTAREA_HEIGHT = 160;
const POPOVER_HEADROOM = 240;
const MAX_OVERLAY_HEIGHT = 460;

interface LocalDoc {
  id: string;
  label: string;
  content: string;
}

interface LocalImage {
  id: string;
  label: string;
  mime: string;
  dataUrl: string;
}

let seq = 0;
const nextId = () => `ov_${Date.now()}_${++seq}`;

function Sparks({ option }: { option: EffortOption }) {
  if (option.stars === 0) return <SparkOffIcon />;
  return (
    <span className="effort-sparks">
      {Array.from({ length: option.stars }, (_, i) => (
        <SparkIcon key={i} />
      ))}
    </span>
  );
}

/** Minimal Windows-"Run"-style overlay: prompt bar, attach button, model
    pill, reasoning effort pill, and send button. Always starts a new chat in
    the background and closes immediately on Send. */
export function App() {
  const status = useStackStore((s) => s.status);
  const initStack = useStackStore((s) => s.init);
  const booting = useStackStore(selectBooting);
  const degraded = DEGRADED.includes(status);
  const bootSpinner = status === 'backend_down' && booting;

  const [text, setText] = useState('');
  const [files, setFiles] = useState<PathInfo[]>([]);
  const [docs, setDocs] = useState<LocalDoc[]>([]);
  const [images, setImages] = useState<LocalImage[]>([]);
  const [notice, setNotice] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const [providers, setProviders] = useState<ProviderView[]>([]);
  const [selectedProviderId, setSelectedProviderId] = useState<string | null>(null);
  const [thinkingEffort, setThinkingEffort] = useState<ThinkingEffort | null>(null);
  const [selectedEffortValue, setSelectedEffortValue] = useState<string | null>(null);

  const [modelOpen, setModelOpen] = useState(false);
  const [effortOpen, setEffortOpen] = useState(false);

  const cardRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const modelWrapRef = useRef<HTMLDivElement>(null);
  const effortWrapRef = useRef<HTMLDivElement>(null);
  const resizeRaf = useRef<number | null>(null);
  const submittingRef = useRef(false);

  const menuOpen = modelOpen || effortOpen;
  useEscapeLayer(menuOpen, () => {
    setModelOpen(false);
    setEffortOpen(false);
  });

  const usableProviders = useMemo(() => providers.filter((p) => !p.disabled_reason), [providers]);

  const activeProvider = useMemo(() => {
    if (selectedProviderId) {
      const hit = usableProviders.find((p) => p.id === selectedProviderId);
      if (hit) return hit;
    }
    return usableProviders.find((p) => p.active) ?? usableProviders[0] ?? null;
  }, [usableProviders, selectedProviderId]);
  const activeProviderId = activeProvider?.id ?? null;
  const toolsSupported = activeProvider ? activeProvider.tools_supported : true;

  const canAttachImages = modelAcceptsImages(
    activeProvider?.models[0] ?? null,
    activeProvider?.supports_vision ?? false,
    activeProvider?.accepts_images ?? null
  );

  const activeEffortOption = useMemo(() => {
    if (!thinkingEffort) return null;
    if (selectedEffortValue) {
      const found = thinkingEffort.options.find((o) => o.value === selectedEffortValue);
      if (found) return found;
    }
    return selectedEffort(thinkingEffort);
  }, [thinkingEffort, selectedEffortValue]);

  const refreshProviders = useCallback(async () => {
    try {
      const list = await ipc.listProviders();
      setProviders(list);
    } catch {
      // keep existing list if backend is temporarily unreachable
    }
  }, []);

  const refreshEffort = useCallback(async (cardId: string | null) => {
    try {
      const eff = await ipc.getCardEffort(cardId);
      setThinkingEffort(eff);
      setSelectedEffortValue((prev) =>
        prev && eff?.options.some((o) => o.value === prev) ? prev : null
      );
    } catch {
      setThinkingEffort(null);
    }
  }, []);

  const scheduleTextareaResize = useCallback(() => {
    if (resizeRaf.current) cancelAnimationFrame(resizeRaf.current);
    resizeRaf.current = requestAnimationFrame(() => {
      resizeRaf.current = null;
      const el = textareaRef.current;
      if (!el) return;
      el.style.height = 'auto';
      el.style.height = `${Math.min(el.scrollHeight, MAX_TEXTAREA_HEIGHT)}px`;
      el.style.overflowY = el.scrollHeight > MAX_TEXTAREA_HEIGHT ? 'auto' : 'hidden';
    });
  }, []);

  const resetTextareaHeight = () => {
    if (resizeRaf.current) cancelAnimationFrame(resizeRaf.current);
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.overflowY = 'hidden';
  };

  useEffect(() => {
    void initStack();
    void refreshProviders();

    const focusInput = () => {
      requestAnimationFrame(() => textareaRef.current?.focus());
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !e.defaultPrevented) {
        void ipc.hideOverlay();
      }
    };
    window.addEventListener('keydown', onKey);
    window.addEventListener('focus', focusInput);

    const un = [
      onOverlayShown(focusInput),
      onOverlaySubmitFailed(({ text: failedText, error }) => {
        setText((cur) => (cur.trim().length === 0 ? failedText : cur));
        scheduleTextareaResize();
        setNotice(error);
      }),
      onConfigChanged(() => void refreshProviders()),
      onProviderActivated(() => void refreshProviders()),
    ];
    return () => {
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('focus', focusInput);
      un.forEach((u) => void u.then((fn) => fn()));
    };
  }, [initStack, refreshProviders, scheduleTextareaResize]);

  useEffect(() => {
    void refreshEffort(activeProviderId);
    const un = onEffortDetected((p) => {
      if (!activeProviderId || p.provider_id === activeProviderId) {
        void refreshEffort(activeProviderId);
      }
    });
    return () => {
      void un.then((fn) => fn());
    };
  }, [activeProviderId, refreshEffort]);

  // Close popovers on click/tap outside.
  useEffect(() => {
    if (!menuOpen) return;
    const onPointerDown = (e: PointerEvent) => {
      const target = e.target as Node;
      if (modelWrapRef.current?.contains(target) || effortWrapRef.current?.contains(target)) {
        return;
      }
      setModelOpen(false);
      setEffortOpen(false);
    };
    document.addEventListener('pointerdown', onPointerDown);
    return () => document.removeEventListener('pointerdown', onPointerDown);
  }, [menuOpen]);

  // Resize the window height dynamically to fit the card + upward popover headroom.
  const syncWindowHeight = useCallback(() => {
    const card = cardRef.current;
    if (!card) return;
    const cardHeight = Math.ceil(card.getBoundingClientRect().height);
    if (cardHeight <= 0) return;
    const target = menuOpen
      ? Math.min(MAX_OVERLAY_HEIGHT, cardHeight + POPOVER_HEADROOM)
      : cardHeight + 2;
    void ipc.setOverlayHeight(target).catch(() => {});
  }, [menuOpen]);

  useLayoutEffect(() => {
    syncWindowHeight();
    const card = cardRef.current;
    if (!card || typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(() => syncWindowHeight());
    ro.observe(card);
    return () => ro.disconnect();
  }, [syncWindowHeight]);

  const addPaths = useCallback(
    async (paths: string[]) => {
      if (!paths.length) return;
      try {
        let infos = await ipc.inspectPaths(paths);
        const notices: string[] = [];

        if (!canAttachImages) {
          const rejected = infos.filter((f) => !f.is_dir && isImageFileName(f.name));
          if (rejected.length) {
            infos = infos.filter((f) => !rejected.includes(f));
            notices.push(
              `The active model doesn't support images — skipped: ${rejected
                .map((f) => f.name)
                .join(', ')}.`
            );
          }
        }

        // Toolless providers cannot invoke file-system tools; inline text and
        // supported images concurrently, and reject directories or binary files.
        if (!toolsSupported) {
          const skippedNoTools: string[] = infos.filter((f) => f.is_dir).map((f) => f.name);
          const candidateFiles = infos.filter((f) => !f.is_dir);

          const readResults = await Promise.all(
            candidateFiles.map(async (f) => ({
              f,
              file: await ipc.readFileAny(f.path),
            }))
          );

          const nextImages: LocalImage[] = [];
          const nextDocs: LocalDoc[] = [];

          for (const { f, file } of readResults) {
            if (isImageFileName(f.name)) {
              nextImages.push({
                id: nextId(),
                label: f.name,
                mime: file.mime ?? 'image/png',
                dataUrl: file.content,
              });
            } else if (file.kind === 'text') {
              nextDocs.push({ id: nextId(), label: f.name, content: file.content });
            } else {
              skippedNoTools.push(f.name);
            }
          }

          if (nextImages.length) {
            setImages((prev) => [...prev, ...nextImages]);
          }
          if (nextDocs.length) {
            setDocs((prev) => [...prev, ...nextDocs]);
          }
          if (skippedNoTools.length) {
            notices.push(
              `The active model doesn't have file tools — skipped: ${skippedNoTools.join(', ')}.`
            );
          }
          if (notices.length) {
            setNotice(notices.join(' '));
          }
          return;
        }

        if (notices.length) {
          setNotice(notices.join(' '));
        }
        if (!infos.length) return;

        setFiles((prev) => {
          const seen = new Set(prev.map((p) => p.path));
          return [...prev, ...infos.filter((f) => !seen.has(f.path))];
        });
      } catch (e) {
        setNotice(String(e));
      }
    },
    [toolsSupported, canAttachImages]
  );

  // OS file drops + clipboard-attach hotkey.
  useEffect(() => {
    const unDrop = onFileDrop((paths) => void addPaths(paths));
    const unClip = onClipboardAttach((e) => {
      if (e.kind === 'text') {
        const words = e.text.trim().split(/\s+/).length;
        setDocs((prev) => [
          ...prev,
          { id: nextId(), label: `Clipboard — ${words} words`, content: e.text },
        ]);
      } else if (canAttachImages) {
        setImages((prev) => [
          ...prev,
          { id: nextId(), label: 'Clipboard image', mime: e.mime, dataUrl: e.data_url },
        ]);
      } else {
        setNotice("The active model doesn't support images — the image wasn't attached.");
      }
    });
    return () => {
      void unDrop.then((fn) => fn());
      void unClip.then((fn) => fn());
    };
  }, [addPaths, canAttachImages]);

  const attachFromPicker = async () => {
    try {
      const paths = await pickFiles();
      if (paths.length) await addPaths(paths);
    } catch (e) {
      setNotice(String(e));
    }
  };

  const attachPastedFiles = async (pastedFiles: File[]) => {
    const skipped: string[] = [];
    for (const file of pastedFiles) {
      const kind = pastedFileKind(file);
      try {
        if (kind === 'image') {
          if (!canAttachImages) {
            skipped.push(file.name);
            continue;
          }
          const dataUrl = await readAsDataUrl(file);
          setImages((prev) => [
            ...prev,
            { id: nextId(), label: file.name || 'Clipboard image', mime: file.type, dataUrl },
          ]);
        } else if (kind === 'text') {
          const content = await file.text();
          setDocs((prev) => [...prev, { id: nextId(), label: file.name, content }]);
        } else {
          skipped.push(file.name);
        }
      } catch {
        skipped.push(file.name);
      }
    }
    if (skipped.length) {
      setNotice(
        `Couldn't attach ${skipped.join(', ')} from the clipboard — use the attach button.`
      );
    }
  };

  const hasContent =
    text.trim().length > 0 || files.length > 0 || docs.length > 0 || images.length > 0;

  const submit = async () => {
    if (!hasContent || degraded || bootSpinner || submittingRef.current) return;
    submittingRef.current = true;
    setSubmitting(true);
    setModelOpen(false);
    setEffortOpen(false);
    setNotice(null);

    try {
      const trimmed = text.trim();
      const providerId = activeProvider?.id ?? null;
      const effortVal = activeEffortOption?.value ?? null;

      // Read attached image files into base64 blocks; non-image files pass as paths.
      const imageFiles = files.filter((f) => !f.is_dir && isImageFileName(f.name));
      const imageSet = new Set(imageFiles);
      const otherFiles = files.filter((f) => !imageSet.has(f));

      const payloadImages: { mime: string; data_url: string }[] = images.map((img) => ({
        mime: img.mime,
        data_url: img.dataUrl,
      }));

      if (imageFiles.length > 0) {
        const failedNames: string[] = [];
        const loaded = await Promise.all(
          imageFiles.map(async (f) => {
            try {
              const read = await ipc.readFileAny(f.path);
              return { mime: read.mime ?? 'image/png', data_url: read.content };
            } catch {
              failedNames.push(f.name);
              return null;
            }
          })
        );
        if (failedNames.length > 0) {
          setNotice(
            `Couldn't read attached image${failedNames.length > 1 ? 's' : ''}: ${failedNames.join(', ')}. Remove or re-attach before sending.`
          );
          return;
        }
        for (const item of loaded) {
          if (item) payloadImages.push(item);
        }
      }

      const request: OverlayPromptRequest = {
        text: trimmed,
        ...(payloadImages.length ? { images: payloadImages } : {}),
        ...(otherFiles.length ? { attached_paths: otherFiles.map((f) => f.path) } : {}),
        ...(docs.length
          ? { documents: docs.map((d) => ({ label: d.label, content: d.content })) }
          : {}),
        provider_id: providerId,
        thinking_effort: effortVal,
      };

      await ipc.submitOverlayPrompt(request);

      setText('');
      setFiles([]);
      setDocs([]);
      setImages([]);
      resetTextareaHeight();
    } catch (e) {
      setNotice(String(e));
    } finally {
      submittingRef.current = false;
      setSubmitting(false);
    }
  };

  return (
    <div className="overlay-root">
      <div className="overlay-run-card" ref={cardRef}>
        {degraded && !bootSpinner ? (
          <div className="overlay-run-degraded">
            <span>Kitty&apos;s engine isn&apos;t running yet.</span>
            <div className="overlay-run-degraded-actions">
              <button
                type="button"
                className="primary"
                onClick={() => {
                  void ipc.openNewChatWindow();
                  void ipc.hideOverlay();
                }}
              >
                Open Kitty
              </button>
              <button type="button" onClick={() => void ipc.hideOverlay()}>
                Hide
              </button>
            </div>
          </div>
        ) : (
          <>
            <textarea
              ref={textareaRef}
              rows={1}
              autoFocus
              className="overlay-run-textarea"
              value={text}
              placeholder={bootSpinner ? 'Starting Kitty…' : 'Ask Kitty…'}
              onChange={(e) => {
                setText(e.target.value);
                scheduleTextareaResize();
              }}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.shiftKey) {
                  e.preventDefault();
                  void submit();
                }
              }}
              onPaste={(e) => {
                const pastedFiles = Array.from(e.clipboardData.files);
                if (pastedFiles.length > 0) {
                  e.preventDefault();
                  void attachPastedFiles(pastedFiles);
                  return;
                }
                const pasted = e.clipboardData.getData('text');
                if (pasted.length > PASTE_THRESHOLD) {
                  e.preventDefault();
                  const words = pasted.trim().split(/\s+/).length;
                  setDocs((prev) => [
                    ...prev,
                    { id: nextId(), label: `Pasted text — ${words} words`, content: pasted },
                  ]);
                }
              }}
            />

            {(files.length > 0 || docs.length > 0 || images.length > 0) && (
              <div className="file-chips overlay-run-attachments">
                {files.map((f) => (
                  <span className="chip" key={f.path} title={f.path}>
                    <span>{f.is_dir ? <FolderIcon /> : <DocumentIcon />}</span>
                    <span className="chip-name">{f.name}</span>
                    <button
                      type="button"
                      className="chip-x"
                      title="Remove"
                      onClick={() => setFiles((prev) => prev.filter((x) => x.path !== f.path))}
                    >
                      ×
                    </button>
                  </span>
                ))}
                {docs.map((d) => (
                  <span className="chip" key={d.id} title={d.label}>
                    <DocumentIcon />
                    <span className="chip-name">{d.label}</span>
                    <button
                      type="button"
                      className="chip-x"
                      title="Remove"
                      onClick={() => setDocs((prev) => prev.filter((x) => x.id !== d.id))}
                    >
                      ×
                    </button>
                  </span>
                ))}
                {images.map((img) => (
                  <span className="chip" key={img.id} title={img.label}>
                    <img className="chip-thumb" src={img.dataUrl} alt="" />
                    <span className="chip-name">{img.label}</span>
                    <button
                      type="button"
                      className="chip-x"
                      title="Remove"
                      onClick={() => setImages((prev) => prev.filter((x) => x.id !== img.id))}
                    >
                      ×
                    </button>
                  </span>
                ))}
              </div>
            )}

            {notice && (
              <div className="overlay-run-notice" role="status">
                <span>{notice}</span>
                <button type="button" className="link" onClick={() => setNotice(null)}>
                  Dismiss
                </button>
              </div>
            )}

            <div className="overlay-run-footer">
              <div className="overlay-run-controls">
                {/* 1. File attachment button */}
                <button
                  type="button"
                  className="overlay-run-attach"
                  onClick={() => void attachFromPicker()}
                  title="Attach files"
                  aria-label="Attach files"
                >
                  <UploadIcon />
                </button>

                {/* 2. Model pill */}
                <div className="overlay-run-control-wrap" ref={modelWrapRef}>
                  <button
                    type="button"
                    className="status-badge provider-badge"
                    title={
                      activeProvider
                        ? `${setupTitle(activeProvider)}\nClick to choose model`
                        : 'Choose model'
                    }
                    onClick={() => {
                      setEffortOpen(false);
                      setModelOpen((o) => !o);
                    }}
                  >
                    {activeProvider ? (
                      <TrustIcon
                        tier={activeProvider.network_tier}
                        isTrusted={activeProvider.is_trusted}
                      />
                    ) : (
                      <SettingsGearIcon />
                    )}
                    <span className="provider-badge-label">
                      {activeProvider ? activeProvider.name : 'No model'}
                    </span>
                    <span aria-hidden="true">▾</span>
                  </button>
                  {modelOpen && (
                    <div className="mode-popover provider-popover overlay-run-popover" role="menu">
                      {usableProviders.map((p) => (
                        <button
                          key={p.id}
                          type="button"
                          role="menuitemradio"
                          aria-checked={p.id === activeProvider?.id}
                          className={`provider-option${p.id === activeProvider?.id ? ' active' : ''}`}
                          title={setupTitle(p)}
                          onClick={() => {
                            setSelectedProviderId(p.id);
                            setSelectedEffortValue(null);
                            setModelOpen(false);
                          }}
                        >
                          <TrustIcon tier={p.network_tier} isTrusted={p.is_trusted} />
                          <span className="provider-option-name">{p.name}</span>
                        </button>
                      ))}
                      {usableProviders.length === 0 && <span className="muted">No models yet</span>}
                    </div>
                  )}
                </div>

                {/* 3. Reasoning effort pill */}
                {thinkingEffort && activeEffortOption && (
                  <div className="overlay-run-control-wrap" ref={effortWrapRef}>
                    <button
                      type="button"
                      className="status-badge effort-picker"
                      title={effortLabel(activeEffortOption, thinkingEffort.max_stars)}
                      aria-label={effortLabel(activeEffortOption, thinkingEffort.max_stars)}
                      aria-haspopup="menu"
                      aria-expanded={effortOpen}
                      onClick={() => {
                        setModelOpen(false);
                        setEffortOpen((o) => !o);
                      }}
                    >
                      <Sparks option={activeEffortOption} />
                    </button>
                    {effortOpen && (
                      <div
                        className="mode-popover effort-popover overlay-run-popover"
                        role="menu"
                        aria-label="Reasoning effort"
                      >
                        {thinkingEffort.options.map((o) => {
                          const checked = o.value === activeEffortOption.value;
                          return (
                            <button
                              key={o.value}
                              type="button"
                              role="menuitemradio"
                              aria-checked={checked}
                              aria-label={effortLabel(o, thinkingEffort.max_stars)}
                              title={effortLabel(o, thinkingEffort.max_stars)}
                              className={`effort-option${checked ? ' active' : ''}`}
                              onClick={() => {
                                setSelectedEffortValue(o.value);
                                setEffortOpen(false);
                              }}
                            >
                              <Sparks option={o} />
                            </button>
                          );
                        })}
                      </div>
                    )}
                  </div>
                )}
              </div>

              {/* Drag handle spacer */}
              <div className="overlay-run-spacer" data-tauri-drag-region />

              {/* 4. Send button */}
              <button
                type="button"
                className="primary overlay-run-send"
                disabled={!hasContent || bootSpinner || submitting}
                onClick={() => void submit()}
              >
                Send
              </button>
            </div>
          </>
        )}
      </div>
      <ContextMenu />
    </div>
  );
}
