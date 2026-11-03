import { expect, test } from "@playwright/test";

/**
 * Track identification (fork-only) against the mock backend: Identify
 * Tracks on a selection starts a job, the review queue follows its progress,
 * and a result's ticked fields are applied and then undone. The mock's job
 * comes out review, unidentified, a fill-only fingerprint match, review,
 * failed, one track every 400 ms. Auto-apply is off by default, so the
 * fill-only match waits for review; turned on, the job writes it.
 */

test("identify a selection, review a result, apply ticked fields and undo", async ({ page }) => {
  await page.goto("/?writable=1");
  await expect(page.getByTestId("browser-title")).toContainText("Tracks)");
  const rows = page.getByRole("row").filter({ has: page.getByRole("gridcell") });
  await rows.nth(0).click();
  await rows.nth(4).click({ modifiers: ["Shift"] });
  await page.locator('[role="gridcell"][data-col="title"]').nth(2).click({ button: "right" });
  await page.getByRole("menu", { name: "Track" }).getByRole("menuitem", { name: "Identify Tracks" }).click();

  const panel = page.getByRole("dialog", { name: "Identify Tracks" });
  await expect(panel).toBeVisible();
  // Progress while it runs, in the panel and the status bar.
  await expect(panel.getByRole("status").first()).toContainText(/Identifying \d of 5/);
  await expect(panel.getByRole("button", { name: "Cancel", exact: true })).toBeVisible();
  await expect(page.getByRole("contentinfo").getByRole("button", { name: /Identifying \d of 5/ })).toBeVisible();

  // Closing the panel leaves the job running; reopening picks it up.
  await panel.getByRole("button", { name: "Close" }).click();
  await expect(panel).toHaveCount(0);
  await page.getByRole("contentinfo").getByRole("button", { name: /Identifying \d of 5/ }).click();
  await expect(panel).toBeVisible();

  await expect(panel.getByRole("status").first()).toHaveText("Identified 5 of 5", { timeout: 10_000 });
  await expect(panel).toContainText("0 applied automatically · 3 to review · 1 unidentified · 1 failed");

  const list = panel.getByRole("listbox");
  await expect(list.getByRole("option")).toHaveCount(3);
  const review = list.getByRole("option").filter({ hasText: "81%" }).first();
  await review.click();
  const detail = panel.getByRole("region", { name: "Identification result" });
  await expect(detail.getByRole("row").filter({ hasText: "Sub-genre (added to Comment)" })).toContainText("Purchased at Beatport.com; Tech House");
  await expect(detail.getByRole("checkbox", { name: "Write Track Title" })).toBeChecked();
  // rekordbox's imported record, where it differs from the file.
  await expect(detail).toContainText("rekordbox: Older Label");
  await expect(review).toContainText("Fingerprint");
  await detail.getByRole("checkbox", { name: "Write Label" }).uncheck();

  await detail.getByRole("button", { name: "Accept", exact: true }).click();
  await expect(detail.locator("header")).toContainText("Applied");
  await expect(detail.getByRole("button", { name: "Accept", exact: true })).toBeDisabled();
  await expect(detail).toContainText("select the tracks there and choose Reload Tag");
  await expect(list.getByRole("option")).toHaveCount(2);

  await panel.getByRole("tab", { name: "Applied (undoable)" }).click();
  await expect(list.getByRole("option")).toHaveCount(1);
  await list.getByRole("option").first().click();
  await detail.getByRole("button", { name: "Undo", exact: true }).click();
  await expect(detail.locator("header")).toContainText("Undone");
  await expect(list.getByRole("option")).toHaveCount(0);

  await page.keyboard.press("Escape");
  await expect(panel).toHaveCount(0);
});

test("bulk accept above a threshold, then fill only empty fields after confirming", async ({ page }) => {
  await page.goto("/?writable=1");
  await expect(page.getByTestId("browser-title")).toContainText("Tracks)");
  const rows = page.getByRole("row").filter({ has: page.getByRole("gridcell") });
  await rows.nth(0).click();
  await rows.nth(4).click({ modifiers: ["Shift"] });
  await page.locator('[role="gridcell"][data-col="title"]').nth(2).click({ button: "right" });
  await page.getByRole("menu", { name: "Track" }).getByRole("menuitem", { name: "Identify Tracks" }).click();
  const panel = page.getByRole("dialog", { name: "Identify Tracks" });
  await expect(panel.getByRole("status").first()).toHaveText("Identified 5 of 5", { timeout: 10_000 });

  await expect(panel.getByRole("spinbutton", { name: "Accept threshold value" })).toHaveValue("0.92");
  await panel.getByRole("spinbutton", { name: "Accept threshold value" }).fill("0.8");
  await panel.getByRole("button", { name: "Accept above" }).click();
  await expect(panel).toContainText("3 results accepted.");
  await expect(panel.getByRole("listbox").getByRole("option")).toHaveCount(0);

  await panel.getByRole("button", { name: "Apply accepted" }).click();
  const confirm = panel.getByRole("group", { name: "Apply accepted" });
  // The fill-only match fills two empty fields; the two reviews overwrite
  // a title and add a sub-genre, so they wait for their own review.
  await expect(confirm).toContainText("Fill 2 empty fields in 1 results. 2 results with overwrites or a sub-genre are left for individual review.");
  await confirm.getByRole("button", { name: "Apply", exact: true }).click();
  await expect(panel).toContainText("1 applied.");
  await panel.getByRole("tab", { name: "Applied (undoable)" }).click();
  await expect(panel.getByRole("listbox").getByRole("option")).toHaveCount(1);
});

