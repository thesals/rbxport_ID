/**
 * Owns a track view: opens it on the backend, pages rows in as they are
 * scrolled into range, and drops stale pages when the view changes.
 *
 * The library itself is never held here — only a bounded LRU of fetched pages.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { getBackend } from "@/ipc/client";
import type { Backend, RowCue, RowDto, ViewSpec } from "@/ipc/types";
import { rowCuesOf } from "@/lib/cues";
import { RowCache, PAGE_SIZE, type CacheToken } from "@/lib/rowCache";
import { planFetches } from "@/lib/virtual";

export interface TrackView {
  /** Row count of the current view. */
  count: number;
  /** Identity of the current view's data; changes whenever rows must be refetched. */
  token: CacheToken;
  loading: boolean;
  error: string | null;
  /** Row at an absolute index, or undefined while its page is in flight. */
  rowAt: (index: number) => RowDto | undefined;
  /** Ask for the pages covering [start, end); safe to call every frame. */
  ensureRange: (start: number, end: number) => void;
  /** Ids between two row indices inclusive, resolved by the backend. */
  idsInRange: (from: number, to: number) => Promise<string[]>;
}

/**
 * Edits applied to rows before the backend has caught up.
 *
 * A write makes the backend re-read the library, which on the reference
 * collection is 243 ms. Waiting for that before a star fills in makes the
 * interface feel broken, so the edit is shown at once and dropped when the
 * reload lands with the same value in it.
 */
export type PendingEdits = ReadonlyMap<string, Partial<RowDto>>;

/**
 * Rows to draw before the backend has answered.
 *
 * The last screen, kept from the previous run so the window is not empty while
 * the library is read. Dropped the instant a real page lands.
 */
export interface Seed {
  count: number;
  rows: readonly RowDto[];
}

