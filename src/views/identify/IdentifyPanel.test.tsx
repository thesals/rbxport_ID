/**
 * @vitest-environment jsdom
 *
 * The review queue against the mock identify store: the job's progress and
 * Cancel, the filters, a result's per-field diff with its ticks, Accept
 * writing only the ticked fields, Undo, Edit, and bulk accept above a
 * threshold followed by Apply accepted, and writes deferred while a file is
 * in use: waiting, retried and cancelled. Most of these turn "Write tags to
 * audio files" on first; the last group leaves it off, as it is by default,
 * where nothing may write.
 */
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { __setBackend } from "@/ipc/client";
import { FILE_NOT_WRITTEN_OFF, FILE_WRITES_OFF, createMockIdentify, type MockIdentifyBackend } from "@/ipc/mock-identify";
import type { Backend } from "@/ipc/types";
import { useIdentifyJob } from "@/store/useIdentifyJob";
import { IdentifyPanel } from "./IdentifyPanel";

declare global {
  var IS_REACT_ACT_ENVIRONMENT: boolean;
}

const TICK = 10;
let host: HTMLDivElement;
let root: Root;
let identify: MockIdentifyBackend;
const onClose = vi.fn();

/** What App passes as `libraryLocked`; null unless a test locks it. */
let libraryLocked: string | null = null;

function Harness() {
  const job = useIdentifyJob();
  return <IdentifyPanel job={job} onClose={onClose} libraryLocked={libraryLocked} />;
}

const flush = (ms = 0) => act(async () => { await vi.advanceTimersByTimeAsync(ms); });
const render = async (settle = 200) => {
  act(() => { root.render(<Harness />); });
  await flush(settle);
};
const byText = (selector: string, text: string | RegExp) =>
  [...host.querySelectorAll<HTMLElement>(selector)].find((el) =>
    typeof text === "string" ? el.textContent?.trim() === text : text.test(el.textContent ?? ""));
const button = (name: string) => byText("button", name) as HTMLButtonElement | undefined;
const options = () => [...host.querySelectorAll<HTMLElement>("[role=option]")];
const click = async (element: HTMLElement | undefined) => {
  expect(element).toBeDefined();
  act(() => { element!.click(); });
  await flush(200);
};
/** Types into a controlled input the way React hears it: through the native setter. */
const type = (input: HTMLInputElement, value: string) => act(() => {
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(input, value);
  input.dispatchEvent(new Event("input", { bubbles: true }));
});
const checkbox = (label: string) => host.querySelector<HTMLInputElement>(`input[aria-label="Write ${label}"]`);

beforeEach(() => {
  vi.useFakeTimers();
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  globalThis.ResizeObserver ??= class { observe() {} disconnect() {} unobserve() {} } as unknown as typeof ResizeObserver;
  // Auto-apply on: the job writes the third track of five itself.
  useMock(true);
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  onClose.mockReset();
  libraryLocked = null;
});
afterEach(() => {
  act(() => root.unmount());
  host.remove();
  __setBackend(null);
  vi.useRealTimers();
});

/** A fresh mock store with auto-apply on or off, and file writes on unless asked otherwise. */
function useMock(autoApply: boolean, fileWrites = true) {
  identify = createMockIdentify({ tickMs: TICK, settings: { autoApply, fileWrites } });
  __setBackend({ identify } as unknown as Backend);
}

async function identifyFive() {
  await identify.start(["1", "2", "3", "4", "5"]);
  await flush(TICK * 8);
}

