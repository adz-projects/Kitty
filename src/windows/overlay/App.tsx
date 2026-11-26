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
  pickFolder,
} from '@/lib/ipc';
import { useStackStore, selectBooting } from '@/stores/stackStore';
import { TrustIcon } from '@/lib/provider_trust';
import { modelAcceptsImages } from '@/lib/vision_models';
import { pastedFileKind, readAsDataUrl } from '@/lib/pasteFiles';
import { effortLabel, selectedEffort } from '@/lib/effort';
import { completeSlashCommand, isExactSlashCommand, matchSlashCommands } from '@/lib/slashCommands';
import { DEFAULT_RESEARCH_TURNS, parseResearchCommand } from '@/lib/research';
import { DEFAULT_TIDY_TEXT, parseTidyCommand } from '@/lib/tidy';
import {
  isImageFileName,
  parseCritiqueCommand,
  parseDebateCommand,
} from '@/stores/chat/messageUtils';
import { setupTitle } from '@/components/settings/setups/setupUtils';
import { UploadIcon } from '@/components/icons/UploadIcon';
import { CameraIcon } from '@/components/icons/CameraIcon';
import { DocumentIcon } from '@/components/icons/DocumentIcon';
import { FolderIcon } from '@/components/icons/FolderIcon';
import { SettingsGearIcon } from '@/components/icons/SettingsGearIcon';
import { SparkIcon, SparkOffIcon } from '@/components/icons/SparkIcon';
import { ContextMenu, useContextMenuStore } from '@/components/shared/ContextMenu';
import { useEscapeLayer } from '@/hooks/useEscapeLayer';
import type {
  EffortOption,
  OverlayHitRect,
  OverlayPromptRequest,
  PathInfo,
  ProviderView,
  StackStatus,
  ThinkingEffort,
} from '@/lib/types';

const DEGRADED: StackStatus[] = ['backend_down'];
const PASTE_THRESHOLD = 500;
const MAX_TEXTAREA_HEIGHT = 160;
const MAX_RECENT_DIRS_SHOWN = 5;
const MAX_PROMPT_HISTORY = 40;

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