export function useTrackView(
  spec: ViewSpec,
  libraryGeneration = 0,
  pending?: PendingEdits,
  seed?: Seed,
  extraColumns: readonly string[] = [],
): TrackView {
  const extraKey = extraColumns.join(",");
  // `specKey` records which spec this state describes. Loading is derived from
  // comparing it against the current spec rather than set by an effect: an
  // effect runs *after* the render that changed the spec, so for one frame the
  // new playlist's title was drawn beside the previous view's row count.
  const [state, setState] = useState({
    viewId: 0,
    count: 0,
    gen: 0,
    specKey: "",
    error: null as string | null,
  });
  const cache = useRef(new RowCache<RowDto>(PAGE_SIZE));
  const inFlight = useRef(new Set<number>());
  // Weak keys follow the page cache's lifetime. Repeated reads of one pending
  // edit keep object identity without retaining evicted rows or stale patches.
  const overlays = useRef(new WeakMap<RowDto, { edit: Partial<RowDto>; row: RowDto }>());
  // Bumped when a page lands, to re-render the rows it filled.
  const [pagesLoaded, setPagesLoaded] = useState(0);
  // Bumped when the Tag List changes. The backend keeps the generation for
  // that edit, since no other list shows it, so a Tag List view reopens on
  // this alone and every other view keeps the pages it has.
  const [tagListRevision, setTagListRevision] = useState(0);
  const tagListKey = spec.source.kind === "tagList" ? tagListRevision : 0;

  // The library generation is part of the key: an edit changes the rows under
  // a spec that has not itself changed, and without this the view would keep
  // serving the pages it cached before the edit.
  const specKey = useMemo(
    () =>
      JSON.stringify([
        spec.source, spec.sort, spec.descending, spec.query, spec.searchField, spec.filter ?? null, libraryGeneration, tagListKey, extraKey,
      ]),
    [spec.source, spec.sort, spec.descending, spec.query, spec.searchField, spec.filter, libraryGeneration, tagListKey, extraKey],
  );

  // View identity for the cache: a new view id, or a library change, invalidates pages.
  const token: CacheToken = `${state.viewId}:${state.gen}`;

  useEffect(() => {
    let cancelled = false;
    let opened = false;
    let stopReady: (() => void) | undefined;
    cache.current.clear();
    inFlight.current.clear();
    setState((s) => ({ ...s, error: null }));

    const attempt = async (backend: Backend) => {
      // One open per spec. A later ready event — a library reload — must not
      // make every view that is already up refetch itself.
      if (cancelled || opened) return;
      try {
        const handle = await backend.openView(spec);
        if (cancelled) return;
        opened = true;
        setState({
          viewId: handle.viewId,
          count: handle.len,
          gen: handle.gen,
          specKey,
          error: null,
        });
      } catch (e) {
        if (cancelled) return;
        // Mark the failure as belonging to this spec, or it reads as still
        // loading and retries forever.
        setState((s) => ({ ...s, specKey, error: e instanceof Error ? e.message : String(e) }));
      }
    };

    void (async () => {
      const backend = await getBackend();
      if (cancelled) return;
      // The library is read on its own thread, so this open can arrive before
      // there is anything to answer it and come back "not finished loading".
      // That is a race the app is expected to lose sometimes, and it has to
      // recover on its own: the effect keys on the spec, so without this a
      // view that lost it stayed at zero rows until the user picked something
      // else — and a table with a count of zero draws nothing at any scroll
      // position, which is a blank window.
      //
      // Subscribed before the first attempt, not after, for the reason
      // `App.tsx` gives about the tree: the library can become ready in the
      // gap between a failed attempt and a later subscription, and that gap is
      // exactly where this used to get stuck.
      const ready = backend.onLibraryReady(() => {
        void attempt(backend);
      });
      stopReady = ready;
      // Listening, not only subscribed: see App.tsx.
      await ready.registered;
      if (cancelled) return;
      await attempt(backend);
    })();

    return () => {
      cancelled = true;
      stopReady?.();
    };
    // specKey captures every field that changes the view.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [specKey]);

  useEffect(() => {
    let stop: (() => void) | undefined;
    let live = true;
    void (async () => {
      const backend = await getBackend();
      if (!live) return;
      stop = backend.onTagListChanged(() => setTagListRevision((n) => n + 1));
    })();
    return () => {
      live = false;
      stop?.();
    };
  }, []);

  /*
   * A cue edit changes one row's letters and nothing else, and the backend
   * re-reads only that track rather than reloading the library — so no new
   * generation arrives to drop the pages. The row is patched where it is
   * cached instead: one fetch of the track's cues, which is what the backend
   * built the column from. A row not on a cached page needs nothing; the
   * page it is on will be fetched fresh.
   */
  useEffect(() => {
    let stop: (() => void) | undefined;
    let live = true;
    void (async () => {
      const backend = await getBackend();
      if (!live) return;
      stop = backend.onCuesChanged((trackId) => {
        // Only a row the cache holds is worth the fetch.
        if (!cache.current.holds((row) => row.id === trackId)) return;
        void backend.trackCues(trackId).then((cues) => {
          if (!live) return;
          const hotCues = rowCuesOf(cues);
          const memoryCues = cues.filter(cue => cue.memory).map(cue => cue.positionMs);
          const changed = cache.current.patch(
            (row) => row.id === trackId && (!sameRowCues(row.hotCues, hotCues)
              || (row.memoryCues?.length ?? 0) !== memoryCues.length
              || memoryCues.some((position, i) => row.memoryCues?.[i] !== position)),
            (row) => ({ ...row, hotCues, memoryCues }),
          );
          if (changed) setPagesLoaded((n) => n + 1);
        }).catch(() => {
          // Leave the row as it was; the next fetch of its page is current.
        });
      });
    })();
    return () => {
      live = false;
      stop?.();
    };
  }, []);

  const ensureRange = useCallback(
    (start: number, end: number) => {
      const { viewId, count } = state;
      const loading = state.specKey !== specKey;
      // Fetching while a view swap is in flight would fill the cache from the
      // outgoing view under the incoming token.
      if (!viewId || count === 0 || loading) return;
      const missing = cache.current.missingPages(start, Math.min(end, count), token);
      const toFetch = planFetches(missing, inFlight.current);
      if (toFetch.length === 0) return;

      for (const page of toFetch) {
        inFlight.current.add(page);
        void (async () => {
          try {
            const backend = await getBackend();
            const rows = await backend.fetchRows(viewId, page * PAGE_SIZE, PAGE_SIZE, extraColumns);
            // A view swap between request and response makes this page stale.
            if (cache.current.hasPage(page, token)) return;
            cache.current.setPage(page, token, rows);
            setPagesLoaded((n) => n + 1);
          } catch {
            // Leave the page missing; the next scroll retries it.
          } finally {
            inFlight.current.delete(page);
          }
        })();
      }
    },
    // `specKey` matters as much as `state`: between a spec change and its
    // fetch resolving, only `specKey` has moved, and a stale one here reads as
    // "not loading" and fills the cache from the outgoing view.
    // extraKey changes only with visible extra columns, not on width drags.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [state, token, specKey, extraKey],
  );

  const rowAt = useCallback(
    (index: number) => {
      const row = cache.current.get(index, token);
      if (!row || !pending) return row;
      // The cached row is what the backend last said; the overlay is what the
      // user just did. Merging rather than mutating keeps the cache honest.
      const edit = pending.get(row.id);
      if (!edit) return row;
      const held = overlays.current.get(row);
      if (held?.edit === edit) return held.row;
      const merged = { ...row, ...edit };
      overlays.current.set(row, { edit, row: merged });
      return merged;
    },
    // `pagesLoaded` is not read here on purpose: the cache is a ref, so this
    // counter is the only signal that a page arrived and callers must redraw.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [token, pagesLoaded, pending],
  );

  const idsInRange = useCallback(
    async (from: number, to: number) => {
      if (!state.viewId) return [];
      const backend = await getBackend();
      return backend.viewIdsInRange(state.viewId, from, to);
    },
    [state.viewId],
  );

  // Nothing has been opened yet, so the seed is all there is to draw. Once a
  // view exists the seed is gone for good — it is a picture of the last run,
  // not a fallback for a slow page.
  const unopened = state.viewId === 0;
  const seeded = unopened && seed !== undefined && seed.rows.length > 0;

  const seedCount = seeded ? seed.count : 0;
  const rows = seeded ? seed.rows : null;
  // Memoised as a whole. A fresh object every render is a changed dependency
  // for every effect that takes the view rather than a field of it, and one of
  // those handed a new array back up to the app on each pass, which re-rendered
  // the window and made the view new again.
  return useMemo(
    () => ({
      count: rows ? seedCount : state.count,
      token,
      loading: state.specKey !== specKey,
      error: state.error,
      rowAt: rows ? (index: number) => rows[index] : rowAt,
      ensureRange,
      idsInRange,
    }),
    [rows, seedCount, state.count, state.specKey, state.error, token, specKey, rowAt, ensureRange, idsInRange],
  );
}

/** Whether two rows' hot-cue tuples read the same, so an unchanged row is not re-rendered. */
function sameRowCues(a: readonly RowCue[], b: readonly RowCue[]): boolean {
  return a.length === b.length && a.every((x, i) => {
    const y = b[i];
    return y !== undefined && x[0] === y[0] && x[1] === y[1] && x[2] === y[2];
  });
}
