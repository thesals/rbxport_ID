/**
 * Track identification in a browser: a fake background job and an
 * in-memory review queue with the identify store's rules, so `pnpm dev:web`
 * and the tests can drive the flow without keys, audio or network.
 *
 * The job identifies one track per tick (timers chained with `setTimeout`,
 * never polled), emitting `progress` as a track starts and `result` as it
 * ends, then one final progress — the order the Rust job keeps. Outcomes
 * cycle through review, unidentified, auto-apply, review and a failure, so
 * every queue filter has something in it.
 *
 * Kept apart from `backend-mock.ts` so the backend does not grow another
 * few hundred lines.
 */
import type {
  AppErrorDto, IdentifyBackend, IdentifyCandidate, IdentifyChangedEvent, IdentifyDecision, IdentifyDetail, IdentifyFieldChange,
  IdentifyFilter, IdentifyProgress, IdentifyQueueItem, IdentifyResultEvent, IdentifyReviewAction, IdentifyReviewState,
  IdentifyWriteOutcome,
} from "./types";

/** The most rows one queue call answers, as `MAX_ROWS` is in Rust. */
export const IDENTIFY_MAX_ROWS = 128;

export interface MockIdentifyOptions {
  /** Time a track takes; tests shorten it. */
  tickMs?: number;
  /** The library's title and artist for a track id, when it has one. */
  describe?: (trackId: string) => { title: string; artist: string } | undefined;
}

type Outcome = IdentifyDecision | "failed";
/** What the tracks of a job come out as, in turn. */
const OUTCOMES: readonly Outcome[] = ["review", "unidentified", "autoApply", "review", "failed"];

/** The comment a Beatport download carries, which `subGenre` appends to. */
const COMMENT = "Purchased at Beatport.com";

const fail = (kind: AppErrorDto["kind"], message: string) =>
  Promise.reject(Object.assign(new Error(message), { kind }));

/** Transitions the store allows through a review action (`ReviewState::may_become`). */
const ALLOWED: Record<IdentifyReviewState, readonly IdentifyReviewState[]> = {
  pending: ["accepted", "rejected"],
  accepted: ["pending", "rejected"],
  rejected: ["pending"],
  undone: ["pending"],
  applied: [],
  autoApplied: [],
  unidentified: [],
};

const TARGET: Record<IdentifyReviewAction, IdentifyReviewState> = {
  accept: "accepted",
  reject: "rejected",
  pending: "pending",
};

const inFilter = (filter: IdentifyFilter, state: IdentifyReviewState): boolean => {
  switch (filter) {
    case "unidentified": return state === "unidentified";
    case "needsReview": return state === "pending";
    case "applied": return state === "applied" || state === "autoApplied";
    case "all": return true;
  }
};

const candidate = (artist: string, title: string, score: number): IdentifyCandidate => ({
  recordingMbid: "41f4b491-cc31-40f1-8d81-cc58b7d8f761",
  artist,
  title,
  disambiguation: "",
  remixers: [],
  lengthMs: 360_000,
  isrcs: ["GBXXX1400001"],
  genres: [],
  releases: [{
    mbid: "c3132648-11c5-4476-b152-979253d9241a", title: `${title} EP`, date: "2014-06-02", country: "XW",
    label: "Mock Records", catalogNumber: "MOCK001", releaseGroupMbid: "", releaseGroupType: "EP", secondaryTypes: [],
  }],
  source: "fingerprint",
  score,
  acoustidScore: 0.97,
  durationScore: 1,
  textScore: 0.9,
  mixScore: 1,
  rejected: null,
});

