// @vitest-environment jsdom
/**
 * The Identify settings pane against the mock identify store: keys are never
 * shown or kept, each save sends only what changed (absent keeps, null
 * clears), the auto-apply threshold stays within 0.92–1, and a key from the
 * environment cannot be cleared here.
 */
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi, type MockInstance } from "vitest";

import { __setBackend } from "@/ipc/client";
import { createMockIdentify } from "@/ipc/mock-identify";
import type { Backend, IdentifyBackend } from "@/ipc/types";
import { IdentifyPane } from "./IdentifyPane";

const SECRET = "s3cret-acoustid-key";
let host: HTMLDivElement;
let root: Root;
let identify: IdentifyBackend;
let save: MockInstance<IdentifyBackend["saveSettings"]>;

const flush = () => act(async () => { await Promise.resolve(); await Promise.resolve(); });
const card = (name: string) => host.querySelector<HTMLElement>(`section[aria-label="${name}"]`)!;
const buttonIn = (scope: HTMLElement, name: string) =>
  [...scope.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === name || b.getAttribute("aria-label") === name);
const click = async (element: HTMLElement | undefined) => {
  expect(element).toBeDefined();
  act(() => { element!.click(); });
  await flush();
};
/** Types into a controlled input the way React hears it: through the native setter. */
const type = (input: HTMLInputElement, value: string) => act(() => {
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(input, value);
  input.dispatchEvent(new Event("input", { bubbles: true }));
});
const submit = async (scope: HTMLElement) => {
  act(() => { scope.querySelector("form")!.requestSubmit(); });
  await flush();
};

async function render(settings: Parameters<typeof createMockIdentify>[0] = {}) {
  identify = createMockIdentify(settings);
  save = vi.spyOn(identify, "saveSettings");
  __setBackend({ identify, openUrl: vi.fn().mockResolvedValue(undefined) } as unknown as Backend);
  act(() => { root.render(<IdentifyPane />); });
  await flush();
}

beforeEach(() => {
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});
afterEach(() => {
  act(() => root.unmount());
  host.remove();
  __setBackend(null);
});

