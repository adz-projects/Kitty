# Goose Overlay — Project Description

## 1. Summary

A custom, lightweight desktop GUI for [Goose](https://github.com/block/goose) that replaces or supplements the stock Goose Desktop app. It runs as a hidden background app, summoned instantly via a global hotkey as a floating overlay window (Spotlight/Raycast-style), and doubles as both a general chat interface and an agentic tool-calling front end. It can also expand into a full window when the overlay is too small for the task at hand. Configuration lives in a system tray menu, not in the overlay itself.

The app is a **client only** — it does not reimplement any agent logic, tool execution, or LLM orchestration. All of that is delegated to Goose's own background server (`goosed`), which already handles sessions, tool calling, MCP extensions, and multi-provider model access. This project's job is purely the interaction layer: instant summon, look and feel, configuration, and quality-of-life features like file drag-and-drop, session history, and artifact tracking — plus getting a new user from "nothing installed" to "ready to chat" with minimal friction.

**Scope note:** this is a single-platform project for now (Windows, given the Copilot-key hook), not a cross-platform effort. Cross-platform support can be revisited later.

## 2. Motivation

- The existing Goose Desktop app (Electron) is a full window you have to switch to; this should instead behave like a spotlight-style overlay that appears instantly over whatever you're doing.
- Many laptops now have a dedicated "Copilot" key that's otherwise wasted on a Microsoft-specific assistant. Reclaiming it for a local, Ollama-backed agent is a natural use case.
- Ollama gives full local/private inference, but there are times a more capable hosted model (via OpenRouter or similar) is worth using — so the tool should support both, with privacy trade-offs made explicit.
- Setting up Ollama + Goose + a first model today requires several manual steps across separate tools; a single guided installer should collapse that into one flow.

## 3. Installer & First-Run Setup

A guided setup flow gets a new user to "ready to chat" without them having to separately discover, install, and configure Ollama and Goose themselves.

- **Dependency detection**: on first launch (or via an installer run before first launch), check whether Ollama and Goose (`goosed`/CLI) are already installed.
- **Install what's missing**: if either is missing, download and run their official installers (or bundle/invoke them silently where possible) rather than just linking out and telling the user to do it manually. Requiring administrator elevation for this step is acceptable.
- **Guided configuration**: after both are present, walk the user through:
  - Confirming/setting the Ollama endpoint (default `localhost:11434`)
  - Choosing a default provider for Goose (local Ollama as the recommended default)
  - Setting the default filesystem context folder (see Section 7)
  - Setting the hotkey (defaulting to the Copilot key if detected)
- **First model setup**: help the user pick and pull a starter model from a short curated list of models **4B parameters and under**, so the recommendation works on essentially any hardware. Show pull progress. Users who want something bigger can browse the Ollama library from settings afterward.
- This flow reuses the same underlying mechanisms as the regular settings panel (provider config, Ollama pull-with-progress) — it's a first-run wizard wrapping those, not a separate implementation.
- Re-running this setup later (e.g. to add Goose/Ollama on a fresh reinstall) should be possible from the tray/settings, not just on first launch.

## 4. Process & Lifecycle Management

- **The front end owns the stack**: Ollama and `goosed` are started when this app launches and stopped when it quits. The user should never have to manually start either service for the overlay to work.
- **Goose Desktop conflict warning**: if the stock Goose Desktop app is detected running alongside this front end, show a warning — both clients managing the same config and sessions (and potentially competing goosed instances/ports) can produce confusing behavior. The warning explains the situation; it doesn't block the user.
- **Degraded-state handling & repair**: if something in the stack is broken — Ollama not responding, no model pulled, goosed crashed, provider misconfigured — the overlay shows a clear, plain-language status instead of a dead chat box, with a one-click path into a repair workflow. The repair workflow opens the preferences panel with the relevant section highlighted (e.g. jumping straight to provider/model configuration if the active provider is unreachable), and can restart Ollama/goosed or re-run parts of first-run setup as needed.

## 5. Core Interaction Model

- **Hotkey summon**: A global hotkey (default: the hardware Copilot key, otherwise user-configurable) toggles a floating overlay window. Pressing again, or hitting Escape, hides it.
- **Floating overlay window**: Borderless, transparent-capable, always-on-top, not shown in the taskbar. Behaves like a command palette — appears near-instantly, takes focus, disappears cleanly when dismissed. Intended for quick chats and light tool use.
- **Expand to full window**: The overlay can be "popped out" into a normal, resizable window for cases where the compact overlay isn't enough — e.g. reviewing long tool output, browsing session history, or working with the artifacts sidepane. The same session continues; it's a change of chrome, not a new conversation.
- **System tray**: A persistent tray icon is the only other visible presence. Left-click could optionally toggle the overlay too; right-click (or click) opens a menu with at least "Open Settings," "Quit," and maybe "New Session."
- **Settings/configuration panel**: A separate, normal (non-overlay) window opened from the tray. This is where all persistent configuration lives, including re-running first-run setup.

## 6. Chat, Tool Calling & Approvals

The overlay (and full window) is a single interface that handles both:
- **General chat** — open-ended conversation with the configured model.
- **Agentic tool use** — the same conversation can trigger Goose's tools (filesystem access, shell commands, MCP extensions, etc.).

Under the hood, the app talks to the local `goosed` server (REST + streaming API) rather than calling Ollama or any model provider directly. `goosed` already owns model routing, tool execution, and MCP extension management, so the app just sends messages and renders streamed responses, tool call status, and results.

### 6.1 Tool approval modes
- The app surfaces Goose's approval modes — **auto**, **smart-approve**, and **manual** — as a first-class, easily visible setting (including an indicator of the current mode in the chat UI, not buried in settings).
- When a tool call requires approval, the approval prompt shows what tool is being invoked and with what parameters (e.g. the exact shell command), with approve/deny controls inline in the chat.
- **Approvals while hidden**: if the overlay is dismissed when an approval is needed, the app raises a notification (see 6.2); clicking it re-summons the overlay directly to the pending approval. Tool execution never proceeds silently past a required approval just because the window wasn't visible.

### 6.2 Notifications
- Since the core interaction model is "dismiss the window and get on with your life," the app uses native system notifications (and a tray-icon state change) for events that happen while the overlay is hidden:
  - A long-running task completed
  - A tool call is waiting for approval
  - A task failed or the stack entered a degraded state (see Section 4)
- Clicking a notification summons the overlay to the relevant session/prompt.
- Notification behavior (on/off, which events) is configurable in settings.

## 7. Filesystem Context

- **Default context folder**: Settings lets the user set a default working directory for new sessions (e.g. `Documents/Goose`), so the agent always has a sensible default scope instead of an ambiguous or overly broad one. This is also set as part of first-run setup.
- **Drag-and-drop file/folder references**: Users can drag files and folders from the OS file browser directly onto the window. Dropped items appear as removable "chips" in the composer (showing name/icon, file vs. folder). On send, the referenced paths are included as context so Goose's filesystem/developer tools can act on them directly.
- **Setting working directory from a drop**: Dropping a folder offers an option to set it as the active session's working directory, overriding the default for that session.
- **Context indicator**: The chat UI always shows what filesystem context it's currently operating in (e.g. a small breadcrumb/pill showing the active working directory), so it's never ambiguous what scope the agent has access to. This updates whenever the context changes via drop, session resume, or default settings.

## 8. Artifacts Sidepane

- A side panel (available in full-window mode, and collapsible/expandable in the overlay if screen space allows) lists everything the agent has created or produced during the session — generated files, code snippets, documents, images, etc.
- Each entry links back to the underlying file/tool-output so the user can open, save, or reference it without having to scroll back through chat history to find it.
- This is a view over what Goose's tools already produced on disk/in the session — not a separate artifact-generation system.

## 9. Session History

- The app includes a history view (in the full window, and reachable from the overlay) listing past sessions — not just the most recent ones, but full searchable history.
- Each entry shows a description/title, working directory, last-updated time, and which model/provider was used.
- Selecting a session resumes it, restoring full conversation history, working directory context, and artifact list — letting the user pick back up on a prior project rather than always starting fresh.
- This is implemented entirely against Goose's existing session management (sessions are already persisted server-side); the app does not maintain its own separate history store.

## 10. Configuration / Preferences

The settings window is organized into a few areas:

### 10.1 Goose settings (mirrors what Goose Desktop exposes)
- Active provider & model selection
- Extensions/MCP servers: enable/disable, add custom (stdio or HTTP-based)
- Tool approval mode (auto / smart-approve / manual) and per-tool permission levels
- Session behavior: auto-summarize threshold, recipe repository location
- Default filesystem context folder (see Section 7)
- Notification preferences (see Section 6.2)
- General: telemetry, auto-update toggles
- Re-run first-run setup / repair (see Sections 3 and 4)

### 10.2 Multiple model provider endpoints
- Support for configuring more than one provider/endpoint, not just a single default:
  - Local Ollama (default, no key required)
  - Built-in hosted providers Goose already supports (OpenRouter, Anthropic, OpenAI, Databricks, etc.)
  - Custom OpenAI-compatible endpoints (base URL + API key + model list) for other gateways
- Users can maintain multiple named provider profiles and switch between them per session.
- **Privacy handling**: any provider whose endpoint isn't local (`localhost`/`127.0.0.1`/private LAN) is flagged as "Remote" in the UI. Adding a remote provider triggers an explicit warning explaining that prompts, file contents, and tool outputs may be sent to a third party.
- **Context handoff on provider switch**: when a session switches from a local to a remote provider, the user must make an **active choice, every time**, about whether to keep the session's existing context (conversation history, file references, tool outputs) — which will now be sent to the remote endpoint — or jettison it and continue with a clean slate. There is deliberately no "remember my choice" option for this decision.
- **Credential security**: since provider profiles include API keys, the stored profile data (at minimum, the key/secret fields) must be encrypted at rest rather than kept in a plain config file — e.g. via the OS credential store (Windows Credential Manager) or an encrypted local store, not plaintext JSON/YAML.

### 10.3 Ollama model management
- View locally installed Ollama models (name, size, last modified).
- Pull new models with a live progress indicator (download layers/bytes).
- Delete installed models.
- Browsing/searching the model catalog itself is **not** built into the app — a "Browse models" action simply opens the Ollama model library website in the user's default browser.

### 10.4 Advanced (collapsed by default)
An "Advanced" section, collapsed by default so it doesn't clutter the main settings for typical users, containing:
- **Ollama environment variable helper**: a simple UI for viewing/setting the Ollama-relevant environment variables that affect its behavior (e.g. host, model storage path, parallelism), rather than requiring the user to hand-edit system environment variables and restart Ollama themselves.
- **Model parameters**: sampling and inference preferences such as temperature, top_p, context length (num_ctx), and planner provider/model for multi-model setups.

### 10.5 Look & feel
- Theming is CSS-first and kept as simple as possible: built-in themes are plain CSS files/variables, and advanced users can drop in their own CSS.
- One explicit exception: users can set a custom background image for the overlay/window, since that's a common and low-complexity customization that pure CSS variables don't elegantly cover on their own.
- Configurable hotkey (with the hardware Copilot key as the default target).
- Overlay window sizing/position preferences.

## 11. Explicit Non-Goals

- This project does not reimplement tool execution, MCP handling, or model routing — all of that stays inside Goose.
- No custom telemetry, analytics, or cloud sync beyond what Goose itself already does.
- No built-in Ollama model catalog/search UI — that's delegated to the Ollama website.
- No cross-platform support for the initial version — Windows only.
- The installer handles initial setup of Ollama/Goose/first model; it is not intended to become a general package manager or ongoing auto-updater for those tools beyond what they already provide themselves.

## 12. Open Questions to Resolve in the Technical Spec

- Exact hotkey-interception mechanism for the Copilot key on Windows (low-level keyboard hook vs. other approach), and a documented fallback if it proves unreliable.
- Whether the overlay and settings window share a single background process or are launched independently.
- How much of the composer/chat UI is shared code between overlay mode and full-window mode.
- Data model and encryption approach for provider profiles (local/remote flag, encrypted credential storage).
- How the artifacts sidepane identifies "artifacts" out of the tool-call stream (e.g. which tool outputs count vs. don't).
- Exact set of Ollama environment variables to expose in the Advanced helper, and how changes are applied (env var + restart vs. live).
- Whether theme files are plain CSS only, or CSS plus a small metadata file (for things like the background-image setting).
- Installer mechanics: standalone installer executable vs. first-run wizard bundled in the main app; how Ollama/Goose installers are invoked (silent flags vs. handing off to their own UI).
- The curated ≤4B starter model list: which specific models, and how the list gets updated over time.
- Goose version compatibility: goosed's API surface is actively evolving (a migration away from its custom streaming API is underway), so the spec should pin a supported Goose version range and define an upgrade policy.
- Detection mechanism for a running stock Goose Desktop instance (process name, port probe, or lockfile).
