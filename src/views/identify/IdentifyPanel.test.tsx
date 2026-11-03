/**
 * @vitest-environment jsdom
 *
 * The review queue against the mock identify store: the job's progress and
 * Cancel, the filters, a result's per-field diff with its ticks, Accept
 * writing only the ticked fields, Undo, Edit, and bulk accept above a
 * threshold followed by Apply accepted.
 */
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { __setBackend } from "@/ipc/client";
import { createMockIdentify } from "@/ipc/mock-identify";
import type { Backend, IdentifyBackend } from "@/ipc/types";
import { useIdentifyJob } from "@/store/useIdentifyJob";
import { IdentifyPanel } from "./IdentifyPanel";

declare global {
  var IS_REACT_ACT_ENVIRONMENT: boolean;
}

const TICK = 10;
let host: HTMLDivElement;
let root: Root;
let identify: IdentifyBackend;
const onClose = vi.fn();

function Harness() {
  const job = useIdentifyJob();
  return <IdentifyPanel job={job} onClose={onClose} />;
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
  identify = createMockIdentify({ tickMs: TICK });
  __setBackend({ identify } as unknown as Backend);
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  onClose.mockReset();
});
afterEach(() => {
  act(() => root.unmount());
  host.remove();
  __setBackend(null);
  vi.useRealTimers();
});

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
    expect(host.textContent).toMatch(/auto-apply · \d+ to review/);
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
    expect(host.querySelector("[role=note]")?.textContent).toBe(
      "To see the changes in rekordbox, select the tracks there and choose Reload Tag.");
    expect(button("Accept")?.disabled).toBe(true);
    expect(options()).toHaveLength(1);

    await click(button("Undo"));
    expect(byText("section span", "Undone")).toBeDefined();
    expect(button("Undo")).toBeUndefined();
    expect(button("Accept")?.disabled).toBe(true);
    await click(button("Back to review"));
    expect(byText("section span", "Needs review")).toBeDefined();
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

  it("shows each result's source, and rekordbox's record where it differs from the file", async () => {
    await identifyFive();
    await render();
    const row = options()[0]!;
    const source = row.querySelector<HTMLElement>("[title=Fingerprint]");
    expect(source?.textContent).toBe("Fingerprint");
    await click(row);
    // The mock's rekordbox record has an older label and no comment; its
    // title matches the file, so that row says nothing about rekordbox.
    const rowOf = (label: string) => [...host.querySelectorAll("tr")].find((tr) => tr.querySelector("th")?.textContent === label);
    expect(rowOf("Label")?.textContent).toContain("rekordbox: Older Label");
    expect(rowOf("Sub-genre (added to Comment)")?.textContent).toContain("rekordbox: (empty)");
    expect(rowOf("Track Title")?.textContent).not.toContain("rekordbox:");
    expect(host.textContent).toContain("the rekordbox library is not changed");
  });

  it("accepts above the threshold, then applies what was accepted", async () => {
    await identifyFive();
    const apply = vi.spyOn(identify, "apply");
    await render();
    await click(button("Accept above"));
    expect(host.textContent).toContain("0 results accepted.");
    const number = host.querySelector<HTMLInputElement>('input[aria-label="Accept threshold value"]')!;
    type(number, "0.8");
    await click(button("Accept above"));
    expect(host.textContent).toContain("2 results accepted.");
    await click(button("Apply accepted"));
    // Nothing is written before the counts are confirmed. Of the three
    // accepted (the two above plus the auto-apply match the mock leaves
    // accepted), only the auto-apply match just fills empty fields; the two
    // reviews overwrite a title and add a sub-genre, so they wait.
    expect(apply).not.toHaveBeenCalled();
    expect(host.textContent).toContain(
      "Fill 2 empty fields in 1 results. 2 results with overwrites or a sub-genre are left for individual review.");
    await click(button("Apply"));
    expect(apply).toHaveBeenCalledTimes(1);
    expect(apply.mock.calls[0]?.[0]).toEqual([{ resultId: expect.any(Number) as number, fields: ["label", "catalogNumber"] }]);
    expect(host.textContent).toContain("1 applied. To see the changes in rekordbox");
    await click(byText("[role=tab]", "Applied (undoable)"));
    expect(options()).toHaveLength(1);
    await click(byText("[role=tab]", "All"));
    expect(options().filter((row) => row.textContent?.includes("Accepted"))).toHaveLength(2);
  });

  it("drops a bulk write the user does not confirm", async () => {
    await identifyFive();
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
