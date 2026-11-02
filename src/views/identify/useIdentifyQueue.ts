/**
 * The review queue, a window at a time.
 *
 * The store pages the queue (`identify.queue`, at most 128 rows a call), so
 * this keeps only the pages the list has scrolled past, as the track table
 * does: the list says which rows it shows and the missing pages are asked
 * for. A finished track or a change to a result (`identify:result`,
 * `identify:changed`) marks every page stale; the rows stay on screen until
 * their replacements arrive, so a refresh never blanks the list. Refreshes
 * that land together are coalesced into one.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { getBackend } from "@/ipc/client";
import type { IdentifyFilter, IdentifyQueueItem } from "@/ipc/types";
import { errorMessage } from "@/lib/errorMessage";
import { planFetches } from "@/lib/virtual";

export const QUEUE_PAGE = 64;
/** How long a burst of events is gathered before the visible pages are re-read. */
const SETTLE_MS = 150;

interface Page {
  generation: number;
  rows: readonly IdentifyQueueItem[];
}

export interface IdentifyQueueWindow {
  filter: IdentifyFilter;
  /** Rows the filter holds; null until the first page has answered. */
  total: number | null;
  error: string | null;
  rowAt: (index: number) => IdentifyQueueItem | undefined;
  /** The rows the list shows now, `[start, end)`. */
  show: (start: number, end: number) => void;
  /** Re-reads what is on screen, keeping it there meanwhile. */
  refresh: () => void;
}

export function useIdentifyQueue(filter: IdentifyFilter): IdentifyQueueWindow {
  const [total, setTotal] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Bumped when a page lands: the rows live in a ref, outside React state,
  // so a page re-renders the list once rather than per row.
  const [, setLanded] = useState(0);
  const pages = useRef(new Map<number, Page>());
  const generation = useRef(0);
  const inFlight = useRef(new Set<number>());
  const shown = useRef({ start: 0, end: QUEUE_PAGE });
  const filterRef = useRef(filter);
  const settle = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  const fetchMissing = useCallback(() => {
    const { start, end } = shown.current;
    const first = Math.floor(Math.max(0, start) / QUEUE_PAGE);
    const last = Math.floor(Math.max(0, end - 1) / QUEUE_PAGE);
    const missing: number[] = [];
    for (let page = first; page <= last; page++) {
      if (pages.current.get(page)?.generation !== generation.current) missing.push(page);
    }
    for (const page of planFetches(missing, inFlight.current)) {
      inFlight.current.add(page);
      const asked = { filter: filterRef.current, generation: generation.current };
      void getBackend()
        .then((backend) => backend.identify.queue(asked.filter, page * QUEUE_PAGE, QUEUE_PAGE))
        .then((answer) => {
          if (asked.filter !== filterRef.current) return;
          if (asked.generation === generation.current) {
            pages.current.set(page, { generation: asked.generation, rows: answer.items });
          }
          // Pages past the end are rows that have left the filter.
          for (const key of pages.current.keys()) {
            if (key * QUEUE_PAGE >= answer.total) pages.current.delete(key);
          }
          setTotal(answer.total);
          setError(null);
          setLanded((n) => n + 1);
        })
        .catch((e: unknown) => {
          if (asked.filter === filterRef.current) setError(errorMessage(e));
        })
        .finally(() => {
          inFlight.current.delete(page);
          // A refresh that came in while this was out asked for a newer generation.
          if (asked.filter === filterRef.current && asked.generation !== generation.current) fetchMissing();
        });
    }
  }, []);

  const refresh = useCallback(() => {
    generation.current += 1;
    fetchMissing();
  }, [fetchMissing]);

  const show = useCallback((start: number, end: number) => {
    shown.current = { start, end: Math.max(end, start + 1) };
    fetchMissing();
  }, [fetchMissing]);

  // A new filter is a new list: nothing of the old one is shown.
  useEffect(() => {
    filterRef.current = filter;
    pages.current.clear();
    inFlight.current.clear();
    generation.current += 1;
    shown.current = { start: 0, end: QUEUE_PAGE };
    setTotal(null);
    setError(null);
    fetchMissing();
  }, [filter, fetchMissing]);

  useEffect(() => {
    let live = true;
    const stops: (() => void)[] = [];
    const soon = () => {
      clearTimeout(settle.current);
      settle.current = setTimeout(() => { if (live) refresh(); }, SETTLE_MS);
    };
    void getBackend().then((backend) => {
      if (!live) return;
      stops.push(backend.identify.onResult(soon), backend.identify.onChanged(soon));
    });
    return () => {
      live = false;
      clearTimeout(settle.current);
      for (const stop of stops) stop();
    };
  }, [refresh]);

  const rowAt = useCallback((index: number) =>
    pages.current.get(Math.floor(index / QUEUE_PAGE))?.rows[index % QUEUE_PAGE], []);

  return useMemo(() => ({ filter, total, error, rowAt, show, refresh }), [filter, total, error, rowAt, show, refresh]);
}