describe("IdentifyPane", () => {
  it("says where each key comes from and never shows or keeps one", async () => {
    await render({ settings: { acoustidKey: null, discogsToken: "settings" } });
    const acoustid = card("AcoustID API key");
    expect(acoustid.textContent).toContain("Not set");
    expect(card("Discogs personal token").textContent).toContain("Set (from settings)");
    // No key box until asked for, and an empty one then.
    expect(host.querySelector('input[type="password"]')).toBeNull();
    await click(buttonIn(acoustid, "Add…"));
    const input = acoustid.querySelector<HTMLInputElement>('input[type="password"]')!;
    expect(input.value).toBe("");
    type(input, SECRET);
    await submit(acoustid);
    expect(save).toHaveBeenCalledWith({ acoustidKey: SECRET });
    expect(acoustid.textContent).toContain("Set (from settings)");
    expect(host.querySelector('input[type="password"]')).toBeNull();
    expect(host.innerHTML).not.toContain(SECRET);
    // Replacing starts empty again.
    await click(buttonIn(acoustid, "Replace…"));
    expect(acoustid.querySelector<HTMLInputElement>('input[type="password"]')?.value).toBe("");
    expect(host.innerHTML).not.toContain(SECRET);
  });

  it("sends only what changed: a cleared key as null, every other member left out", async () => {
    await render({ settings: { acoustidKey: "settings", discogsToken: "settings" } });
    await click(buttonIn(card("AcoustID API key"), "Remove AcoustID API key"));
    expect(save).toHaveBeenLastCalledWith({ acoustidKey: null });
    expect(card("AcoustID API key").textContent).toContain("Not set");

    const genre = card("Genre");
    const replace = [...genre.querySelectorAll<HTMLInputElement>('input[type="radio"]')].find((r) => r.value === "replace")!;
    await click(replace);
    expect(save).toHaveBeenLastCalledWith({ genrePolicy: "replace" });

    const agent = card("MusicBrainz user agent");
    type(agent.querySelector<HTMLInputElement>('input[type="text"]')!, "  rbxport/9 ( me@example.com ) ");
    await submit(agent);
    expect(save).toHaveBeenLastCalledWith({ mbUserAgent: "rbxport/9 ( me@example.com )" });

    for (const [update] of save.mock.calls) expect(Object.keys(update)).toHaveLength(1);
    expect(save.mock.calls.some(([update]) => "discogsToken" in update)).toBe(false);
  });

  it("keeps the auto-apply threshold between 0.92 and 1", async () => {
    await render();
    const auto = card("Auto-apply");
    const threshold = auto.querySelector<HTMLInputElement>('input[type="number"]')!;
    const toggle = auto.querySelector<HTMLInputElement>('input[role="switch"]')!;
    expect(toggle.checked).toBe(false);
    expect(threshold.disabled).toBe(true);
    expect(threshold.value).toBe("0.92");
    await click(toggle);
    expect(save).toHaveBeenLastCalledWith({ autoApply: true });
    expect(threshold.disabled).toBe(false);

    for (const outside of ["0.91", "1.01", ""]) {
      save.mockClear();
      type(threshold, outside);
      act(() => { threshold.dispatchEvent(new FocusEvent("focusout", { bubbles: true })); });
      await flush();
      expect(save).not.toHaveBeenCalled();
      expect(auto.querySelector('[role="alert"]')?.textContent).toBe("Choose a threshold from 0.92 to 1.00.");
    }
    type(threshold, "0.97");
    act(() => { threshold.dispatchEvent(new FocusEvent("focusout", { bubbles: true })); });
    await flush();
    expect(save).toHaveBeenLastCalledWith({ autoApplyThreshold: 0.97 });
    expect(auto.querySelector('[role="alert"]')).toBeNull();
  });

  it("leaves a key from the environment alone, and lets a development-file key be replaced but not cleared", async () => {
    await render({ settings: { discogsToken: "environment", acoustidKey: "devFile", mbUserAgentSource: "environment" } });
    const discogs = card("Discogs personal token");
    expect(discogs.textContent).toContain("Set (from environment)");
    // The variable wins over anything saved, so neither change is offered.
    expect(buttonIn(discogs, "Remove Discogs personal token")).toBeUndefined();
    expect(buttonIn(discogs, "Replace…")).toBeUndefined();
    expect(discogs.textContent).toContain("The environment variable DISCOGS_TOKEN sets this and takes priority");
    expect(discogs.textContent).toContain("Change or remove it outside rbxport.");

    const acoustid = card("AcoustID API key");
    expect(buttonIn(acoustid, "Remove AcoustID API key")).toBeUndefined();
    expect(buttonIn(acoustid, "Replace…")).toBeDefined();
    expect(acoustid.textContent).toContain("development file .env.identify");
    expect(acoustid.textContent).toContain("A value saved here takes priority over it.");

    const agent = card("MusicBrainz user agent");
    expect(agent.querySelector<HTMLInputElement>('input[type="text"]')?.disabled).toBe(true);
    expect(agent.textContent).toContain("The environment variable MB_USER_AGENT sets this");
  });

  it("shows a refused save beside the control, and the settings file", async () => {
    await render();
    expect(host.textContent).toContain("identify\\.env.identify");
    save.mockRejectedValueOnce(Object.assign(new Error("The settings file could not be written."), { kind: "io" }));
    const genre = card("Genre");
    await click([...genre.querySelectorAll<HTMLInputElement>('input[type="radio"]')].find((r) => r.value === "never"));
    expect(genre.querySelector('[role="alert"]')?.textContent).toBe("The settings file could not be written.");
    expect(genre.querySelector<HTMLInputElement>('input[value="fillEmpty"]')?.checked).toBe(true);
  });
});
