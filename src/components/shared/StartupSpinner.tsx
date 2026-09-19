// Shown in place of the "engine isn't running" panel while the backend is
// still coming up on first start (Windows) or any start (Android) — see
// stackStore's grace window (`selectBooting`). A transient early
// `backend_down` is a slow port bind, not a failure, so a spinner covers the
// lag instead of a hard error the user is told to "restart".
export function StartupSpinner() {
  return (
    <div className="startup-spinner" role="status" aria-live="polite">
      <span className="startup-spinner-dot" aria-hidden="true" />
      <span className="muted">Starting Kitty…</span>
    </div>
  );
}