export function createMockIdentify(options: MockIdentifyOptions = {}): IdentifyBackend {
  const tickMs = options.tickMs ?? 400;
  const progressListeners = new Set<(progress: IdentifyProgress) => void>();
  const resultListeners = new Set<(result: IdentifyResultEvent) => void>();
  const changedListeners = new Set<(event: IdentifyChangedEvent) => void>();
  /** The values an apply replaced, by result, for its undo. */
  const snapshots = new Map<number, IdentifyFieldChange[]>();
  /** Results written at least once: only an auto-apply's first write is autoApplied. */
  const written = new Set<number>();
  /** Every result, current or replaced, by id. */
  const results = new Map<number, IdentifyDetail>();
  /** Each file's current result. */
  const current = new Map<string, number>();
  let nextResult = 1;
  let nextJob = 1;
  let running: { jobId: number; cancelled: boolean } | null = null;
  let last: IdentifyProgress | null = null;

  const tellProgress = (progress: IdentifyProgress) => {
    // A final progress frees the slot first, as it does in Rust.
    if (progress.state !== "running" && running?.jobId === progress.jobId) running = null;
    last = progress;
    for (const listener of progressListeners) listener({ ...progress });
  };
  const tellResult = (result: IdentifyResultEvent) => {
    for (const listener of resultListeners) listener({ ...result });
  };
  const tellChanged = (resultIds: readonly number[]) => {
    if (resultIds.length === 0) return;
    for (const listener of changedListeners) listener({ resultIds: [...resultIds] });
  };

  /** Stores one finished track as the store would, and returns its id. */
  const save = (trackId: string, path: string, artist: string, title: string, decision: IdentifyDecision): number => {
    const id = nextResult++;
    const score = decision === "unidentified" ? 0.31 : decision === "autoApply" ? 0.97 : 0.81;
    const changes: IdentifyFieldChange[] =
      decision === "unidentified"
        ? []
        : decision === "autoApply"
          ? [
              { field: "label", current: "", proposed: "Mock Records" },
              { field: "catalogNumber", current: "", proposed: "MOCK001" },
            ]
          : [
              { field: "title", current: title, proposed: `${title} (Extended Mix)` },
              { field: "label", current: "Old Label", proposed: "Mock Records" },
              { field: "subGenre", current: COMMENT, proposed: `${COMMENT}; Tech House` },
              { field: "artwork", current: "none", proposed: "JPEG from Cover Art Archive" },
            ];
    const state: IdentifyReviewState =
      decision === "unidentified" ? "unidentified" : decision === "autoApply" ? "accepted" : "pending";
    results.set(id, {
      item: {
        resultId: id, trackId, path, state, decision, score: decision === "unidentified" ? null : score,
        artist, title, changes, updatedUnix: Math.floor(Date.now() / 1000), undoable: false,
      },
      existing: {
        artist, title, album: "", label: decision === "review" ? "Old Label" : "", year: null, isrc: "", genre: "",
        catalogNumber: "", comment: decision === "review" ? COMMENT : "", hasArtwork: decision !== "review",
      },
      candidates: decision === "unidentified" ? [] : [candidate(artist, `${title} (Extended Mix)`, score)],
      enrichment: null,
      artwork: decision === "review"
        ? { source: "coverArtArchive", url: "https://coverartarchive.org/release/c3132648-11c5-4476-b152-979253d9241a/front-500", sha256: "0".repeat(64), mime: "image/jpeg", bytes: 48_213 }
        : null,
      warnings: decision === "unidentified" ? ["AcoustID: no match"] : [],
      writeInProgress: false,
    });
    current.set(path, id);
    return id;
  };

  const runJob = (jobId: number, trackIds: readonly string[]) => {
    const progress: IdentifyProgress = {
      jobId, state: "running", done: 0, total: trackIds.length, currentTitle: "", currentPath: "",
      identified: 0, review: 0, unidentified: 0, failed: 0, message: null,
    };
    const finish = () => {
      progress.state = running?.cancelled && progress.done < progress.total ? "cancelled" : "done";
      progress.currentTitle = "";
      progress.currentPath = "";
      tellProgress(progress);
    };
    const step = (index: number) => {
      const trackId = trackIds[index];
      if (trackId === undefined || running?.cancelled) {
        finish();
        return;
      }
      const known = options.describe?.(trackId);
      const title = known?.title || `Track ${trackId}`;
      const artist = known?.artist ?? "";
      const path = `/Music/${trackId}.mp3`;
      progress.currentTitle = title;
      progress.currentPath = path;
      tellProgress(progress);
      setTimeout(() => {
        const outcome = OUTCOMES[index % OUTCOMES.length] ?? "failed";
        const result: IdentifyResultEvent = {
          jobId, trackId, resultId: null, path, decision: outcome, score: null, error: null,
        };
        const held = current.get(path);
        const heldState = held === undefined ? undefined : results.get(held)?.item.state;
        if (outcome === "failed") {
          result.error = "The file could not be found.";
        } else if (heldState === "applied" || heldState === "autoApplied") {
          result.decision = "failed";
          result.error = "identify store: this track has an applied result; undo it before identifying again";
        } else {
          result.resultId = save(trackId, path, artist, title, outcome);
          result.score = results.get(result.resultId)?.item.score ?? null;
        }
        if (result.decision === "autoApply") progress.identified += 1;
        else if (result.decision === "review") progress.review += 1;
        else if (result.decision === "unidentified") progress.unidentified += 1;
        else progress.failed += 1;
        progress.done += 1;
        tellResult(result);
        step(index + 1);
      }, tickMs);
    };
    step(0);
  };

  /** A current result with no write open, or the store's refusal. */
  const changeable = (resultId: number): IdentifyDetail | null => {
    const detail = results.get(resultId);
    return detail && current.get(detail.item.path) === resultId ? detail : null;
  };
  const touchless = (detail: IdentifyDetail): IdentifyQueueItem =>
    ({ ...detail.item, changes: detail.item.changes.map((c) => ({ ...c })) });
  const touch = (detail: IdentifyDetail): IdentifyQueueItem => {
    detail.item.updatedUnix = Math.floor(Date.now() / 1000);
    return touchless(detail);
  };

  return {
    start: (trackIds) => {
      if (trackIds.length === 0) return fail("malformed", "Choose at least one track to identify.");
      if (running) return fail("readOnly", "Identification is already running. Wait for it to finish or cancel it.");
      const jobId = nextJob++;
      running = { jobId, cancelled: false };
      const ids = [...new Set(trackIds)];
      last = {
        jobId, state: "running", done: 0, total: ids.length, currentTitle: "", currentPath: "",
        identified: 0, review: 0, unidentified: 0, failed: 0, message: null,
      };
      // The job starts after the call returns, as the thread does.
      setTimeout(() => runJob(jobId, ids), 0);
      return Promise.resolve(jobId);
    },
    cancel: (jobId) => {
      if (running?.jobId === jobId) running.cancelled = true;
      return Promise.resolve();
    },
    progress: () => Promise.resolve(last ? { ...last } : null),
    onProgress: (listener) => {
      progressListeners.add(listener);
      return () => { progressListeners.delete(listener); };
    },
    onResult: (listener) => {
      resultListeners.add(listener);
      return () => { resultListeners.delete(listener); };
    },
    queue: (filter, offset, limit) => {
      const rows = [...current.values()]
        .map((id) => results.get(id)?.item)
        .filter((item): item is IdentifyQueueItem => item !== undefined && inFilter(filter, item.state))
        .sort((a, b) =>
          (a.score === null ? 1 : 0) - (b.score === null ? 1 : 0)
          || (b.score ?? 0) - (a.score ?? 0)
          || a.path.localeCompare(b.path)
          || a.resultId - b.resultId);
      const start = Math.max(0, offset);
      const items = rows.slice(start, start + Math.max(0, Math.min(limit, IDENTIFY_MAX_ROWS)));
      return Promise.resolve({ total: rows.length, items: items.map((item) => ({ ...item })) });
    },
    result: (resultId) => {
      const detail = results.get(resultId);
      if (!detail) return fail("notFound", "That identification result is no longer in the store.");
      // A copy of what a caller could edit: the item and its changes.
      return Promise.resolve({ ...detail, item: touchless(detail) });
    },
    setState: (resultId, action) => {
      const detail = results.get(resultId);
      if (!detail) return fail("notFound", "That identification result is no longer in the store.");
      if (detail.writeInProgress) {
        return fail("readOnly", "Tags are being written to this file. Its review cannot change until the write is finished.");
      }
      const to = TARGET[action];
      if (!changeable(resultId) || !ALLOWED[detail.item.state].includes(to)) {
        return fail("malformed", `A ${detail.item.state} result cannot become ${to}.`);
      }
      detail.item.state = to;
      return Promise.resolve(touch(detail));
    },
    edit: (resultId, edits) => {
      const detail = results.get(resultId);
      if (!detail) return fail("notFound", "That identification result is no longer in the store.");
      if (detail.writeInProgress) {
        return fail("readOnly", "Tags are being written to this file. Its review cannot change until the write is finished.");
      }
      if (!changeable(resultId) || (detail.item.state !== "pending" && detail.item.state !== "accepted")) {
        return fail("malformed", "That change could not be made to this result.");
      }
      // All or nothing, as the store's transaction is.
      const next = detail.item.changes.map((c) => ({ ...c }));
      for (const edit of edits) {
        const change = next.find((c) => c.field === edit.field);
        if (!change || edit.field === "artwork") return fail("malformed", "That change could not be made to this result.");
        change.proposed = edit.proposed;
      }
      detail.item.changes = next;
      const item = touch(detail);
      tellChanged([resultId]);
      return Promise.resolve(item);
    },
    apply: (items) => {
      const outcomes = items.map(({ resultId, fields }): IdentifyWriteOutcome => {
        const detail = changeable(resultId);
        const refuse = (error: string): IdentifyWriteOutcome => ({ resultId, ok: false, error });
        if (!detail) return refuse("That identification result is no longer in the store.");
        if (detail.item.state !== "pending" && detail.item.state !== "accepted") {
          return refuse(`A ${detail.item.state} result cannot be applied.`);
        }
        if (fields.length === 0) return refuse("Choose at least one field to write.");
        const picked = detail.item.changes.filter((c) => fields.includes(c.field));
        if (picked.length !== new Set(fields).size) return refuse("That change could not be made to this result.");
        snapshots.set(resultId, detail.item.changes.map((c) => ({ ...c })));
        detail.item.changes = detail.item.changes.map((c) => fields.includes(c.field) ? { ...c, current: c.proposed } : c);
        for (const change of picked) {
          if (change.field === "artwork") detail.existing.hasArtwork = true;
          else if (change.field === "subGenre") detail.existing.comment = change.proposed;
          else if (change.field === "year") detail.existing.year = Number(change.proposed) || null;
          else if (change.field in detail.existing) {
            (detail.existing as unknown as Record<string, string>)[change.field] = change.proposed;
          }
        }
        // As the store: an auto-apply result's first write is autoApplied.
        detail.item.state = detail.item.decision === "autoApply" && !written.has(resultId) ? "autoApplied" : "applied";
        written.add(resultId);
        detail.item.undoable = true;
        touch(detail);
        return { resultId, ok: true, error: null };
      });
      tellChanged(outcomes.filter((o) => o.ok).map((o) => o.resultId));
      return Promise.resolve(outcomes);
    },
    undo: (resultId) => {
      const detail = changeable(resultId);
      const snapshot = snapshots.get(resultId);
      if (!detail || !detail.item.undoable || !snapshot) {
        return Promise.resolve({ resultId, ok: false, error: "There is nothing to undo for this result." });
      }
      for (const original of snapshot) {
        const now = detail.item.changes.find((c) => c.field === original.field);
        if (!now || now.current === original.current) continue;
        if (original.field === "artwork") detail.existing.hasArtwork = false;
        else if (original.field === "subGenre") detail.existing.comment = original.current;
        else if (original.field === "year") detail.existing.year = Number(original.current) || null;
        else if (original.field in detail.existing) {
          (detail.existing as unknown as Record<string, string>)[original.field] = original.current;
        }
      }
      detail.item.changes = detail.item.changes.map((c) => ({
        ...c, current: snapshot.find((o) => o.field === c.field)?.current ?? c.current,
      }));
      snapshots.delete(resultId);
      detail.item.state = "undone";
      detail.item.undoable = false;
      touch(detail);
      tellChanged([resultId]);
      return Promise.resolve({ resultId, ok: true, error: null });
    },
    acceptAbove: (threshold) => {
      if (!Number.isFinite(threshold)) return fail("malformed", "That change could not be made to this result.");
      const accepted: number[] = [];
      for (const id of current.values()) {
        const detail = results.get(id);
        if (!detail || detail.item.state !== "pending" || detail.writeInProgress) continue;
        if ((detail.item.score ?? -1) < threshold) continue;
        detail.item.state = "accepted";
        touch(detail);
        accepted.push(id);
      }
      tellChanged(accepted);
      return Promise.resolve(accepted.length);
    },
    onChanged: (listener) => {
      changedListeners.add(listener);
      return () => { changedListeners.delete(listener); };
    },
  };
}