function normalizeDirKey(dir: string): string {
  return dir
    .trim()
    .replace(/[\\/]+$/, '')
    .replace(/\//g, '\\')
    .toLowerCase();
}

function dirLeafName(dir: string): string {
  return dir.split(/[\\/]/).filter(Boolean).pop() || dir;
}

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

/** Minimal Windows-"Run"-style overlay: prompt bar, attach button, working
    directory ("Thought Partner") pill, model pill, reasoning effort pill, and
    send button. Always starts a new chat in the background and closes
    immediately on Send. */
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
  const [selectedCwd, setSelectedCwd] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const [providers, setProviders] = useState<ProviderView[]>([]);
  const [selectedProviderId, setSelectedProviderId] = useState<string | null>(null);
  const [thinkingEffort, setThinkingEffort] = useState<ThinkingEffort | null>(null);
  const [selectedEffortValue, setSelectedEffortValue] = useState<string | null>(null);
  const [recentDirs, setRecentDirs] = useState<string[]>([]);
  const [promptHistory, setPromptHistory] = useState<string[]>([]);

  const [attachOpen, setAttachOpen] = useState(false);
  const [cwdOpen, setCwdOpen] = useState(false);
  const [modelOpen, setModelOpen] = useState(false);
  const [effortOpen, setEffortOpen] = useState(false);
  const [modelFilter, setModelFilter] = useState('');
  const [modelHighlight, setModelHighlight] = useState(0);
  const [slashIndex, setSlashIndex] = useState(0);
  const [slashDismissed, setSlashDismissed] = useState(false);
  const contextMenuOpen = useContextMenuStore((s) => s.menu !== null);

  const cardRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const attachWrapRef = useRef<HTMLDivElement>(null);
  const cwdWrapRef = useRef<HTMLDivElement>(null);
  const modelWrapRef = useRef<HTMLDivElement>(null);
  const effortWrapRef = useRef<HTMLDivElement>(null);
  const modelFilterRef = useRef<HTMLInputElement>(null);
  const resizeRaf = useRef<number | null>(null);
  const submittingRef = useRef(false);
  const lastHitKeyRef = useRef('');
  const historyIndexRef = useRef(-1);
  const historyDraftRef = useRef('');

  const closeAllMenus = useCallback(() => {
    setAttachOpen(false);
    setCwdOpen(false);
    setModelOpen(false);
    setEffortOpen(false);
  }, []);

  const menuOpen = attachOpen || cwdOpen || modelOpen || effortOpen;
  useEscapeLayer(menuOpen, () => {
    closeAllMenus();
    requestAnimationFrame(() => textareaRef.current?.focus());
  });

  const usableProviders = useMemo(() => providers.filter((p) => !p.disabled_reason), [providers]);

  const activeProvider = useMemo(() => {
    if (selectedProviderId) {
      const hit = usableProviders.find((p) => p.id === selectedProviderId);
      if (hit) return hit;
    }
    return usableProviders.find((p) => p.active) ?? usableProviders[0] ?? null;
  }, [usableProviders, selectedProviderId]);

  const filteredProviders = useMemo(() => {
    const q = modelFilter.trim().toLowerCase();
    if (!q) return usableProviders;
    return usableProviders.filter((p) => {
      const haystack = `${p.name} ${setupTitle(p)} ${p.models.join(' ')}`.toLowerCase();
      return haystack.includes(q);
    });
  }, [usableProviders, modelFilter]);

  const safeModelHighlight = Math.min(modelHighlight, Math.max(filteredProviders.length - 1, 0));

  const slashMatches = useMemo(() => {
    if (slashDismissed) return [];
    const matches = matchSlashCommands(text, false).filter((c) => c.name !== 'compact');
    if (matches.length === 1 && isExactSlashCommand(text)) return [];
    return matches;
  }, [slashDismissed, text]);
  const slashOpen = slashMatches.length > 0;
  const safeSlashIndex = Math.min(slashIndex, Math.max(slashMatches.length - 1, 0));
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

  const cwdLabel = selectedCwd
    ? selectedCwd.split(/[\\/]/).filter(Boolean).pop() || selectedCwd
    : null;

  const refreshProviders = useCallback(async () => {
    try {
      const list = await ipc.listProviders();
      setProviders(list);
    } catch {
      // keep existing list if backend is temporarily unreachable
    }
  }, []);

  const refreshConfig = useCallback(async () => {
    try {
      const cfg = await ipc.getConfig();
      setRecentDirs(cfg.recent_working_dirs ?? []);
      setPromptHistory(cfg.overlay_prompt_history ?? []);
    } catch {
      // keep existing config snapshot if temporarily unreachable
    }
  }, []);

  const selectProvider = useCallback((providerId: string) => {
    setSelectedProviderId(providerId);
    setSelectedEffortValue(null);
    setModelOpen(false);
    requestAnimationFrame(() => textareaRef.current?.focus());
  }, []);

  const openModelMenu = useCallback(() => {
    setAttachOpen(false);
    setCwdOpen(false);
    setEffortOpen(false);
    setModelFilter('');
    const curIdx = usableProviders.findIndex((p) => p.id === activeProvider?.id);
    setModelHighlight(curIdx >= 0 ? curIdx : 0);
    setModelOpen(true);
    requestAnimationFrame(() => modelFilterRef.current?.focus());
  }, [usableProviders, activeProvider]);

  const cycleEffort = useCallback(() => {
    if (!thinkingEffort || thinkingEffort.options.length < 2) return;
    const opts = thinkingEffort.options;
    const curIdx = opts.findIndex((o) => o.value === activeEffortOption?.value);
    const next = opts[(curIdx + 1) % opts.length];
    if (next) setSelectedEffortValue(next.value);
  }, [thinkingEffort, activeEffortOption]);

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

  const syncHitRects = useCallback(() => {
    const els = document.querySelectorAll<HTMLElement>(
      '.overlay-run-card, .overlay-run-popover, .overlay-run-slash-menu, .context-menu'
    );
    const rects: OverlayHitRect[] = [];
    for (const el of els) {
      const r = el.getBoundingClientRect();
      if (r.width > 0 && r.height > 0) {
        rects.push({
          x: Math.floor(r.left),
          y: Math.floor(r.top),
          width: Math.ceil(r.width),
          height: Math.ceil(r.height),
        });
      }
    }
    const key = JSON.stringify(rects);
    if (key === lastHitKeyRef.current) return;
    lastHitKeyRef.current = key;
    void ipc.setOverlayHitRects(rects).catch(() => {});
  }, []);

  useEffect(() => {
    void initStack();
    void refreshProviders();
    void refreshConfig();

    const focusInput = () => {
      requestAnimationFrame(() => textareaRef.current?.focus());
    };
    const onShown = () => {
      lastHitKeyRef.current = '';
      syncHitRects();
      focusInput();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !e.defaultPrevented) {
        void ipc.hideOverlay();
      }
    };
    window.addEventListener('keydown', onKey);
    window.addEventListener('focus', focusInput);

    const un = [
      onOverlayShown(onShown),
      onOverlaySubmitFailed(({ text: failedText, error }) => {
        setText((cur) => (cur.trim().length === 0 ? failedText : cur));
        scheduleTextareaResize();
        setNotice(error);
      }),
      onConfigChanged(() => {
        void refreshProviders();
        void refreshConfig();
      }),
      onProviderActivated(() => void refreshProviders()),
    ];
    return () => {
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('focus', focusInput);
      un.forEach((u) => void u.then((fn) => fn()));
    };
  }, [initStack, refreshConfig, refreshProviders, scheduleTextareaResize, syncHitRects]);

  useLayoutEffect(() => {
    syncHitRects();
    const raf = requestAnimationFrame(syncHitRects);
    const card = cardRef.current;
    if (!card || typeof ResizeObserver === 'undefined') {
      return () => cancelAnimationFrame(raf);
    }
    const ro = new ResizeObserver(() => syncHitRects());
    ro.observe(card);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  }, [
    syncHitRects,
    menuOpen,
    slashOpen,
    slashMatches.length,
    modelFilter,
    recentDirs.length,
    canAttachImages,
    contextMenuOpen,
    usableProviders.length,
    thinkingEffort,
    degraded,
    bootSpinner,
    notice,
    files.length,
    docs.length,
    images.length,
  ]);

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

  // Close popovers on click/tap outside or when the overlay loses window focus.
  useEffect(() => {
    if (!menuOpen) return;
    const onPointerDown = (e: PointerEvent) => {
      const target = e.target as Node;
      if (
        attachWrapRef.current?.contains(target) ||
        cwdWrapRef.current?.contains(target) ||
        modelWrapRef.current?.contains(target) ||
        effortWrapRef.current?.contains(target)
      ) {
        return;
      }
      closeAllMenus();
    };
    const onBlur = () => closeAllMenus();
    document.addEventListener('pointerdown', onPointerDown);
    window.addEventListener('blur', onBlur);
    return () => {
      document.removeEventListener('pointerdown', onPointerDown);
      window.removeEventListener('blur', onBlur);
    };
  }, [closeAllMenus, menuOpen]);

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
    } finally {
      requestAnimationFrame(() => textareaRef.current?.focus());
    }
  };

  const captureScreenSnip = async () => {
    closeAllMenus();
    if (!canAttachImages) {
      setNotice("The active model doesn't support images — switch to a vision model for snips.");
      return;
    }
    try {
      const shot = await ipc.captureScreenshotRegion();
      if (shot) {
        setImages((prev) => [
          ...prev,
          { id: nextId(), label: 'Screen snip', mime: shot.mime, dataUrl: shot.data_url },
        ]);
      }
    } catch (e) {
      setNotice(String(e));
    } finally {
      requestAnimationFrame(() => textareaRef.current?.focus());
    }
  };

  const chooseWorkingFolder = async () => {
    closeAllMenus();
    try {
      const dir = await pickFolder();
      if (dir) setSelectedCwd(dir);
    } catch (e) {
      setNotice(String(e));
    } finally {
      requestAnimationFrame(() => textareaRef.current?.focus());
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

  const trimmedText = text.trim();
  const parsedDebate = !slashOpen ? parseDebateCommand(trimmedText) : null;
  const parsedCritique = !slashOpen ? parseCritiqueCommand(trimmedText) : null;
  const parsedResearch = !slashOpen ? parseResearchCommand(trimmedText) : null;
  const parsedTidy = !slashOpen ? parseTidyCommand(trimmedText) : null;

  const hasContent =
    parsedResearch !== null
      ? Boolean(parsedResearch.brief)
      : parsedDebate !== null ||
        parsedCritique !== null ||
        parsedTidy !== null ||
        trimmedText.length > 0 ||
        files.length > 0 ||
        docs.length > 0 ||
        images.length > 0;

  const submit = async () => {
    if (!hasContent || degraded || bootSpinner || submittingRef.current) return;
    submittingRef.current = true;
    setSubmitting(true);
    closeAllMenus();
    setNotice(null);

    try {
      const trimmed = text.trim();
      const debate = parseDebateCommand(trimmed);
      const critique = parseCritiqueCommand(trimmed);
      const research = parseResearchCommand(trimmed);
      const tidy = parseTidyCommand(trimmed);

      if (research !== null && !research.brief) return;

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

      const sendText =
        tidy !== null
          ? tidy || DEFAULT_TIDY_TEXT
          : research !== null
            ? research.brief
            : critique !== null
              ? critique
              : debate !== null
                ? debate
                : trimmed;

      const request: OverlayPromptRequest = {
        text: sendText,
        ...(trimmed ? { raw_prompt: trimmed } : {}),
        ...(payloadImages.length ? { images: payloadImages } : {}),
        ...(otherFiles.length ? { attached_paths: otherFiles.map((f) => f.path) } : {}),
        ...(docs.length
          ? { documents: docs.map((d) => ({ label: d.label, content: d.content })) }
          : {}),
        ...(debate !== null ? { debate: true } : {}),
        ...(research !== null ? { research: { max_turns: research.maxTurns } } : {}),
        ...(critique !== null ? { critique: 'start' as const } : {}),
        ...(tidy !== null ? { tidy: true } : {}),
        provider_id: providerId,
        thinking_effort: effortVal,
        ...(selectedCwd ? { cwd: selectedCwd } : {}),
      };

      await ipc.submitOverlayPrompt(request);

      if (trimmed) {
        setPromptHistory((prev) =>
          [trimmed, ...prev.filter((item) => item !== trimmed)].slice(0, MAX_PROMPT_HISTORY)
        );
      }
      if (selectedCwd) {
        const cwdKey = normalizeDirKey(selectedCwd);
        setRecentDirs((prev) =>
          [selectedCwd, ...prev.filter((d) => normalizeDirKey(d) !== cwdKey)].slice(0, 8)
        );
      }

      historyIndexRef.current = -1;
      historyDraftRef.current = '';
      setSlashDismissed(false);
      setSlashIndex(0);
      setText('');
      setFiles([]);
      setDocs([]);
      setImages([]);
      setSelectedCwd(null);
      resetTextareaHeight();
    } catch (e) {
      setNotice(String(e));
    } finally {
      submittingRef.current = false;
      setSubmitting(false);
    }
  };

  return (
    <div
      className="overlay-root"
      onPointerDown={(e) => {
        if (e.target === e.currentTarget && menuOpen) {
          closeAllMenus();
        }
      }}
      onKeyDown={(e) => {
        if (degraded || bootSpinner) return;
        const mod = e.ctrlKey || e.metaKey;
        if (!mod) return;
        const k = e.key.toLowerCase();
        if (e.shiftKey && !e.altKey && k === 's') {
          e.preventDefault();
          void captureScreenSnip();
          return;
        }
        if (e.shiftKey || e.altKey) return;
        if (k === 'm') {
          e.preventDefault();
          if (modelOpen) {
            setModelOpen(false);
            requestAnimationFrame(() => textareaRef.current?.focus());
          } else {
            openModelMenu();
          }
        } else if (k === 'd') {
          e.preventDefault();
          setAttachOpen(false);
          setModelOpen(false);
          setEffortOpen(false);
          setCwdOpen((prev) => {
            if (prev) requestAnimationFrame(() => textareaRef.current?.focus());
            return !prev;
          });
        } else if (k === 'e') {
          e.preventDefault();
          cycleEffort();
        }
      }}
    >
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
            {slashOpen && (
              <ul
                className="slash-menu overlay-run-slash-menu"
                role="listbox"
                aria-label="Slash commands"
              >
                {slashMatches.map((cmd, idx) => (
                  <li
                    key={cmd.name}
                    role="option"
                    aria-selected={idx === safeSlashIndex}
                    className={`slash-item${idx === safeSlashIndex ? ' active' : ''}`}
                    onMouseDown={(e) => {
                      e.preventDefault();
                      setText(completeSlashCommand(cmd));
                      setSlashIndex(0);
                      scheduleTextareaResize();
                      textareaRef.current?.focus();
                    }}
                  >
                    <span className="slash-name">/{cmd.name}</span>
                    {cmd.args && <span className="slash-args muted">{cmd.args}</span>}
                    <span className="slash-desc muted">{cmd.description}</span>
                  </li>
                ))}
              </ul>
            )}
            {parsedDebate !== null && (
              <div className="composer-hint overlay-run-hint" role="status">
                Debate: say who the personas are, how many rounds, and what the debate should
                examine. Attach or link the item under debate.
              </div>
            )}
            {parsedCritique !== null && (
              <div className="composer-hint overlay-run-hint" role="status">
                Critique: attach or paste the draft. Options: genre=op-ed, essay, academic, memo,
                proposal or other; stage=early, revising or polish; factcheck; sketches=off.
              </div>
            )}
            {parsedResearch !== null && (
              <div className="composer-hint overlay-run-hint" role="status">
                Deep research: say what to research and what to produce. Add turns=N to set the
                budget (default {DEFAULT_RESEARCH_TURNS}). You approve the plan before it starts.
              </div>
            )}
            {parsedTidy !== null && (
              <div className="composer-hint overlay-run-hint" role="status">
                Tidy: Kitty grades your chats and proposes which to delete. Add what to keep or
                clear if you like.
              </div>
            )}
            <textarea
              ref={textareaRef}
              rows={1}
              autoFocus
              className="overlay-run-textarea"
              value={text}
              placeholder={bootSpinner ? 'Starting Kitty…' : 'Ask Kitty…'}
              onChange={(e) => {
                historyIndexRef.current = -1;
                setSlashDismissed(false);
                setSlashIndex(0);
                setText(e.target.value);
                scheduleTextareaResize();
              }}
              onKeyDown={(e) => {
                if (slashOpen) {
                  if (e.key === 'ArrowDown') {
                    e.preventDefault();
                    setSlashIndex((i) => (i + 1) % slashMatches.length);
                    return;
                  }
                  if (e.key === 'ArrowUp') {
                    e.preventDefault();
                    setSlashIndex((i) => (i - 1 + slashMatches.length) % slashMatches.length);
                    return;
                  }
                  if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
                    const cmd = slashMatches[safeSlashIndex];
                    if (cmd) {
                      e.preventDefault();
                      setText(completeSlashCommand(cmd));
                      setSlashIndex(0);
                      scheduleTextareaResize();
                    }
                    return;
                  }
                  if (e.key === 'Escape') {
                    e.preventDefault();
                    e.stopPropagation();
                    setSlashDismissed(true);
                    return;
                  }
                }

                if (e.key === 'Tab' && e.shiftKey && !menuOpen) {
                  e.preventDefault();
                  cycleEffort();
                  return;
                }

                if (
                  e.key === 'ArrowUp' &&
                  !e.ctrlKey &&
                  !e.metaKey &&
                  !e.altKey &&
                  !e.shiftKey &&
                  promptHistory.length > 0
                ) {
                  const el = e.currentTarget;
                  const atEmptyOrStart =
                    el.value.length === 0 || (el.selectionStart === 0 && el.selectionEnd === 0);
                  const onFirstLine = !el.value.slice(0, el.selectionStart).includes('\n');
                  if (
                    (historyIndexRef.current === -1 && atEmptyOrStart) ||
                    (historyIndexRef.current >= 0 && onFirstLine)
                  ) {
                    e.preventDefault();
                    if (historyIndexRef.current === -1) {
                      historyDraftRef.current = text;
                      historyIndexRef.current = 0;
                    } else {
                      historyIndexRef.current = Math.min(
                        historyIndexRef.current + 1,
                        promptHistory.length - 1
                      );
                    }
                    const recalled = promptHistory[historyIndexRef.current] ?? '';
                    setText(recalled);
                    setSlashDismissed(true);
                    scheduleTextareaResize();
                    requestAnimationFrame(() => {
                      const area = textareaRef.current;
                      if (area) area.setSelectionRange(recalled.length, recalled.length);
                    });
                    return;
                  }
                }

                if (
                  e.key === 'ArrowDown' &&
                  !e.ctrlKey &&
                  !e.metaKey &&
                  !e.altKey &&
                  !e.shiftKey &&
                  historyIndexRef.current >= 0
                ) {
                  const el = e.currentTarget;
                  const onLastLine = !el.value.slice(el.selectionEnd).includes('\n');
                  if (onLastLine) {
                    e.preventDefault();
                    const nextIdx = historyIndexRef.current - 1;
                    historyIndexRef.current = nextIdx;
                    const restored =
                      nextIdx >= 0 ? (promptHistory[nextIdx] ?? '') : historyDraftRef.current;
                    setText(restored);
                    setSlashDismissed(true);
                    scheduleTextareaResize();
                    requestAnimationFrame(() => {
                      const area = textareaRef.current;
                      if (area) area.setSelectionRange(restored.length, restored.length);
                    });
                    return;
                  }
                }

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
                {files.map((f) => {
                  const isCurrentCwd =
                    f.is_dir &&
                    selectedCwd !== null &&
                    normalizeDirKey(f.path) === normalizeDirKey(selectedCwd);
                  return (
                    <span className="chip" key={f.path} title={f.path}>
                      <span>{f.is_dir ? <FolderIcon /> : <DocumentIcon />}</span>
                      <span className="chip-name">{f.name}</span>
                      {f.is_dir && !isCurrentCwd && (
                        <button
                          type="button"
                          className="chip-action"
                          title="Use this folder as the chat working directory"
                          onClick={() => setSelectedCwd(f.path)}
                        >
                          Set cwd
                        </button>
                      )}
                      <button
                        type="button"
                        className="chip-x"
                        title="Remove"
                        onClick={() => setFiles((prev) => prev.filter((x) => x.path !== f.path))}
                      >
                        ×
                      </button>
                    </span>
                  );
                })}
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
                {/* 1. File / screen-snip attachment button */}
                <div className="overlay-run-control-wrap" ref={attachWrapRef}>
                  <button
                    type="button"
                    className="overlay-run-attach"
                    onClick={() => {
                      if (!canAttachImages) {
                        closeAllMenus();
                        void attachFromPicker();
                        return;
                      }
                      setCwdOpen(false);
                      setModelOpen(false);
                      setEffortOpen(false);
                      setAttachOpen((o) => !o);
                    }}
                    title={canAttachImages ? 'Attach files or capture screen snip' : 'Attach files'}
                    aria-label={
                      canAttachImages ? 'Attach files or capture screen snip' : 'Attach files'
                    }
                    aria-haspopup={canAttachImages ? 'menu' : undefined}
                    aria-expanded={canAttachImages ? attachOpen : undefined}
                  >
                    <UploadIcon />
                  </button>
                  {attachOpen && canAttachImages && (
                    <div
                      className="mode-popover attach-popover overlay-run-popover"
                      role="menu"
                      aria-label="Attach"
                    >
                      <button
                        type="button"
                        role="menuitem"
                        onClick={() => {
                          closeAllMenus();
                          void attachFromPicker();
                        }}
                      >
                        <span className="overlay-popover-row">
                          <span className="overlay-popover-row-main">
                            <UploadIcon />
                            <span>Attach files…</span>
                          </span>
                        </span>
                      </button>
                      <button
                        type="button"
                        role="menuitem"
                        onClick={() => void captureScreenSnip()}
                      >
                        <span className="overlay-popover-row">
                          <span className="overlay-popover-row-main">
                            <CameraIcon />
                            <span>Capture screen region</span>
                          </span>
                          <span className="overlay-menu-shortcut">Ctrl+Shift+S</span>
                        </span>
                      </button>
                    </div>
                  )}
                </div>

                {/* 2. Working directory ("Thought Partner") pill */}
                <div className="overlay-run-control-wrap" ref={cwdWrapRef}>
                  {selectedCwd ? (
                    <span className="status-badge overlay-run-cwd overlay-run-cwd-active">
                      <button
                        type="button"
                        className="overlay-run-cwd-body"
                        title={`Working directory: ${selectedCwd} — click to change (Ctrl+D)`}
                        aria-haspopup="menu"
                        aria-expanded={cwdOpen}
                        onClick={() => {
                          setAttachOpen(false);
                          setModelOpen(false);
                          setEffortOpen(false);
                          setCwdOpen((o) => !o);
                        }}
                      >
                        <FolderIcon />
                        <span className="overlay-run-cwd-label">{cwdLabel}</span>
                      </button>
                      <button
                        type="button"
                        className="overlay-run-cwd-reset"
                        title="Return to thought partner (clear the working folder)"
                        aria-label="Return to thought partner"
                        onClick={() => {
                          setCwdOpen(false);
                          setSelectedCwd(null);
                          requestAnimationFrame(() => textareaRef.current?.focus());
                        }}
                      >
                        ×
                      </button>
                    </span>
                  ) : (
                    <button
                      type="button"
                      className="status-badge overlay-run-cwd"
                      title="Thinking space — click to set a working directory (Ctrl+D)"
                      aria-haspopup="menu"
                      aria-expanded={cwdOpen}
                      onClick={() => {
                        setAttachOpen(false);
                        setModelOpen(false);
                        setEffortOpen(false);
                        setCwdOpen((o) => !o);
                      }}
                    >
                      <span className="overlay-run-cwd-label">Thought Partner</span>
                    </button>
                  )}
                  {cwdOpen && (
                    <div
                      className="mode-popover cwd-popover overlay-run-popover"
                      role="menu"
                      aria-label="Working directory"
                    >
                      <button
                        type="button"
                        role="menuitemradio"
                        aria-checked={!selectedCwd}
                        className={!selectedCwd ? 'active' : undefined}
                        onClick={() => {
                          setSelectedCwd(null);
                          setCwdOpen(false);
                          requestAnimationFrame(() => textareaRef.current?.focus());
                        }}
                      >
                        <span className="overlay-popover-row">
                          <span>Thought Partner (default)</span>
                        </span>
                      </button>
                      {recentDirs.slice(0, MAX_RECENT_DIRS_SHOWN).map((dir) => {
                        const checked =
                          selectedCwd !== null &&
                          normalizeDirKey(dir) === normalizeDirKey(selectedCwd);
                        return (
                          <button
                            key={dir}
                            type="button"
                            role="menuitemradio"
                            aria-checked={checked}
                            title={dir}
                            className={checked ? 'active' : undefined}
                            onClick={() => {
                              setSelectedCwd(dir);
                              setCwdOpen(false);
                              requestAnimationFrame(() => textareaRef.current?.focus());
                            }}
                          >
                            <span className="overlay-cwd-item">
                              <span className="overlay-cwd-name">
                                <FolderIcon />
                                <span>{dirLeafName(dir)}</span>
                              </span>
                              <span className="overlay-cwd-path">{dir}</span>
                            </span>
                          </button>
                        );
                      })}
                      <hr className="mode-popover-sep" />
                      <button
                        type="button"
                        role="menuitem"
                        onClick={() => void chooseWorkingFolder()}
                      >
                        Choose another folder…
                      </button>
                    </div>
                  )}
                </div>

                {/* 3. Model pill */}
                <div className="overlay-run-control-wrap" ref={modelWrapRef}>
                  <button
                    type="button"
                    className="status-badge provider-badge"
                    title={
                      activeProvider
                        ? `${setupTitle(activeProvider)}\nClick to choose model (Ctrl+M)`
                        : 'Choose model (Ctrl+M)'
                    }
                    onClick={() => {
                      if (modelOpen) {
                        setModelOpen(false);
                      } else {
                        openModelMenu();
                      }
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
                      <input
                        ref={modelFilterRef}
                        type="text"
                        className="overlay-model-filter"
                        placeholder="Filter models…"
                        aria-label="Filter models"
                        value={modelFilter}
                        onChange={(e) => {
                          setModelFilter(e.target.value);
                          setModelHighlight(0);
                        }}
                        onKeyDown={(e) => {
                          if (e.key === 'ArrowDown') {
                            e.preventDefault();
                            setModelHighlight((i) =>
                              filteredProviders.length
                                ? Math.min(i + 1, filteredProviders.length - 1)
                                : 0
                            );
                          } else if (e.key === 'ArrowUp') {
                            e.preventDefault();
                            setModelHighlight((i) => Math.max(i - 1, 0));
                          } else if (e.key === 'Enter') {
                            const pick = filteredProviders[safeModelHighlight];
                            if (pick) {
                              e.preventDefault();
                              selectProvider(pick.id);
                            }
                          }
                        }}
                      />
                      {filteredProviders.map((p, idx) => (
                        <button
                          key={p.id}
                          type="button"
                          role="menuitemradio"
                          aria-checked={p.id === activeProvider?.id}
                          className={`provider-option${
                            p.id === activeProvider?.id ? ' active' : ''
                          }${idx === safeModelHighlight ? ' highlighted' : ''}`}
                          title={setupTitle(p)}
                          onMouseEnter={() => setModelHighlight(idx)}
                          onClick={() => selectProvider(p.id)}
                        >
                          <TrustIcon tier={p.network_tier} isTrusted={p.is_trusted} />
                          <span className="provider-option-name">{p.name}</span>
                        </button>
                      ))}
                      {filteredProviders.length === 0 && (
                        <span className="overlay-model-empty">
                          {usableProviders.length === 0 ? 'No models yet' : 'No matching models'}
                        </span>
                      )}
                    </div>
                  )}
                </div>

                {/* 4. Reasoning effort pill */}
                {thinkingEffort && activeEffortOption && (
                  <div className="overlay-run-control-wrap" ref={effortWrapRef}>
                    <button
                      type="button"
                      className="status-badge effort-picker"
                      title={`${effortLabel(activeEffortOption, thinkingEffort.max_stars)} (Shift+Tab or Ctrl+E)`}
                      aria-label={effortLabel(activeEffortOption, thinkingEffort.max_stars)}
                      aria-haspopup="menu"
                      aria-expanded={effortOpen}
                      onClick={() => {
                        setAttachOpen(false);
                        setCwdOpen(false);
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
                                requestAnimationFrame(() => textareaRef.current?.focus());
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

              {/* 5. Send button */}
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
