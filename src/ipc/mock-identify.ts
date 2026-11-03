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
  AppErrorDto, IdentifyBackend, IdentifyCandidate, IdentifyChangedEvent, IdentifyDecision, IdentifyDetail, IdentifyField, IdentifyFieldChange,
  IdentifyFilter, IdentifyProgress, IdentifyQueueItem, IdentifyResultEvent, IdentifyReviewAction, IdentifyReviewState,
  IdentifyKeySource, IdentifySettings, IdentifyWriteOutcome,
} from "./types";

/** The most rows one queue call answers, as `MAX_ROWS` is in Rust. */
export const IDENTIFY_MAX_ROWS = 128;

export interface MockIdentifyOptions {
  /** Time a track takes; tests shorten it. */
  tickMs?: number;
  /** The library's title and artist for a track id, when it has one. */
  describe?: (trackId: string) => { title: string; artist: string } | undefined;
  /** Where each key comes from at first; the settings file holds the rest. */
  settings?: Partial<IdentifySettings>;
  /** Make the job's automatic writes fail, as a locked file would. */
  refuseAutoWrite?: boolean;
}

/** The mock store, with a hand on which files another program holds open. */
export interface MockIdentifyBackend extends IdentifyBackend {
  /**
   * Marks a file in use (a player holding it), or every file when no path is
   * given; a write to it is then deferred until it is released and retried.
   */
  setFileInUse(inUse: boolean, path?: string): void;
}

/**
 * Where a value can be found, by layer. The backend resolves environment
 * first, then the data folder's `.env.identify` (what a save writes), then
 * a development checkout's `.env.identify` (`Sources::resolve`).
 */
interface Layers<T> {
  environment: T | null;
  saved: T | null;
  devFile: T | null;
}

function resolve<T>(layers: Layers<T>): [T | null, IdentifyKeySource] {
  if (layers.environment !== null) return [layers.environment, "environment"];
  if (layers.saved !== null) return [layers.saved, "settings"];
  if (layers.devFile !== null) return [layers.devFile, "devFile"];
  return [null, null];
}

/** Layers holding one value at the reported source. */
function layersAt<T>(source: IdentifyKeySource, value: T): Layers<T> {
  return {
    environment: source === "environment" ? value : null,
    saved: source === "settings" ? value : null,
    devFile: source === "devFile" ? value : null,
  };
}

type Outcome = IdentifyDecision | "failed";
/**
 * What the tracks of a job come out as, in turn. `autoApply` is a
 * fingerprint match that only fills empty fields: an auto-apply decision,
 * written at once, when auto-apply is on and its score reaches the
 * threshold, and a review otherwise (`decide` in Rust).
 */
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

const inFilter = (filter: IdentifyFilter, { state, waiting }: IdentifyQueueItem): boolean => {
  switch (filter) {
    case "unidentified": return state === "unidentified";
    case "needsReview": return state === "pending";
    case "waiting": return waiting !== null;
    case "applied": return state === "applied" || state === "autoApplied";
    case "all": return true;
  }
};

/** The store's refusal of a review change while a write waits for its file. */
export const WAITING_READ_ONLY = "This result is waiting to be written once the file is closed in the other program. Cancel the waiting write to change its review.";

/**
 * What every audio-file write answers while "Write tags to audio files" is
 * off, in the backend's words.
 */
export const FILE_WRITES_OFF = "Writing to audio files is turned off. Turn on \"Write tags to audio files\" in Preferences → Identify to write.";

/** The warning a job leaves on a match it would have applied but for the lock. */
export const AUTO_APPLY_LOCKED = "Not applied automatically: writing to audio files is turned off. It waits for review.";

/** What a write to a file in use answers: not a failure, a write for later. */
export const DEFERRED_MESSAGE = "The file is open in another program. It will be written when the file is free.";

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

