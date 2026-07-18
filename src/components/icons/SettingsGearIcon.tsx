/** Gear — settings buttons, provider badge's "no active provider" fallback.
    A ring with discrete rectangular teeth and a center hole, not radiating
    spokes (an earlier version of this icon used thin spokes off a small
    circle, which read as a sun rather than a gear at 14px). Replaces ⚙. */
const TOOTH_ANGLES = [0, 45, 90, 135, 180, 225, 270, 315];

export function SettingsGearIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <circle cx="8" cy="8" r="4.1" stroke="currentColor" strokeWidth="1.3" />
      <circle cx="8" cy="8" r="1.6" stroke="currentColor" strokeWidth="1.3" />
      {TOOTH_ANGLES.map((deg) => (
        <rect
          key={deg}
          x="7.15"
          y="1.9"
          width="1.7"
          height="2.1"
          rx="0.4"
          fill="currentColor"
          transform={`rotate(${deg} 8 8)`}
        />
      ))}
    </svg>
  );
}
