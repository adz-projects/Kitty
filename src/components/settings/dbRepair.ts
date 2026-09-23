import { ipc, type DbRecoverReport } from '@/lib/ipc';

/** Confirmation shown before a memory-database check & repair. */
export const DB_REPAIR_CONFIRM =
  'Check the memory database? If it is damaged, Kitty restarts its memory service to ' +
  'rebuild it. The current file is backed up first, and a reply in progress may be interrupted.';

/** Check a memory engine's database and, if it's damaged, get it rebuilt.
    Returns the result message for the Health pane.

    A damaged file is only rebuilt when the memory service starts, before
    anything has it open (swapping it under a running engine is what corrupts
    it). So when the check reports `restart_required`, restart the service and
    check again; the fresh start will have done the rebuild. */
export async function runDbRepair(recover: () => Promise<DbRecoverReport>): Promise<string> {
  let r = await recover();
  if (r.restart_required) {
    await ipc.restartBackend();
    r = await recover();
  }

  if (r.open_error) {
    return (
      `${r.rebuilt ? 'The database was rebuilt' : 'The database file is intact'}, ` +
      `but the memory engine could not start: ${r.open_error}`
    );
  }
  if (r.rebuilt && r.integrity_ok) {
    const total = Object.values(r.salvaged).reduce((a, b) => a + b, 0);
    return (
      `Rebuilt the database (${total} record${total === 1 ? '' : 's'} recovered).` +
      (r.backup ? ` A backup was saved to ${r.backup}.` : '')
    );
  }
  if (r.restart_required || !r.integrity_ok) {
    // Still damaged after a restart: on Android the service can't restart in
    // place, so the rebuild happens on the next launch.
    return 'The database is damaged. It will be rebuilt the next time Kitty starts.';
  }
  return 'Database is healthy — no repair needed.';
}
