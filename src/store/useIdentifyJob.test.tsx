// @vitest-environment jsdom
/**
 * The identify job hook follows the backend's events and picks up a job that
 * was already running when it mounted, so the panel survives a close and a
 * reopen.
 */
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

import { __setBackend } from "@/ipc/client";
import { createMockIdentify } from "@/ipc/mock-identify";
import type { Backend, IdentifyBackend } from "@/ipc/types";
import { useIdentifyJob, type IdentifyJob } from "./useIdentifyJob";

const TICK = 10;
let host: HTMLDivElement;
let root: Root;
let identify: IdentifyBackend;
let job: IdentifyJob | undefined;

function Probe() {
  job = useIdentifyJob();
  return null;
}

const mount = () => act(async () => { root.render(<Probe />); await vi.advanceTimersByTimeAsync(0); });

beforeEach(() => {
  vi.useFakeTimers();
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  identify = createMockIdentify({ tickMs: TICK });
  __setBackend({ identify } as unknown as Backend);
  host = document.createElement("div");
  root = createRoot(host);
});
afterEach(() => {
  act(() => root.unmount());
  __setBackend(null);
  vi.useRealTimers();
});

it("starts a job and follows its progress to the end", async () => {
  await mount();
  expect(job?.progress).toBeNull();
  let started = false;
  await act(async () => { started = await job!.start(["1", "2", "3"]); });
  expect(started).toBe(true);
  expect(job?.running).toBe(true);
  expect(job?.progress).toMatchObject({ done: 0, total: 3 });

  await act(async () => { await vi.advanceTimersByTimeAsync(TICK + 1); });
  expect(job?.progress).toMatchObject({ state: "running", done: 1, currentTitle: "Track 2" });

  await act(async () => { await vi.advanceTimersByTimeAsync(TICK * 4); });
  expect(job?.running).toBe(false);
  expect(job?.progress).toMatchObject({ state: "done", done: 3, review: 1, unidentified: 1, identified: 1 });
});

it("reads a job that was already running when it mounted, and cancels it", async () => {
  await identify.start(["1", "2", "3", "4"]);
  await vi.advanceTimersByTimeAsync(TICK + 1);
  await mount();
  expect(job?.progress).toMatchObject({ state: "running", done: 1, total: 4 });

  act(() => job!.cancel());
  await act(async () => { await vi.advanceTimersByTimeAsync(TICK * 6); });
  expect(job?.progress).toMatchObject({ state: "cancelled" });
  expect(job?.progress?.done).toBeLessThan(4);
});

it("says why a start was refused", async () => {
  await mount();
  await act(async () => { await job!.start(["1"]); });
  let second = true;
  await act(async () => { second = await job!.start(["2"]); });
  expect(second).toBe(false);
  expect(job?.error).toMatch(/already running/);
  act(() => job!.clearError());
  expect(job?.error).toBeNull();
});
