import { useEffect, useState } from 'react';
import { ipc, onMemoryStatus } from '@/lib/ipc';
import type { MemoryStatus } from '@/lib/types';

/** Where the memory engines stand (`lifecycle::memory`), kept current by
    `memory://status` — a download finishing turns them on without a reload. */
export function useMemoryStatus(): MemoryStatus | null {
  const [status, setStatus] = useState<MemoryStatus | null>(null);
  useEffect(() => {
    void ipc
      .getMemoryStatus()
      .then(setStatus)
      .catch(() => {});
    const un = onMemoryStatus(setStatus);
    return () => void un.then((f) => f());
  }, []);
  return status;
}
