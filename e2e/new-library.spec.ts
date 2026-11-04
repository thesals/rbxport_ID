/**
 * A first start, before rbxport has a library of its own, gets one question —
 * create a new library, or quit — and nothing else: no error in the status
 * bar, no rows left over from a previous session.
 */
import { expect, test } from "@playwright/test";

test("with no library the window asks to create one, and creating it loads the library", async ({ page }) => {
  await page.goto("/?nolibrary");

  const dialog = page.getByRole("dialog", { name: "Welcome to rbxport" });
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText("rbxport keeps its own library, separate from rekordbox.");
  await expect(dialog).toContainText("/rbxport/library/master.db");
  await expect(page.locator('[role="row"] [data-col="title"]')).toHaveCount(0);

  // Escape is not a way out: there is nothing to go back to.
  await page.keyboard.press("Escape");
  await expect(dialog).toBeVisible();

  await dialog.getByRole("button", { name: "Create a New Library" }).click();
  await expect(dialog).toBeHidden();
  await expect
    .poll(async () => page.locator('[role="row"] [data-col="title"]').count(), { timeout: 10_000 })
    .toBeGreaterThan(0);
});

test("a library that is there never asks", async ({ page }) => {
  await page.goto("/");
  await expect
    .poll(async () => page.locator('[role="row"] [data-col="title"]').count(), { timeout: 10_000 })
    .toBeGreaterThan(0);
  await expect(page.getByRole("dialog", { name: "Welcome to rbxport" })).toHaveCount(0);
});
