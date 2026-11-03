import { describe, expect, it } from "vitest";

import uiStrings from "./ui.json";
import de from "../../public/locales/de.json";
import ja from "../../public/locales/ja.json";
import { LANGUAGE_CHOICES, translate, type Catalog } from ".";
import { LOCALES } from "@/lib/preferences";

describe("localization", () => {
  it("offers every rekordbox language in its native name", () => {
    expect(LANGUAGE_CHOICES.map(({ value }) => value)).toEqual(LOCALES);
    expect(LANGUAGE_CHOICES.at(-1)).toEqual({ value: "ja", label: "日本語" });
  });

  it("uses the canonical catalogs and falls back to English", () => {
    expect(translate("Collection", de)).toBe("Sammlung");
    expect(translate("Collection", ja)).not.toBe("Collection");
    expect(translate("rbxport-only wording", de)).toBe("rbxport-only wording");
  });

  it("translates runtime messages while preserving their inserted details", () => {
    const catalog = { "Writing to {device}…": "Schreibe auf {device}…" };
    expect(translate("Writing to USB A…", catalog)).toBe("Schreibe auf USB A…");
  });

  it("has a translation entry for every detected UI string in every locale", async () => {
    for (const locale of LOCALES.filter((value) => value !== "en")) {
      const catalog = (await import(`../../public/locales/${locale}.json`)).default as Catalog;
      expect(uiStrings.filter((text) => catalog[text] === undefined), locale).toEqual([]);
    }
  });

  it("preserves named placeholders in translated dynamic messages", async () => {
    const placeholders = (text: string) => text.match(/\{[a-z][a-z0-9]*\}/gi) ?? [];
    const dynamic = uiStrings.filter((text) => placeholders(text).length > 0);
    for (const locale of LOCALES.filter((value) => value !== "en")) {
      const catalog = (await import(`../../public/locales/${locale}.json`)).default as Catalog;
      for (const source of dynamic) expect(placeholders(catalog[source] ?? ""), `${locale}: ${source}`).toEqual(placeholders(source));
    }
  });

  it("never translates a path, a file name or a service name", async () => {
    // Whole paths and module names, file names with an extension, dotfiles,
    // and the names of the services identification talks to.
    const kept = (text: string) => [
      ...(/^(?:@?\/|\.\/)\S+$/.test(text) ? [text] : []),
      ...(text.match(/(?<![\w.])\.[a-z][\w.]*[a-z]|\b[\w-]+\.(?:db|xml|json|identify|lang|zip|jpg|pdb|edb|dat|ext)\b/gi) ?? []),
      ...["AcoustID", "MusicBrainz", "Discogs", "Cover Art Archive"].filter((name) => text.includes(name)),
    ];
    const guarded = uiStrings.filter((text) => kept(text).length > 0);
    for (const locale of LOCALES.filter((value) => value !== "en")) {
      const catalog = (await import(`../../public/locales/${locale}.json`)).default as Catalog;
      for (const source of guarded) {
        const missing = kept(source).filter((token) => !(catalog[source] ?? source).includes(token));
        expect(missing, `${locale}: ${source}`).toEqual([]);
      }
    }
  });
});