describe("IdentifyPanel", () => {
  it("shows a running job with its counts and cancels it", async () => {
    await identify.start(["1", "2", "3", "4", "5", "6"]);
    await vi.advanceTimersByTimeAsync(TICK * 2 + 1);
    await render(0);
    expect(host.textContent).toMatch(/Identifying \d of 6/);
    expect(host.textContent).toMatch(/applied automatically · \d+ to review/);
    expect(host.querySelector("progress")).not.toBeNull();
    await click(button("Cancel"));
    await flush(TICK * 10);
    expect(host.textContent).toMatch(/Cancelled after \d of 6/);
    expect(button("Cancel")).toBeUndefined();
  });

  it("lists results under each filter and refreshes as tracks finish", async () => {
    await render();
    expect(host.textContent).toContain("No identification has run yet.");
    await act(async () => { await identify.start(["1", "2", "3", "4", "5"]); });
    await flush(TICK * 8 + 200);
    expect(options()).toHaveLength(2);
    expect(options()[0]?.textContent).toContain("81%");
    expect(options()[0]?.textContent).toContain("Needs review");
    await click(byText("[role=tab]", "Unidentified"));
    expect(options()).toHaveLength(1);
    expect(options()[0]?.textContent).toContain("No match");
    await click(byText("[role=tab]", "All"));
    expect(options()).toHaveLength(4);
  });

  it("writes only the ticked fields, then undoes them", async () => {
    await identifyFive();
    const apply = vi.spyOn(identify, "apply");
    await render();
    await click(options()[0]);
    expect(host.textContent).toContain("Sub-genre (added to Comment)");
    expect(host.textContent).toContain("Purchased at Beatport.com; Tech House");
    expect(checkbox("Track Title")?.checked).toBe(true);
    expect(checkbox("Artwork")?.checked).toBe(true);
    await click(checkbox("Label") ?? undefined);
    expect(checkbox("Label")?.checked).toBe(false);

    expect(host.textContent).not.toContain("Reload Tag");
    await click(button("Accept"));
    expect(apply).toHaveBeenCalledWith([{ resultId: expect.any(Number) as number, fields: ["title", "subGenre", "artwork"] }]);
    expect(byText("[role=dialog] section span", "Applied")).toBeDefined();
    // Written to the library and the file: no library-only note.
    expect(host.querySelector("[aria-label='Identification result'] [role=note]")).toBeNull();
    expect(button("Accept")?.disabled).toBe(true);
    expect(options()).toHaveLength(1);

    await click(button("Undo"));
    expect(byText("section span", "Undone")).toBeDefined();
    expect(button("Undo")).toBeUndefined();
    expect(button("Accept")?.disabled).toBe(true);
    await click(button("Back to review"));
    expect(byText("section span", "Needs review")).toBeDefined();
  });

  it("files a result the job wrote under Applied, where it can be undone like any other", async () => {
    await identifyFive();
    await render();
    expect(host.textContent).toContain("1 applied automatically · 2 to review");
    await click(byText("[role=tab]", "Applied (undoable)"));
    expect(options()).toHaveLength(1);
    expect(options()[0]?.textContent).toContain("Applied automatically");
    await click(options()[0]);
    expect(button("Accept")?.disabled).toBe(true);
    expect(host.textContent).not.toContain("Reload Tag");
    await click(button("Undo"));
    expect(byText("section span", "Undone")).toBeDefined();
    expect(options()).toHaveLength(0);
  });

  it("edits a proposed value and rejects", async () => {
    await identifyFive();
    await render();
    await click(options()[0]);
    await click(host.querySelector<HTMLButtonElement>('button[aria-label="Edit Label"]') ?? undefined);
    const input = host.querySelector<HTMLInputElement>('input[aria-label="Proposed Label"]')!;
    type(input, "Edited Records");
    await click(button("Save"));
    expect(host.textContent).toContain("Edited Records");
    await click(button("Reject"));
    expect(byText("section span", "Rejected")).toBeDefined();
  });

  it("shows each result's source, and rbxport's library where it differs from the file", async () => {
    await identifyFive();
    await render();
    const row = options()[0]!;
    const source = row.querySelector<HTMLElement>("[title=Fingerprint]");
    expect(source?.textContent).toBe("Fingerprint");
    await click(row);
    // The mock's library record has an older label and no comment; its
    // title matches the file, so that row says nothing about the library.
    const rowOf = (label: string) => [...host.querySelectorAll("tr")].find((tr) => tr.querySelector("th")?.textContent === label);
    expect(rowOf("Label")?.textContent).toContain("In library: Older Label");
    expect(rowOf("Sub-genre (added to Comment)")?.textContent).toContain("In library: (empty)");
    expect(rowOf("Track Title")?.textContent).not.toContain("In library:");
    expect(host.textContent).toContain("Accepted fields are written to rbxport's library and to the audio file.");
  });

  it("accepts above the threshold, then applies what was accepted", async () => {
    // Off, so the fill-only match waits for review with the others.
    useMock(false);
    await identifyFive();
    const apply = vi.spyOn(identify, "apply");
    await render();
    await click(button("Accept above"));
    expect(host.textContent).toContain("1 result accepted.");
    const number = host.querySelector<HTMLInputElement>('input[aria-label="Accept threshold value"]')!;
    type(number, "0.8");
    await click(button("Accept above"));
    expect(host.textContent).toContain("2 results accepted.");
    await click(button("Apply accepted"));
    // Nothing is written before the counts are confirmed. Of the three
    // accepted (the fill-only match at 0.92, then the two reviews at 0.8),
    // only the fill-only match just fills empty fields; the two reviews
    // overwrite a title and add a sub-genre, so they wait.
    expect(apply).not.toHaveBeenCalled();
    expect(host.textContent).toContain(
      "Fill 2 empty fields in 1 results. 2 results with overwrites or a sub-genre are left for individual review.");
    await click(button("Apply"));
    expect(apply).toHaveBeenCalledTimes(1);
    expect(apply.mock.calls[0]?.[0]).toEqual([{ resultId: expect.any(Number) as number, fields: ["label", "catalogNumber"] }]);
    expect(host.textContent).toContain("1 applied.");
    expect(host.textContent).not.toContain("Reload Tag");
    await click(byText("[role=tab]", "Applied (undoable)"));
    expect(options()).toHaveLength(1);
    await click(byText("[role=tab]", "All"));
    expect(options().filter((row) => row.textContent?.includes("Accepted"))).toHaveLength(2);
  });

  it("defers Accept on a file in use, explains it, and writes it on Retry now once the file is free", async () => {
    await identifyFive();
    await render();
    const row = options()[0]!;
    const name = row.querySelector("span")?.textContent ?? "";
    await click(row);
    const path = host.querySelector("[role=dialog] section header p")?.textContent ?? "";
    identify.setFileInUse(true, path);
    await click(button("Accept"));
    // A notice, not an error.
    expect(host.querySelector("[aria-label='Identification result'] [role=alert]")).toBeNull();
    expect(byText("[aria-label='Identification result'] [role=status]", "1 waiting for files in use.")).toBeDefined();
    const waiting = host.querySelector<HTMLElement>("section[aria-label='Waiting for file']");
    expect(waiting?.textContent).toContain("open in another program, such as a player");
    expect(waiting?.textContent).toContain("the next time rbxport starts or closes, or when you choose Retry now");
    expect(waiting?.textContent).toContain("Nothing is written if the file changes in the meantime.");
    expect(button("Accept")?.disabled).toBe(true);
    expect(button("Reject")?.disabled).toBe(true);
    expect(checkbox("Track Title")?.checked).toBe(true);
    expect(checkbox("Track Title")?.disabled).toBe(true);

    await click(byText("[role=tab]", "Waiting for file"));
    expect(options()).toHaveLength(1);
    expect(options()[0]?.textContent).toContain("Waiting — file in use");
    await click(options()[0]);

    // Still in use: it keeps waiting.
    await click(button("Retry now"));
    expect(byText("[aria-label='Identification result'] [role=status]", "1 waiting for files in use.")).toBeDefined();
    expect(options()).toHaveLength(1);

    identify.setFileInUse(false, path);
    await click(button("Retry now"));
    expect(host.querySelector("section[aria-label='Waiting for file']")).toBeNull();
    expect(byText("[role=dialog] section span", "Applied")).toBeDefined();
    expect(options()).toHaveLength(0);
    await click(byText("[role=tab]", "Applied (undoable)"));
    expect(name).not.toBe("");
    expect(options().some((option) => option.querySelector("span")?.textContent === name)).toBe(true);
  });

  it("cancels a waiting write without writing it", async () => {
    await identifyFive();
    const cancel = vi.spyOn(identify, "cancelWaiting");
    await render();
    identify.setFileInUse(true);
    await click(options()[0]);
    await click(button("Accept"));
    await click(button("Cancel write"));
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(host.querySelector("section[aria-label='Waiting for file']")).toBeNull();
    // Back to review, writable again.
    expect(byText("[role=dialog] section header span", "Needs review")).toBeDefined();
    expect(button("Accept")?.disabled).toBe(false);
    expect(options().some((row) => row.textContent?.includes("Needs review"))).toBe(true);
    identify.setFileInUse(false);
    await click(byText("[role=tab]", "Waiting for file"));
    expect(options()).toHaveLength(0);
    await click(byText("[role=tab]", "Applied (undoable)"));
    expect(options().filter((row) => row.textContent?.includes("Applied"))).toHaveLength(1);
  });

  it("reports a bulk write to files in use as waiting, not failed", async () => {
    useMock(false);
    await identifyFive();
    await act(async () => { await identify.acceptAbove(0.92); });
    identify.setFileInUse(true);
    await render();
    await click(button("Apply accepted"));
    await click(button("Apply"));
    expect(byText("[role=status]", "1 waiting for files in use.")).toBeDefined();
    expect(host.querySelector("[role=alert]")).toBeNull();
    await click(byText("[role=tab]", "Waiting for file"));
    expect(options()).toHaveLength(1);
    // Already waiting: a second Apply accepted does not plan it again.
    await click(button("Apply accepted"));
    expect(host.textContent).toContain("No accepted results to apply.");
  });

  it("drops a bulk write the user does not confirm", async () => {
    useMock(false);
    await identifyFive();
    await act(async () => { await identify.acceptAbove(0.92); });
    const apply = vi.spyOn(identify, "apply");
    await render();
    await click(button("Apply accepted"));
    expect(host.textContent).toContain("Fill 2 empty fields in 1 results.");
    const cancels = [...host.querySelectorAll<HTMLButtonElement>("[role=group] button")].filter((b) => b.textContent === "Cancel");
    await click(cancels[0]);
    expect(host.textContent).not.toContain("Fill 2 empty fields");
    expect(apply).not.toHaveBeenCalled();
  });

  it("moves through the list with the arrows and closes on Escape", async () => {
    await identifyFive();
    await render();
    const list = host.querySelector<HTMLElement>("[role=listbox]")!;
    act(() => { list.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true })); });
    await flush(200);
    expect(options()[0]?.getAttribute("aria-selected")).toBe("true");
    act(() => { list.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true })); });
    expect(options()[1]?.getAttribute("aria-selected")).toBe("true");
    expect(list.getAttribute("aria-activedescendant")).toBe("identify-row-1");
    act(() => { list.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })); });
    expect(onClose).toHaveBeenCalled();
  });
});