test("All Tracks offers identifying the whole library and the review queue", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByTestId("browser-title")).toContainText("Tracks)");
  await page.getByRole("treeitem").filter({ hasText: "All Tracks" }).first().click({ button: "right" });
  const menu = page.getByRole("menu", { name: "All Tracks" });
  await expect(menu.getByRole("menuitem", { name: "Identify All Tracks" })).toBeEnabled();
  await menu.getByRole("menuitem", { name: "Review Identified Tracks" }).click();
  const panel = page.getByRole("dialog", { name: "Identify Tracks" });
  await expect(panel).toContainText("No identification has run yet.");
  await expect(panel.getByRole("tab", { name: "Needs review" })).toHaveAttribute("aria-selected", "true");
});

test("the Identify settings pane reports keys by source and saves a change", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("banner").getByRole("button", { name: "Settings" }).click();
  const dialog = page.getByRole("dialog", { name: "Preferences" });
  await dialog.getByRole("tab", { name: "Identify", exact: true }).click();

  // The mock's AcoustID key is stored in the settings; its Discogs token comes from the environment.
  const acoustid = dialog.getByRole("region", { name: "AcoustID API key" });
  await expect(acoustid).toContainText("Set (from settings)");
  const discogs = dialog.getByRole("region", { name: "Discogs personal token" });
  await expect(discogs).toContainText("Set (from environment)");
  await expect(discogs.getByRole("button", { name: "Remove Discogs personal token" })).toHaveCount(0);
  await expect(dialog).toContainText("identify\\.env.identify");

  await acoustid.getByRole("button", { name: "Replace…" }).click();
  const key = acoustid.getByLabel("New AcoustID API key");
  await expect(key).toHaveValue("");
  await key.fill("not-a-real-key");
  await acoustid.getByRole("button", { name: "Save" }).click();
  await expect(key).toHaveCount(0);
  await expect(dialog).not.toContainText("not-a-real-key");

  const auto = dialog.getByRole("region", { name: "Auto-apply" });
  await expect(auto.getByRole("switch")).not.toBeChecked();
  await auto.getByRole("switch").check();
  const threshold = auto.getByRole("spinbutton", { name: "Minimum score" });
  await threshold.fill("0.5");
  await threshold.press("Enter");
  await expect(auto.getByRole("alert")).toHaveText("Choose a threshold from 0.92 to 1.00.");
  await threshold.fill("0.95");
  await threshold.press("Enter");
  await expect(auto.getByRole("alert")).toHaveCount(0);
  await expect(threshold).toHaveValue("0.95");
});

test("with auto-apply on, the job writes a qualifying match and files it under Applied, undoably", async ({ page }) => {
  await page.goto("/?writable=1");
  await expect(page.getByTestId("browser-title")).toContainText("Tracks)");
  await page.getByRole("banner").getByRole("button", { name: "Settings" }).click();
  const dialog = page.getByRole("dialog", { name: "Preferences" });
  await dialog.getByRole("tab", { name: "Identify", exact: true }).click();
  const auto = dialog.getByRole("region", { name: "Auto-apply" });
  await auto.getByRole("switch").check();
  await expect(auto.getByRole("switch")).toBeChecked();
  await page.keyboard.press("Escape");
  await expect(dialog).toHaveCount(0);

  const rows = page.getByRole("row").filter({ has: page.getByRole("gridcell") });
  await rows.nth(0).click();
  await rows.nth(4).click({ modifiers: ["Shift"] });
  await page.locator('[role="gridcell"][data-col="title"]').nth(2).click({ button: "right" });
  await page.getByRole("menu", { name: "Track" }).getByRole("menuitem", { name: "Identify Tracks" }).click();
  const panel = page.getByRole("dialog", { name: "Identify Tracks" });
  await expect(panel.getByRole("status").first()).toHaveText("Identified 5 of 5", { timeout: 10_000 });
  await expect(panel).toContainText("1 applied automatically · 2 to review · 1 unidentified · 1 failed");
  await expect(panel.getByRole("listbox").getByRole("option")).toHaveCount(2);

  await panel.getByRole("tab", { name: "Applied (undoable)" }).click();
  const written = panel.getByRole("listbox").getByRole("option");
  await expect(written).toHaveCount(1);
  await expect(written.first()).toContainText("Applied automatically");
  await written.first().click();
  const detail = panel.getByRole("region", { name: "Identification result" });
  await expect(detail).toContainText("choose Reload Tag");
  await detail.getByRole("button", { name: "Undo", exact: true }).click();
  await expect(detail.locator("header")).toContainText("Undone");
  await expect(written).toHaveCount(0);
});
