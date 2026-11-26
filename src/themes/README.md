# Theme contract

A theme is **one CSS file** that sets custom properties on `:root`. `base.css`
(structural, color-free) consumes them. Built-in styles live here — `classic`
(`light.css`, `dark.css`), `modern` (`modern-light.css`, `modern-dark.css`),
and `cli` (`cli-light.css`, `cli-dark.css`); user themes are `.css` files
dropped into `%APPDATA%/Kitty/themes/`, which then appear in Settings →
Appearance and apply at runtime with no rebuild.

## Required custom properties

| Property            | Meaning                                        |
| ------------------- | ---------------------------------------------- |
| `--bg`              | Window background (opaque windows)             |
| `--surface`         | Card / panel background                        |
| `--surface-2`       | Secondary surface (inputs, code blocks)        |
| `--text`            | Primary text                                   |
| `--text-muted`      | Secondary / muted text                         |
| `--accent`          | Accent (primary buttons, active state)         |
| `--accent-text`     | Text on the accent fill                        |
| `--border`          | Borders / dividers                             |
| `--radius`          | Corner radius for cards and dialogs (e.g. `4px`) |
| `--font-family`     | UI font stack                                  |
| `--overlay-opacity` | Overlay card opacity (`0`–`1`)                 |
| `--overlay-shadow`  | Shadow under the Android menu drawer and artifacts sheet |
| `--ok` / `--warn` / `--danger` | Status colors                       |

## Optional custom properties

| Property        | Meaning                                                       |
| --------------- | ------------------------------------------------------------- |
| `--accent-fill` | Background behind `--accent-text` (primary buttons, your own messages). Defaults to `--accent`; set it when the accent is too light to carry text, as in the built-in dark theme. |
| `--mono`        | Monospace font stack                                          |
| `--font-reading` | Typeface for the running text of the chat (the user's turns and the answer's prose). Defaults to the UI font. Open Sans is bundled: `'Open Sans Variable', sans-serif`; so is EB Garamond: `'EB Garamond Variable', serif`. `--font-serif` is the older name and still works. |
| `--chat-text`   | Colour of that running text. Defaults to `--text`. The built-in themes set it for about 7:1 contrast against `--bg`; headings stay in `--text`. |
| `--chat-strong` | Colour of bold within that text. Defaults to `--text`; the built-in themes set it a little dimmer. |
| `--radius-sm`   | Corner radius for buttons, chips and badges. Defaults to ¾ of `--radius`. |
| `--reading-max-width` / `--reading-line-height` | Width of the chat column (default `44rem`) and the leading of chat text (default `1.6`). |
| `--chat-font-size` | Size of chat text: the user's turns and the answer's prose (default `0.90625rem`, 14.5px, chosen for Open Sans; a serif reading face wants about a quarter more). |
| `--selection`   | Text-selection background. Defaults to the system highlight.  |
| `--find-match`  | Background of every match while finding in a chat (Ctrl+F). Defaults to `--selection`. |
| `--find-current` | Background of the current match. Defaults to `--accent`. |
| `--find-current-text` | Text colour of the current match. Defaults to `--accent-text`. |
| `--ok-bg` / `--warn-bg` / `--danger-bg` | Tinted backgrounds for success, warning and error panels. Default to `--surface-2` or none. |

## Figures

Charts, diagrams and tables the assistant draws are styled by `viz.css`, in
black and paper plus one accent. They follow the theme through these optional
properties; without them a figure uses the fallback shown.

| Property       | Meaning                                           | Falls back to   |
| -------------- | ------------------------------------------------- | --------------- |
| `--viz-ink`    | Lines, type, solid fills                          | `--text`        |
| `--viz-paper`  | Shape fills, the ground behind hatching           | `--surface`     |
| `--viz-rule`   | Grid lines, table hairlines                       | `--border`      |
| `--viz-muted`  | Tick labels, the grey series                      | `--text-muted`  |
| `--viz-tint`   | Shaded bands                                      | `--surface-2`   |
| `--viz-accent` | The second series; emphasis                       | `--danger`      |
| `--viz-font`   | Typeface. Defaults to Helvetica, then Arial; label widths are measured for those, so a much wider face can crowd labels. | — |

## Minimal example

```css
:root {
  --bg: #101014;
  --surface: #1b1b22;
  --surface-2: #14141a;
  --text: #e8e8f0;
  --text-muted: #9a9aa8;
  --accent: #7c5cff;
  --accent-text: #ffffff;
  --border: #2a2a33;
  --radius: 12px;
  --font-family: 'Segoe UI', system-ui, sans-serif;
  --overlay-opacity: 1;
  --overlay-shadow: 0 12px 40px rgba(0, 0, 0, 0.6);
  --ok: #22c55e;
  --warn: #f59e0b;
  --danger: #f87171;
}
```

## Background image

Set independently in Settings → Appearance (not part of the theme file). It is
applied on the document root with a dim overlay controlled by `--bg-image-dim`
(`0`–`1`); opaque window surfaces become transparent so the image shows through.
