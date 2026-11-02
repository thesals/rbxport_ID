import { describe, expect, it } from "vitest";

import type { IdentifyFieldChange } from "@/ipc/types";
import { bulkPlan, canApply, defaultFields, formatScore, trackName } from "./identifyText";

const item = (resultId: number, changes: IdentifyFieldChange[]) => ({ resultId, changes });

describe("bulkPlan", () => {
  it("fills only empty fields and artwork for a file without any", () => {
    const plan = bulkPlan([
      item(1, [
        { field: "label", current: "", proposed: "Mock Records" },
        { field: "isrc", current: " ", proposed: "GBXXX1400001" },
        { field: "artwork", current: "none", proposed: "JPEG" },
        // Already what the file says: nothing to write, nothing to review.
        { field: "artist", current: "Same", proposed: "Same" },
      ]),
    ], () => false);
    expect(plan).toEqual({ items: [{ resultId: 1, fields: ["label", "isrc", "artwork"] }], fields: 3, leftForReview: 0 });
  });

  it("leaves a result with an overwrite, a sub-genre or a replaced cover for its own review", () => {
    const plan = bulkPlan([
      item(1, [{ field: "label", current: "", proposed: "A" }, { field: "title", current: "T", proposed: "T (Extended Mix)" }]),
      item(2, [{ field: "label", current: "", proposed: "A" }, { field: "subGenre", current: "", proposed: "Tech House" }]),
      item(3, [{ field: "artwork", current: "", proposed: "JPEG" }]),
      item(4, [{ field: "year", current: "", proposed: "2014" }]),
    ], (id) => id === 3);
    expect(plan.items).toEqual([{ resultId: 4, fields: ["year"] }]);
    expect(plan.fields).toBe(1);
    expect(plan.leftForReview).toBe(3);
  });
});

describe("identify text", () => {
  it("ticks every field that would change, writes only pending or accepted, and names tracks", () => {
    expect(defaultFields(item(1, [
      { field: "label", current: "", proposed: "A" },
      { field: "genre", current: "House", proposed: "House" },
      { field: "album", current: "X", proposed: "" },
    ]))).toEqual(["label"]);
    expect(["pending", "accepted", "applied", "undone", "rejected"].map((s) => canApply(s as never)))
      .toEqual([true, true, false, false, false]);
    expect(formatScore(0.917)).toBe("92%");
    expect(formatScore(null)).toBe("–");
    expect(trackName({ artist: "", title: "", path: "C:\\Music\\a.mp3" })).toBe("a.mp3");
    expect(trackName({ artist: "A", title: "B", path: "" })).toBe("A – B");
  });
});
