/** @vitest-environment jsdom */
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

import { StatusBar } from "./StatusBar";

declare global {
  var IS_REACT_ACT_ENVIRONMENT: boolean;
}

let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

it("opens the application log when the name and version are right-clicked", () => {
  const onOpenLog = vi.fn();
  act(() => root.render(<StatusBar version="1.2.3" onOpenLog={onOpenLog} />));

  const logo = host.querySelector<HTMLElement>("footer > span");
  expect(logo?.textContent).toBe("rbxport 1.2.3");
  const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
  act(() => {
    logo?.dispatchEvent(event);
  });

  expect(event.defaultPrevented).toBe(true);
  expect(onOpenLog).toHaveBeenCalledOnce();
});

it("names read-only as a library state", () => {
  act(() => root.render(<StatusBar readOnly />));

  const badge = host.querySelector<HTMLButtonElement>("footer > button");
  expect(badge?.textContent).toBe("Library read-only");
  expect(badge?.title).toContain("This library is open read-only");
});

it("follows a running identify job, opens its queue and stops it", () => {
  const onOpenIdentify = vi.fn();
  const onCancelIdentify = vi.fn();
  act(() => root.render(<StatusBar identifyProgress={{ done: 3, total: 12, currentTitle: "Strings of Life" }}
    onOpenIdentify={onOpenIdentify} onCancelIdentify={onCancelIdentify} />));

  const buttons = Array.from(host.querySelectorAll("button"));
  const readout = buttons.find(candidate => candidate.textContent === "Identifying 3 of 12");
  expect(readout?.title).toBe("Strings of Life");
  expect(host.querySelector<HTMLProgressElement>('progress[aria-label="Identification progress"]')?.value).toBe(25);
  act(() => readout?.click());
  act(() => buttons.find(candidate => candidate.getAttribute("aria-label") === "Stop identifying")?.click());

  expect(onOpenIdentify).toHaveBeenCalledOnce();
  expect(onCancelIdentify).toHaveBeenCalledOnce();
});

it("offers the support action when it is available", () => {
  const onSupport = vi.fn();
  act(() => root.render(<StatusBar onSupport={onSupport} />));

  const button = Array.from(host.querySelectorAll("button")).find(candidate => candidate.textContent?.includes("Support rbxport"));
  act(() => button?.click());

  expect(button).toBeDefined();
  expect(onSupport).toHaveBeenCalledOnce();
});
