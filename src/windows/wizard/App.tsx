/** First-run / repair wizard. Placeholder until Phase 7 (detect → install →
    configure → first model → done). The window exists now so it can be created
    and shown like the others. */
export function App() {
  return (
    <div className="window-root">
      <h1 style={{ fontSize: 20, marginTop: 0 }}>Goose Setup</h1>
      <p className="muted">
        The first-run wizard (dependency detection, installing Ollama/Goose, pulling a starter
        model) is built in Phase 7.
      </p>
    </div>
  );
}
