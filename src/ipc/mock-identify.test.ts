/**
 * The mock identify job and queue have to keep the Rust job's order and the
 * identify store's rules, or the review UI is built against behaviour the
 * real backend does not have (`src-tauri/src/identify.rs` tests the same
 * things on the other side).
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { createMockBackend } from "./backend-mock";
import { IDENTIFY_MAX_ROWS, createMockIdentify } from "./mock-identify";
import type { IdentifyBackend, IdentifyProgress, IdentifyResultEvent } from "./types";

const TICK = 10;

function record(identify: IdentifyBackend) {
  const events: (["progress", IdentifyProgress] | ["result", IdentifyResultEvent])[] = [];
  const stopProgress = identify.onProgress((p) => events.push(["progress", p]));
  const stopResult = identify.onResult((r) => events.push(["result", r]));
  return { events, stop: () => { stopProgress(); stopResult(); } };
}

/** Runs the timers until the job's final progress. */
async function finish(tracks: number) {
  await vi.advanceTimersByTimeAsync(TICK * (tracks + 2));
}

describe("mock identify", () => {
  beforeEach(() => { vi.useFakeTimers(); });
  afterEach(() => { vi.useRealTimers(); });

  it("reports each track as it starts and ends, then a final progress", async () => {
    const identify = createMockIdentify({ tickMs: TICK });
    const { events } = record(identify);
    const ids = ["1", "2", "3", "4", "5"];
    const jobId = await identify.start(ids);
    await finish(ids.length);

    expect(events).toHaveLength(ids.length * 2 + 1);
    ids.forEach((id, i) => {
      const [startKind, started] = events[i * 2]!;
      const [endKind, ended] = events[i * 2 + 1]!;
      expect(startKind).toBe("progress");
      expect(started).toMatchObject({ jobId, state: "running", done: i, total: 5, currentTitle: `Track ${id}` });
      expect(endKind).toBe("result");
      expect(ended).toMatchObject({ jobId, trackId: id });
    });
    const decisions = events.filter(([kind]) => kind === "result").map(([, r]) => (r as IdentifyResultEvent).decision);
    expect(decisions).toEqual(["review", "unidentified", "autoApply", "review", "failed"]);
    const [, final] = events.at(-1)!;
    expect(final).toMatchObject({
      state: "done", done: 5, identified: 1, review: 2, unidentified: 1, failed: 1, currentTitle: "",
    });
    expect(await identify.progress()).toEqual(final);
  });

  it("files every finished track under its filter", async () => {
    const identify = createMockIdentify({ tickMs: TICK });
    await identify.start(["1", "2", "3", "4", "5"]);
    await finish(5);
    expect((await identify.queue("all", 0, 50)).total).toBe(4);
    expect((await identify.queue("needsReview", 0, 50)).total).toBe(2);
    expect((await identify.queue("unidentified", 0, 50)).total).toBe(1);
    expect((await identify.queue("applied", 0, 50)).total).toBe(0);
    const all = await identify.queue("all", 0, 50);
    expect(all.items.at(-1)?.score, "no score sorts last").toBeNull();
    expect(all.items[0]?.score).toBeGreaterThan(all.items[1]?.score ?? 1);
  });

  it("refuses a second job while one runs and stops between tracks on cancel", async () => {
    const identify = createMockIdentify({ tickMs: TICK });
    const { events } = record(identify);
    const jobId = await identify.start(["1", "2", "3"]);
    await expect(identify.start(["4"])).rejects.toMatchObject({ kind: "readOnly" });
    await expect(identify.start([])).rejects.toMatchObject({ kind: "malformed" });

    await vi.advanceTimersByTimeAsync(TICK + 1);
    expect((await identify.progress())?.done).toBe(1);
    await identify.cancel(jobId + 1);
    await identify.cancel(jobId);
    await finish(3);
    const results = events.filter(([kind]) => kind === "result");
    expect(results).toHaveLength(2);
    expect(events.at(-1)?.[1]).toMatchObject({ state: "cancelled", done: 2, total: 3 });
    expect((await identify.queue("all", 0, 50)).total).toBe(2);
    // The slot is free once the final progress is out.
    await expect(identify.start(["9"])).resolves.toBe(jobId + 1);
  });

  it("windows the queue and caps a page", async () => {
    const identify = createMockIdentify({ tickMs: TICK });
    // Four of every five are stored; the fifth fails.
    const ids = Array.from({ length: 200 }, (_, i) => String(1000 + i));
    await identify.start(ids);
    await finish(ids.length);
    const total = (await identify.queue("all", 0, 1)).total;
    expect(total).toBe(160);
    const page = await identify.queue("all", 158, 10);
    expect(page.items).toHaveLength(2);
    expect((await identify.queue("all", 0, 1000)).items).toHaveLength(IDENTIFY_MAX_ROWS);
  });

  it("changes review state and proposals by the store's rules", async () => {
    const identify = createMockIdentify({ tickMs: TICK });
    await identify.start(["1", "2"]);
    await finish(2);
    const review = (await identify.queue("needsReview", 0, 10)).items[0]!;
    const unknown = (await identify.queue("unidentified", 0, 10)).items[0]!;

    const edited = await identify.edit(review.resultId, [{ field: "label", proposed: "Edited Records" }]);
    expect(edited.changes.find((c) => c.field === "label")?.proposed).toBe("Edited Records");
    await expect(identify.edit(review.resultId, [{ field: "isrc", proposed: "X" }])).rejects.toMatchObject({ kind: "malformed" });
    await expect(identify.edit(review.resultId, [{ field: "artwork", proposed: "X" }])).rejects.toMatchObject({ kind: "malformed" });

    expect((await identify.setState(review.resultId, "accept")).state).toBe("accepted");
    expect((await identify.setState(review.resultId, "pending")).state).toBe("pending");
    expect((await identify.setState(review.resultId, "reject")).state).toBe("rejected");
    await expect(identify.edit(review.resultId, [{ field: "label", proposed: "Y" }])).rejects.toMatchObject({ kind: "malformed" });
    await expect(identify.setState(unknown.resultId, "accept")).rejects.toMatchObject({ kind: "malformed" });
    await expect(identify.setState(999, "accept")).rejects.toMatchObject({ kind: "notFound" });

    const detail = await identify.result(review.resultId);
    expect(detail.item.state).toBe("rejected");
    expect(detail.candidates[0]?.source).toBe("fingerprint");
    expect(detail.artwork?.source).toBe("coverArtArchive");
    await expect(identify.result(999)).rejects.toMatchObject({ kind: "notFound" });
  });

  it("applies ticked fields, undoes them, and accepts above a threshold", async () => {
    const identify = createMockIdentify({ tickMs: TICK });
    const changed: number[][] = [];
    identify.onChanged((event) => changed.push(event.resultIds));
    await identify.start(["1", "2", "3", "4"]);
    await finish(4);
    const [first, second] = (await identify.queue("needsReview", 0, 10)).items;
    expect(first?.trackId).toBe("1");
    expect(first?.undoable).toBe(false);

    const outcomes = await identify.apply([
      { resultId: first!.resultId, fields: ["title", "label"] },
      { resultId: 999, fields: ["title"] },
    ]);
    expect(outcomes.map((o) => o.ok)).toEqual([true, false]);
    expect(changed.at(-1)).toEqual([first!.resultId]);
    const applied = await identify.result(first!.resultId);
    expect(applied.item).toMatchObject({ state: "applied", undoable: true });
    expect(applied.existing.label).toBe("Mock Records");
    expect((await identify.queue("applied", 0, 10)).items.map((i) => i.resultId)).toContain(first!.resultId);
    expect((await identify.apply([{ resultId: first!.resultId, fields: ["title"] }]))[0]?.ok).toBe(false);

    expect((await identify.undo(first!.resultId)).ok).toBe(true);
    const undone = await identify.result(first!.resultId);
    expect(undone.item).toMatchObject({ state: "undone", undoable: false });
    expect(undone.existing.label).toBe("Old Label");
    expect((await identify.undo(first!.resultId)).ok).toBe(false);

    expect(await identify.acceptAbove(0.99)).toBe(0);
    expect(await identify.acceptAbove(0.8)).toBe(1);
    expect((await identify.result(second!.resultId)).item.state).toBe("accepted");
  });

  it("names tracks from the mock library", async () => {
    const backend = createMockBackend({ trackCount: 10 });
    const view = await backend.openView({ source: { kind: "collection" }, sort: "trackNo", descending: false, query: "" });
    const [row] = await backend.fetchRows(view.viewId, 0, 1);
    const { events } = record(backend.identify);
    await backend.identify.start([row!.id]);
    await vi.advanceTimersByTimeAsync(400 * 3);
    expect(events[0]?.[1]).toMatchObject({ currentTitle: row!.title });
  });
});
