# Track Identification Feature — Spec (fork-only)

This fork of rbxport adds track identification and tag correction. Upstream deliberately keeps a small feature set, so this lives only in this fork. Follow the repo's existing conventions, performance budgets (`perf-budgets.json`) and testing rules everywhere.

## Hard constraints

1. **Free services only.** Allowed: AcoustID (non-commercial key), MusicBrainz, Cover Art Archive, Discogs (personal token). Never add ACRCloud, Shazam or SongRec endpoints, Beatport scraping, or any paid or ToS-violating source.
2. **Never silently overwrite tags.** Every change goes through the review queue unless it meets the auto-apply rule below. Original tag values are snapshotted before any write so the change can be undone.
3. **Rate limits are mandatory, enforced in code** with a per-service token bucket:
   - AcoustID: ≤ 3 req/s.
   - MusicBrainz: ≤ 1 req/s, sending the `MB_USER_AGENT` value from config.
   - Discogs: ≤ 60 req/min, backing off on HTTP 429.
4. **Secrets** are read from `.env.identify` or app settings. They are never logged, never committed, and never sent anywhere except their own service.
5. **Library safety.** Tests run with `RB_LITE_TEST=1`. Never write to the installed Rekordbox library during development. Integration tests use temp fixtures or `C:\dev\id-testset\` copies only.
6. **Windows is the primary target.** All paths, file locking and long-path handling must work on Windows/NTFS and on FAT32/exFAT USB volumes.
7. **The identify feature must never touch the USB export code path.**

## Architecture

- New crate `crates/rbl-identify` (or whatever matches the existing `rbl-*` naming), independent of the export and LINK crates.
- Preferred Rust dependencies, pure Rust where possible:
  - `symphonia` to decode audio for fingerprinting (the first ~120 s is enough);
  - `rusty-chromaprint` for fingerprints, which avoids shipping fpcalc or FFmpeg;
  - `lofty` to read and write tags (ID3v2.3/2.4, FLAC/Vorbis, MP4, AIFF/WAV ID3);
  - `reqwest` + `serde`, plus the existing async runtime for HTTP.

  Justify any dependency outside this list.
- Persist results in an `identify` SQLite store owned by this feature: fingerprint, candidates, scores, decision, original-tag snapshot and timestamps. Do not alter the Rekordbox schema. Library metadata updates go through rbxport's existing edit path.
- Expose the feature to the frontend with Tauri commands and events: start/cancel job, progress, list queue, accept/reject/edit candidate, undo.

## Pipeline (per track)

1. **Pre-check.** If artist, title and duration are present and plausible, run a MusicBrainz text search to verify them first.
2. **Fingerprint.** Compute a Chromaprint fingerprint, call AcoustID `lookup` with `meta=recordings+releasegroups+compress`, and collect candidate MusicBrainz recording IDs.
3. **Resolve.** Fetch from MusicBrainz: artist(s), title (with mix/version), release, label, catalog number, date, ISRC and genres/tags.
4. **Enrich.** Run a Discogs search by artist + title (+ label/catno when known) to fill label, catalog number, year and **styles**, which give sub-genre granularity such as Tech House or Progressive House. Pull artwork from Cover Art Archive or Discogs only if the track has none.
5. **Score** each candidate from 0 to 1, combining: AcoustID score, duration delta (≤ 2 s strong, > 10 s reject), string similarity to existing tags (normalized; ignore case, "feat." variants and punctuation), and mix-name agreement ("Extended Mix" vs "Radio Edit" vs a remix must match). Never treat a remix and the original as the same track.
6. **Decide.**
   - **Auto-apply** only if score ≥ 0.92, the change fills *empty* fields only, and the user has enabled auto-apply. The default is off.
   - **Review** for everything else with a candidate.
   - **Unidentified** when no candidate scores ≥ 0.5. Don't guess.
7. **Write.** On accept: snapshot the originals, write the tags with `lofty`, update the library record, and record an undo entry.

## Tag mapping rules

- Write to: Title, Artist, Album, Label (`TPUB`), Year/Date, Genre, ISRC, Catalog number (TXXX `CATALOGNUMBER`), MusicBrainz IDs (standard Picard TXXX names), Discogs release ID (TXXX `DISCOGS_RELEASE_ID`), and artwork.
- **Genre policy:** configurable. The default writes the Discogs styles, joined with "; ", into Genre and leaves the existing genre untouched when the user has set it.
- **Never modify:** comments, rating, color, cue points, beat grid, BPM, key or any analysis data. Key and BPM come from analysis, never from web metadata.
- Keep the mix name in the title in the format `Title (Extended Mix)`, matching store conventions.

## UI

- An "Identify" action on a selection, playlist or the whole library, running as a background job with progress and cancel.
- A review queue showing a current-vs-proposed diff per field, with per-field checkboxes, accept/reject/edit and bulk accept above a chosen threshold.
- Filters: Unidentified, Needs review, Applied (undoable).
- Match the existing UI patterns and the translation system.

## Testing and acceptance

- Unit tests: scoring, normalization, mix-name parsing, tag mapping and rate limiters (with mocked time).
- HTTP tests use recorded fixtures or mocks. CI makes no live calls.
- A manual evaluation script runs the pipeline over `C:\dev\id-testset\` in dry-run mode and outputs a CSV with columns: file, top candidate, score, decision, ground-truth match.
- Acceptance on the ground-truth set:
  - **zero** wrong auto-applies;
  - ≥ 80% correct top candidates on mainstream releases;
  - unidentified tracks are reported as unidentified rather than mis-tagged.
- Run `pnpm lint`, `pnpm test`, `pnpm build`, `pnpm budget` and `RB_LITE_TEST=1 cargo test --workspace`. All of them must pass before a phase is called done.

## Phases

0. **Environment and codebase survey.** Report findings and propose a plan; no code.
1. **`rbl-identify` core.** Decode → fingerprint → AcoustID → MusicBrainz, the scoring model, a CLI dry-run over the test set, and the CSV report.
2. **Discogs enrichment, Cover Art Archive, and the identify SQLite store** with original-tag snapshots.
3. **Tauri commands and events,** plus the background job with progress and cancel.
4. **Review queue UI,** accept/reject/edit, and tag writing with undo.
5. **Settings** (keys, genre policy, auto-apply toggle and threshold) and docs (`docs/identify/USER_GUIDE.md`).

Stop at the end of each phase. Summarize what changed, the test results and any open questions, then wait for approval.
