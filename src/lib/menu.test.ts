import { describe, expect, it } from "vitest";

import { menuCommand, resolveMenu } from "./menu";

describe("menu", () => {
  it("ignores an id that is not ours", () => {
    expect(menuCommand("quit")).toBeNull();
    expect(resolveMenu("quit", false)).toBeNull();
  });

  it("runs an action that only reads", () => {
    expect(resolveMenu("settings", true)).toEqual({ action: "settings" });
  });

  it("refuses a write while the library is read-only", () => {
    expect(resolveMenu("import", true)).toEqual({
      refused: "This library is open read-only, so it can't be edited.",
    });
    expect(resolveMenu("import", false)).toEqual({ action: "import" });
  });

  it("treats importing a folder as the write it is", () => {
    expect(menuCommand("import-folder")?.writes).toBe(true);
    expect(resolveMenu("import-folder", true)).toEqual({
      refused: "This library is open read-only, so it can't be edited.",
    });
    expect(resolveMenu("import-folder", false)).toEqual({ action: "import-folder" });
  });

  it("checks for updates whatever state the library is in", () => {
    // An update touches nothing in the library, so a read-only one is no
    // reason to refuse — whichever of the two reasons made it read-only.
    expect(resolveMenu("updates", true)).toEqual({ action: "updates" });
    expect(resolveMenu("updates", true, true)).toEqual({ action: "updates" });
    expect(resolveMenu("updates", false)).toEqual({ action: "updates" });
    expect(menuCommand("updates")?.writes).toBe(false);
  });

  it("says why rather than doing nothing", () => {
    const outcome = resolveMenu("missing", true);
    expect(outcome).not.toBeNull();
    expect(outcome).toHaveProperty("refused");
  });
});