export function createMockIdentify(options: MockIdentifyOptions = {}): MockIdentifyBackend {
  const tickMs = options.tickMs ?? 400;
  /** Files another program holds open; `allInUse` holds every one. */
  const inUse = new Set<string>();
  let allInUse = false;
  const busy = (path: string) => allInUse || inUse.has(path);
  /** Waiting writes that were the job's own, so a retry records them as autoApplied. */
  const waitingAuto = new Set<number>();
  const progressListeners = new Set<(progress: IdentifyProgress) => void>();
  const resultListeners = new Set<(result: IdentifyResultEvent) => void>();
  const changedListeners = new Set<(event: IdentifyChangedEvent) => void>();
  /** The values an apply replaced, by result, for its undo. */
  const snapshots = new Map<number, IdentifyFieldChange[]>();
  /** Every result, current or replaced, by id. */
  const results = new Map<number, IdentifyDetail>();
  /** Each file's current result. */
  const current = new Map<string, number>();
  let nextResult = 1;
  let nextJob = 1;
  let running: { jobId: number; cancelled: boolean } | null = null;
  let last: IdentifyProgress | null = null;
  // Settings: a key is only ever known by where it came from, so its layers
  // hold `true` for "a key is here".
  let settings: IdentifySettings = {
    // Off until the user turns it on: nothing is written to an audio file.
    fileWrites: false,
    acoustidKey: "settings", discogsToken: "environment",
    mbUserAgent: "rbxport/1.2.0 ( dj@example.com )", mbUserAgentSource: "settings",
    genrePolicy: "fillEmpty", autoApply: false, autoApplyThreshold: 0.9,
    // The file a save writes: the data folder's `.env.identify`.
    settingsFile: "C:\\Users\\dj\\AppData\\Roaming\\rbxport\\identify\\.env.identify",
    ...options.settings,
  };
  const keys = {
    acoustidKey: layersAt(settings.acoustidKey, true),
    discogsToken: layersAt(settings.discogsToken, true),
  };
  const agentLayers = layersAt(settings.mbUserAgentSource, settings.mbUserAgent);

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
  const save = (
    trackId: string, path: string, artist: string, title: string, decision: IdentifyDecision,
    /** A match that only fills empty fields. */
    fills: boolean,
  ): number => {
    const id = nextResult++;
    const score = decision === "unidentified" ? 0.31 : fills ? 0.97 : 0.81;
    const overwrites = decision !== "unidentified" && !fills;
    const changes: IdentifyFieldChange[] =
      decision === "unidentified"
        ? []
        : fills
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
    const candidates = decision === "unidentified" ? [] : [candidate(artist, `${title} (Extended Mix)`, score)];
    // An auto-apply decision is accepted until the job writes it.
    const state: IdentifyReviewState =
      decision === "unidentified" ? "unidentified" : decision === "autoApply" ? "accepted" : "pending";
    results.set(id, {
      item: {
        resultId: id, trackId, path, state, decision, score: decision === "unidentified" ? null : score,
        artist, title, changes, updatedUnix: Math.floor(Date.now() / 1000), undoable: false,
        source: candidates[0]?.source ?? null, waiting: null,
      },
      existing: {
        artist, title, album: "", label: overwrites ? "Old Label" : "", year: null, isrc: "", genre: "",
        catalogNumber: "", comment: overwrites ? COMMENT : "", hasArtwork: !overwrites,
      },
      candidates,
      enrichment: null,
      artwork: overwrites
        ? { source: "coverArtArchive", url: "https://coverartarchive.org/release/c3132648-11c5-4476-b152-979253d9241a/front-500", sha256: "0".repeat(64), mime: "image/jpeg", bytes: 48_213 }
        : null,
      warnings: decision === "unidentified" ? ["AcoustID: no match"] : [],
      writeInProgress: false,
      // As imported from rekordbox: its record of a reviewed track lags the
      // file, with an older label and no comment.
      rekordbox: {
        trackId, importedAt: new Date().toISOString(), title, artist, album: "",
        label: overwrites ? "Older Label" : "", genre: "", year: "", comment: "", isrc: "",
      },
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
        const slot = OUTCOMES[index % OUTCOMES.length] ?? "failed";
        const fills = slot === "autoApply";
        // The decision as `decide` makes it: auto-apply only when switched on.
        const qualifies = fills && settings.autoApply && 0.97 >= settings.autoApplyThreshold;
        // A match that qualifies is still only reviewed while file writes are off.
        const locked = qualifies && !settings.fileWrites;
        const outcome: Outcome = fills && (!qualifies || locked) ? "review" : slot;
        let counted: "identified" | "review" | "unidentified" | "failed" =
          outcome === "autoApply" ? "identified" : outcome === "failed" ? "failed" : outcome;
        const result: IdentifyResultEvent = {
          jobId, trackId, resultId: null, path, decision: outcome, score: null, error: null,
        };
        const held = current.get(path);
        const heldItem = held === undefined ? undefined : results.get(held)?.item;
        const heldState = heldItem?.state;
        if (outcome === "failed") {
          result.error = "The file could not be found.";
        } else if (heldState === "applied" || heldState === "autoApplied") {
          result.decision = "failed";
          counted = "failed";
          result.error = "identify store: this track has an applied result; undo it before identifying again";
        } else if (heldItem?.waiting) {
          result.decision = "failed";
          counted = "failed";
          result.error = "identify store: this track has a write waiting for the file to be closed; cancel it before identifying again";
        } else {
          const id = save(trackId, path, artist, title, outcome, fills);
          if (locked) results.get(id)?.warnings.push(AUTO_APPLY_LOCKED);
          result.resultId = id;
          result.score = results.get(id)?.item.score ?? null;
          if (outcome === "autoApply") {
            // Written straight away; a refused write leaves it for review.
            const detail = results.get(id);
            const fields = detail ? detail.item.changes.map((c) => c.field) : [];
            const written: IdentifyWriteOutcome = options.refuseAutoWrite
              ? { resultId: id, ok: false, error: "The file is open in another program.", deferred: false }
              : write(id, fields, true);
            // A deferred write stays accepted, waiting for the file; it is not written yet.
            if (written.deferred) counted = "review";
            else if (!written.ok && detail) {
              detail.item.state = "pending";
              counted = "review";
            }
            tellChanged([id]);
          }
        }
        progress[counted] += 1;
        progress.done += 1;
        tellResult(result);
        step(index + 1);
      }, tickMs);
    };
    step(0);
  };

  /**
   * Writes a result's fields as the store does, snapshotting what they
   * replace. `auto` is the job's own write of an auto-apply decision, which
   * the store records as autoApplied; a person's is applied. A file in use
   * defers the write: the result is accepted and waits, with its fields,
   * for a retry.
   */
  function write(resultId: number, fields: readonly IdentifyField[], auto: boolean): IdentifyWriteOutcome {
    const detail = changeable(resultId);
    const refuse = (error: string): IdentifyWriteOutcome => ({ resultId, ok: false, error, deferred: false });
    if (!settings.fileWrites) return refuse(FILE_WRITES_OFF);
    if (!detail) return refuse("That identification result is no longer in the store.");
    if (detail.item.state !== "pending" && detail.item.state !== "accepted") {
      return refuse(`A ${detail.item.state} result cannot be applied.`);
    }
    if (fields.length === 0) return refuse("Choose at least one field to write.");
    const picked = detail.item.changes.filter((c) => fields.includes(c.field));
    if (picked.length !== new Set(fields).size) return refuse("That change could not be made to this result.");
    if (busy(detail.item.path)) {
      detail.item.state = "accepted";
      detail.item.waiting = {
        sinceUnix: detail.item.waiting?.sinceUnix ?? Math.floor(Date.now() / 1000),
        reason: "The file is open in another program.",
        fields: [...new Set(fields)],
      };
      if (auto) waitingAuto.add(resultId);
      else waitingAuto.delete(resultId);
      touch(detail);
      return { resultId, ok: false, error: DEFERRED_MESSAGE, deferred: true };
    }
    detail.item.waiting = null;
    waitingAuto.delete(resultId);
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
    detail.item.state = auto ? "autoApplied" : "applied";
    detail.item.undoable = true;
    touch(detail);
    return { resultId, ok: true, error: null, deferred: false };
  }

  /**
   * Tries every waiting write, as the backend does at start and exit, at
   * each job and apply, and when asked. A file still in use stays waiting.
   * Results that changed are told on `changed`.
   */
  const retryAll = (skip: ReadonlySet<number> = new Set()): IdentifyWriteOutcome[] => {
    const outcomes: IdentifyWriteOutcome[] = [];
    // Locked: every waiting write is refused and keeps waiting, for when writes are turned on.
    if (!settings.fileWrites) {
      for (const id of current.values()) {
        if (!skip.has(id) && results.get(id)?.item.waiting) {
          outcomes.push({ resultId: id, ok: false, error: FILE_WRITES_OFF, deferred: false });
        }
      }
      return outcomes;
    }
    const changed: number[] = [];
    for (const id of current.values()) {
      if (skip.has(id)) continue;
      const waiting = results.get(id)?.item.waiting;
      if (!waiting) continue;
      const outcome = write(id, waiting.fields, waitingAuto.has(id));
      outcomes.push(outcome);
      if (outcome.deferred) continue;
      // Written, or refused for good: either way it waits no longer.
      const item = results.get(id)?.item;
      if (item) item.waiting = null;
      waitingAuto.delete(id);
      changed.push(id);
    }
    tellChanged(changed);
    return outcomes;
  };

  /** A current result with no write open, or the store's refusal. */
  const changeable = (resultId: number): IdentifyDetail | null => {
    const detail = results.get(resultId);
    return detail && current.get(detail.item.path) === resultId ? detail : null;
  };
  const touchless = (detail: IdentifyDetail): IdentifyQueueItem => ({
    ...detail.item,
    changes: detail.item.changes.map((c) => ({ ...c })),
    waiting: detail.item.waiting ? { ...detail.item.waiting, fields: [...detail.item.waiting.fields] } : null,
  });
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
      retryAll();
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
        .filter((item): item is IdentifyQueueItem => item !== undefined && inFilter(filter, item))
        // Waiting writes newest first, as the store lists them; the rest by score.
        .sort((a, b) => filter === "waiting"
          ? (b.waiting?.sinceUnix ?? 0) - (a.waiting?.sinceUnix ?? 0) || b.resultId - a.resultId
          : (a.score === null ? 1 : 0) - (b.score === null ? 1 : 0)
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
      if (detail.item.waiting) return fail("readOnly", WAITING_READ_ONLY);
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
      if (detail.item.waiting) return fail("readOnly", WAITING_READ_ONLY);
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
      // Waiting writes are tried first, except those this apply replaces.
      retryAll(new Set(items.map((item) => item.resultId)));
      const outcomes = items.map(({ resultId, fields }) => write(resultId, fields, false));
      tellChanged(outcomes.filter((o) => o.ok || o.deferred).map((o) => o.resultId));
      return Promise.resolve(outcomes);
    },
    undo: (resultId) => {
      if (!settings.fileWrites) return Promise.resolve({ resultId, ok: false, error: FILE_WRITES_OFF, deferred: false });
      const detail = changeable(resultId);
      const snapshot = snapshots.get(resultId);
      if (!detail || !detail.item.undoable || !snapshot) {
        return Promise.resolve({ resultId, ok: false, error: "There is nothing to undo for this result.", deferred: false });
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
      return Promise.resolve({ resultId, ok: true, error: null, deferred: false });
    },
    retryWaiting: () => Promise.resolve(retryAll()),
    cancelWaiting: (resultId) => {
      const detail = changeable(resultId);
      if (!detail) return fail("notFound", "That identification result is no longer in the store.");
      if (!detail.item.waiting) return fail("malformed", "This result has no write waiting.");
      // Nothing was written: the result goes back to review.
      detail.item.waiting = null;
      detail.item.state = "pending";
      waitingAuto.delete(resultId);
      const item = touch(detail);
      tellChanged([resultId]);
      return Promise.resolve(item);
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
    settings: () => Promise.resolve({ ...settings }),
    saveSettings: (update) => {
      const threshold = update.autoApplyThreshold;
      if (threshold !== undefined && !(Number.isFinite(threshold) && threshold >= 0.9 && threshold <= 1)) {
        return fail("malformed", "The auto-apply threshold must be between 0.90 and 1.");
      }
      const next = { ...settings };
      // A save writes the data folder's file; an environment variable still wins.
      for (const key of ["acoustidKey", "discogsToken"] as const) {
        const value = update[key];
        if (value === undefined) continue;
        keys[key].saved = value === null || value.trim() === "" ? null : true;
        next[key] = resolve(keys[key])[1];
      }
      if (update.mbUserAgent !== undefined) {
        const agent = update.mbUserAgent.trim();
        agentLayers.saved = agent === "" ? null : agent;
        const [value, source] = resolve(agentLayers);
        next.mbUserAgent = value ?? "";
        next.mbUserAgentSource = source;
      }
      if (update.fileWrites !== undefined) next.fileWrites = update.fileWrites;
      if (update.genrePolicy !== undefined) next.genrePolicy = update.genrePolicy;
      if (update.autoApply !== undefined) next.autoApply = update.autoApply;
      if (threshold !== undefined) next.autoApplyThreshold = threshold;
      settings = next;
      return Promise.resolve({ ...settings });
    },
    setFileInUse: (held, path) => {
      if (path === undefined) {
        allInUse = held;
        if (!held) inUse.clear();
      } else if (held) {
        inUse.add(path);
      } else {
        inUse.delete(path);
      }
    },
  };
}