describe("IdentifyPanel with writing to audio files off", () => {
  const notice = "Writing to audio files is off. Accepted results are written to rbxport's library only.";

  it("says so, writes rbxport's library only and keeps review changes", async () => {
    useMock(false, false);
    await identifyFive();
    await render();
    expect(host.querySelector("[role=note]#identify-writes-off")?.textContent).toBe(notice);
    expect(button("Apply accepted")?.disabled).toBe(false);
    expect(button("Accept above")?.disabled).toBe(false);
    await click(options()[0]);
    expect(host.textContent).toContain("Writing to audio files is off, so the files are left as they are.");
    // Edit, Reject and Back to review change only the identify store.
    await click(host.querySelector<HTMLButtonElement>('button[aria-label="Edit Label"]') ?? undefined);
    type(host.querySelector<HTMLInputElement>('input[aria-label="Proposed Label"]')!, "Edited Records");
    await click(button("Save"));
    expect(host.textContent).toContain("Edited Records");
    await click(button("Reject"));
    expect(byText("section span", "Rejected")).toBeDefined();
    await click(button("Back to review"));
    expect(byText("section span", "Needs review")).toBeDefined();

    expect(button("Accept")?.disabled).toBe(false);
    await click(button("Accept"));
    expect(byText("[aria-label='Identification result'] [role=status]", FILE_NOT_WRITTEN_OFF)).toBeDefined();
    expect(byText("[role=dialog] section span", "Applied")).toBeDefined();
    expect(byText("[role=note]", "Written to rbxport's library only; the audio file was not changed.")).toBeDefined();
    await click(button("Undo"));
    expect(byText("section span", "Undone")).toBeDefined();
  });

  it("is enforced by the store: a field only the file keeps is refused and no file is written", async () => {
    useMock(false, false);
    await identifyFive();
    const [first] = (await identify.queue("needsReview", 0, 10)).items;
    expect(first).toBeDefined();
    const before = await identify.result(first!.resultId);
    expect(await identify.apply([{ resultId: first!.resultId, fields: ["catalogNumber"] }])).toEqual([
      { resultId: first!.resultId, ok: false, deferred: false, error: FILE_WRITES_OFF, note: null },
    ]);
    expect(FILE_WRITES_OFF).toBe("Writing to audio files is turned off. Turn on \"Write tags to audio files\" in Preferences → Identify to write.");
    expect((await identify.result(first!.resultId)).item.state).toBe("pending");

    const fields = first!.changes.map((c) => c.field);
    expect(await identify.apply([{ resultId: first!.resultId, fields }])).toEqual([
      { resultId: first!.resultId, ok: true, deferred: false, error: null, note: FILE_NOT_WRITTEN_OFF },
    ]);
    const after = await identify.result(first!.resultId);
    expect(after.item).toMatchObject({ state: "applied", inLibrary: true, inFile: false });
    expect(after.existing).toEqual(before.existing);
    expect(after.item.changes.map((c) => c.current)).toEqual(first!.changes.map((c) => c.current));
    expect(await identify.undo(first!.resultId)).toMatchObject({ ok: true });
  });

  it("keeps a waiting write waiting while off, and refuses its retry", async () => {
    useMock(false, true);
    await identifyFive();
    identify.setFileInUse(true);
    const [first] = (await identify.queue("needsReview", 0, 10)).items;
    expect((await identify.apply([{ resultId: first!.resultId, fields: first!.changes.map((c) => c.field) }]))[0]?.deferred).toBe(true);
    await identify.saveSettings({ fileWrites: false });
    identify.setFileInUse(false);
    await render();
    await click(byText("[role=tab]", "Waiting for file"));
    await click(options()[0]);
    expect(button("Retry now")?.disabled).toBe(true);
    expect(button("Retry now")?.title).toBe(notice);
    expect(button("Cancel write")?.disabled).toBe(false);
    expect(await identify.retryWaiting()).toEqual([{ resultId: first!.resultId, ok: false, deferred: false, error: FILE_WRITES_OFF, note: null }]);
    expect((await identify.queue("waiting", 0, 10)).total).toBe(1);
  });

  it("auto-applies a match to rbxport's library only", async () => {
    useMock(true, false);
    await identifyFive();
    await render();
    expect(host.textContent).toContain("1 applied automatically · 2 to review");
    const [applied] = (await identify.queue("applied", 0, 10)).items;
    expect(applied).toMatchObject({ state: "autoApplied", inLibrary: true, inFile: false });
  });

  it("reads the setting again when the window gets focus back", async () => {
    useMock(false, false);
    await identifyFive();
    await render();
    await click(options()[0]);
    expect(host.querySelector("#identify-writes-off")).not.toBeNull();
    // Saved in the Preferences window, which emits no event.
    await identify.saveSettings({ fileWrites: true });
    act(() => { window.dispatchEvent(new Event("focus")); });
    await flush(200);
    expect(host.querySelector("#identify-writes-off")).toBeNull();
    expect(host.textContent).toContain("Accepted fields are written to rbxport's library and to the audio file.");
  });
});

describe("IdentifyPanel with rbxport's library locked", () => {
  it("disables every write with the reason, and keeps review changes", async () => {
    libraryLocked = "Editing is locked by Library Protection. Turn it off in Preferences to edit.";
    await identifyFive();
    const apply = vi.spyOn(identify, "apply");
    await render();
    expect(byText("[role=note]", libraryLocked)).toBeDefined();
    expect(button("Apply accepted")?.disabled).toBe(true);
    expect(button("Apply accepted")?.title).toBe(libraryLocked);
    await click(options()[0]);
    expect(button("Accept")?.disabled).toBe(true);
    expect(button("Accept")?.title).toBe(libraryLocked);
    expect(button("Reject")?.disabled).toBe(false);
    await click(button("Accept above"));
    expect(apply).not.toHaveBeenCalled();

    // A result the job applied cannot be undone while locked either.
    await click(byText("[role=tab]", "Applied (undoable)"));
    await click(options()[0]);
    expect(button("Undo")?.disabled).toBe(true);
    expect(button("Undo")?.title).toBe(libraryLocked);
  });
});
