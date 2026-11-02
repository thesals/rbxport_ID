/**
 * The identify job (fork-only; `docs/identify/IDENTIFY_SPEC.md`), as the
 * shell sees it: the running or last job's progress, a start and a cancel.
 *
 * The backend owns the job. This reads where it stands once on mount — so a
 * window or panel opened mid-job shows it — and then follows the
 * `identify:progress` events; nothing is polled.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { getBackend } from "@/ipc/client";
import type { IdentifyProgress } from "@/ipc/types";
import { errorMessage } from "@/lib/errorMessage";

export interface IdentifyJob {
  /** The running or last job; null before the first. */
  progress: IdentifyProgress | null;
  running: boolean;
  /** Why the last start or cancel was refused; null otherwise. */
  error: string | null;
  /** Resolves true once the job has started. */
  start: (trackIds: readonly string[]) => Promise<boolean>;
  cancel: () => void;
  clearError: () => void;
}

/** A later frame of the same job never gives way to an earlier one. */
function newer(current: IdentifyProgress | null, next: IdentifyProgress): boolean {
  if (!current || current.jobId !== next.jobId) return !current || next.jobId >= current.jobId;
  if (current.state !== "running") return next.state !== "running";
  return next.done >= current.done;
}

export function useIdentifyJob(): IdentifyJob {
  const [progress, setProgress] = useState<IdentifyProgress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const jobRef = useRef<number | null>(null);
  jobRef.current = progress?.jobId ?? null;

  useEffect(() => {
    let live = true;
    let stop: (() => void) | undefined;
    const take = (next: IdentifyProgress) => {
      if (live) setProgress((current) => newer(current, next) ? next : current);
    };
    void getBackend().then(async (backend) => {
      if (!live) return;
      stop = backend.identify.onProgress(take);
      const now = await backend.identify.progress().catch(() => null);
      if (now) take(now);
    });
    return () => {
      live = false;
      stop?.();
    };
  }, []);

  const start = useCallback(async (trackIds: readonly string[]) => {
    setError(null);
    try {
      const backend = await getBackend();
      const jobId = await backend.identify.start(trackIds);
      // Shown as running before its first event, so a panel opened now does
      // not read the last job's end.
      setProgress((current) => current && current.jobId >= jobId ? current : {
        jobId, state: "running", done: 0, total: new Set(trackIds).size, currentTitle: "", currentPath: "",
        identified: 0, review: 0, unidentified: 0, failed: 0, message: null,
      });
      return true;
    } catch (e) {
      setError(errorMessage(e));
      return false;
    }
  }, []);

  const cancel = useCallback(() => {
    const jobId = jobRef.current;
    if (jobId === null) return;
    void getBackend()
      .then((backend) => backend.identify.cancel(jobId))
      .catch((e: unknown) => setError(errorMessage(e)));
  }, []);

  const clearError = useCallback(() => setError(null), []);

  return useMemo(() => ({
    progress, running: progress?.state === "running", error, start, cancel, clearError,
  }), [progress, error, start, cancel, clearError]);
}
