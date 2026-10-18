/** Four-pointed spark — one step of reasoning effort in the chat's effort
    picker. Monochrome on purpose (`currentColor`), so it follows the theme's
    text colour on Windows and Android alike instead of depending on how a
    platform font draws a Unicode star. Icon-only; the caller supplies the
    accessible label. */
export function SparkIcon({ size = 12 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" aria-hidden="true">
      <path d={SPARK_PATH} fill="currentColor" />
    </svg>
  );
}

/** Reasoning off: the spark's outline, struck through. */
export function SparkOffIcon({ size = 12 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path d={SPARK_PATH} stroke="currentColor" strokeWidth="1.2" strokeLinejoin="round" />
      <path d="M2.5 13.5 13.5 2.5" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
    </svg>
  );
}

const SPARK_PATH =
  'M8 1C8.5 5.2 10.8 7.5 15 8C10.8 8.5 8.5 10.8 8 15C7.5 10.8 5.2 8.5 1 8C5.2 7.5 7.5 5.2 8 1Z';
