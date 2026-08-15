/** The Kitty mark, traced from `src-tauri/icons/kitty-source.svg` — the same
    source the app/tray/installer icons are generated from, so the two never
    drift apart.
 *
 * `currentColor` rather than the source's hardcoded `#111111`: this renders
 * inside themed chrome, and a fixed near-black would vanish against a dark
 * theme's surface. The source file keeps its literal fill because a `.ico`
 * has no notion of inheriting a colour. */
export function KittyIcon({ size = 22 }: { size?: number }) {
  return (
    <svg
      viewBox="0 0 512 512"
      width={size}
      height={size}
      fill="currentColor"
      aria-hidden="true"
      focusable="false"
    >
      {/* Tail: a low crescent sweeping from the seated rear to the left. */}
      <path
        d="M 330 400 C 300 452 190 486 104 474 C 58 468 52 430 92 434 C 96 410 118 404 140 420
           C 200 452 286 442 322 388 C 336 392 340 388 330 400 Z"
      />
      {/* Sitting body, blending up into the head. */}
      <path
        d="M 322 128 C 384 140 404 236 396 322 C 389 398 352 448 288 448 C 230 448 198 402 200 338
           C 204 232 226 152 276 134 C 290 129 306 127 322 128 Z"
      />
      <circle cx="322" cy="146" r="74" />
      <path d="M 266 122 L 250 44 L 324 98 Z" />
      <path d="M 378 116 L 398 40 L 330 90 Z" />
    </svg>
  );
}
