# Theme contract

A theme is **one CSS file** that sets custom properties on `:root`. `base.css`
(structural, color-free) consumes them. Built-in themes live here
(`default.css`, `dark.css`); user themes are `.css` files dropped into
`%APPDATA%/goose-overlay/themes/`, which then appear in Settings → Appearance and
apply at runtime with no rebuild.

## Required custom properties

| Property            | Meaning                                        |
| ------------------- | ---------------------------------------------- |
| `--bg`              | Window background (opaque windows)             |
| `--surface`         | Card / panel background                        |
| `--surface-2`       | Secondary surface (inputs, code blocks)        |
| `--text`            | Primary text                                   |
| `--text-muted`      | Secondary / muted text                         |
| `--accent`          | Accent (primary buttons, active state)         |
| `--accent-text`     | Text on the accent color                       |
| `--border`          | Borders / dividers                             |
| `--radius`          | Corner radius (e.g. `12px`)                    |
| `--font-family`     | UI font stack                                  |
| `--overlay-opacity` | Overlay card opacity (`0`–`1`)                 |
| `--overlay-shadow`  | Overlay card box-shadow                        |
| `--ok` / `--warn` / `--danger` | Status colors                       |

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
