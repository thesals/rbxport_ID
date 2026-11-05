/**
 * @vitest-environment jsdom
 *
 * The repeat import's dialog: progress while it runs, with no way to close
 * it part-way, then what was taken and what was kept.
 */
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { DeltaImport } from "@/ipc/types";
import { DeltaImportDialog } from "./DeltaImportDialog";

declare global {
  var IS_REACT_ACT_ENVIRONMENT: boolean;
}

let host: HTMLDivElement;
let root: Root;

const REPORT: DeltaImport = {
  newTracks: [{ id: "9001", name: "TRIODE — New" }],
  updated: [],
  skipped: [{
    track: { id: "3", name: "A — Edited" },
    fields: [
      { field: "title", rbxport: "Edited", rekordbox: "Old" },
      { field: "comment", rbxport: "", rekordbox: "from rekordbox" },
    ],
    linkedByPath: true,
  }],
  pathMatches: [],
  filled: [{ track: { id: "4", name: "B — Four" }, what: ["memoryCues", "analysis"] }],
  unchanged: 12, keptDeleted: 2, duplicates: [],
  playlistsCreated: [], playlistsAppended: [{ name: "Warm up", tracks: 2 }], playlistsSkipped: [],
  historySessions: 0, playsAdded: 0, cues: 1, filesCopied: 2, bytesCopied: 10, problems: [],
  sourceUnchanged: true, log: "/x/import-log/import.txt", toAnalyse: [],
};

beforeEach(() => {
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  // jsdom has no modal dialogs.
  HTMLDialogElement.prototype.showModal = function showModal(this: HTMLDialogElement) { this.open = true; };
  HTMLDialogElement.prototype.close = function close(this: HTMLDialogElement) { this.open = false; };
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

const button = () => host.querySelector<HTMLButtonElement>("button[type=submit]");

describe("DeltaImportDialog", () => {
  it("shows progress and cannot be closed while the import runs", () => {
    const onClose = vi.fn();
    act(() => root.render(
      <DeltaImportDialog progress={{ stage: "copyingFiles", done: 64, total: 1910 }} report={null} error={null} onClose={onClose} />,
    ));
    expect(host.querySelector("[role=status]")?.textContent).toBe("Matching tracks and copying what's missing… 64 of 1,910");
    expect(button()?.disabled).toBe(true);
    act(() => { host.querySelector("dialog")?.dispatchEvent(new Event("cancel", { cancelable: true })); });
    expect(onClose).not.toHaveBeenCalled();
  });

  it("lists what was taken and what was kept, with rekordbox's value beside what was kept", () => {
    const onClose = vi.fn();
    act(() => root.render(<DeltaImportDialog progress={null} report={REPORT} error={null} onClose={onClose} />));
    const text = host.textContent ?? "";
    expect(text).toContain("1 new tracks, 0 updated, 1 kept as edited in rbxport, 1 filled in, 12 unchanged.");
    expect(text).toContain("Track Title: Edited (rekordbox: Old)");
    expect(text).toContain("Comments: (empty) (rekordbox: from rekordbox)");
    expect(text).toContain("matched by file path");
    expect(text).toContain("B — Four: memory cues, beat grid and waveforms");
    expect(text).toContain("Warm up · 2 tracks");
    expect(text).toContain("2 tracks you deleted in rbxport were left deleted.");
    expect(text).toContain("rekordbox's library is unchanged.");
    expect(button()?.disabled).toBe(false);
    act(() => button()?.click());
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("takes rekordbox's tags for the kept tracks you choose, with the differences it showed", () => {
    const onTake = vi.fn();
    act(() => root.render(<DeltaImportDialog progress={null} report={REPORT} error={null} onClose={() => undefined} onTake={onTake} />));
    const take = [...host.querySelectorAll<HTMLButtonElement>("button[type=button]")].find(b => b.textContent?.startsWith("Take"));
    expect(take?.textContent).toBe("Take rekordbox's tags (0)");
    expect(take?.disabled).toBe(true);
    const boxes = host.querySelectorAll<HTMLInputElement>("input[type=checkbox]");
    expect(boxes).toHaveLength(2);
    act(() => boxes[1]?.click());
    expect(take?.textContent).toBe("Take rekordbox's tags (1)");
    expect(boxes[0]?.checked).toBe(true);
    act(() => take?.click());
    expect(onTake).toHaveBeenCalledWith([{ id: "3", fields: REPORT.skipped[0]?.fields }]);
  });

  it("offers no choice while the library is read-only", () => {
    act(() => root.render(<DeltaImportDialog progress={null} report={REPORT} error={null} onClose={() => undefined} />));
    expect(host.querySelectorAll("input[type=checkbox]")).toHaveLength(0);
  });

  it("says when there was nothing new, and shows a failure", () => {
    const empty: DeltaImport = { ...REPORT, newTracks: [], skipped: [], filled: [], playlistsAppended: [], keptDeleted: 0 };
    act(() => root.render(<DeltaImportDialog progress={null} report={empty} error={null} onClose={() => undefined} />));
    expect(host.textContent).toContain("rbxport already had everything new in rekordbox's library.");
    act(() => root.render(
      <DeltaImportDialog progress={null} report={null} error="rekordbox's library was not found on this computer." onClose={() => undefined} />,
    ));
    expect(host.querySelector("[role=alert]")?.textContent).toBe("rekordbox's library was not found on this computer.");
    expect(button()?.disabled).toBe(false);
  });
});
