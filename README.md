<p align="center">
  <img src="docs/assets/brand/combined/rbxport-logo-color-white.png" alt="rbxport" width="360">
</p>

# rbxport ID

**rbxport ID** is an unofficial fork of [rbxport](https://github.com/chrisle/rbxport), the open-source DJ library manager inspired by rekordbox. It adds what upstream keeps out on purpose: **its own music library**, so rekordbox becomes something you import from rather than the place your library lives, **track identification and tag correction**, and **intelligent playlists**.

[![Latest release](https://img.shields.io/github/v/release/thesals/rbxport_ID?label=latest%20release)](https://github.com/thesals/rbxport_ID/releases/latest)

## Download

| | Download | Notes |
|---|---|---|
| **Windows** 10 / 11 (64-bit) | [**rbxport-ID-Windows-setup.exe**](https://github.com/thesals/rbxport_ID/releases/latest/download/rbxport-ID-Windows-setup.exe) | If SmartScreen warns you, click **More info → Run anyway**. |
| **macOS** 11 or later (Apple silicon and Intel) | [**rbxport-ID-Mac.dmg**](https://github.com/thesals/rbxport_ID/releases/latest/download/rbxport-ID-Mac.dmg) | Not signed by Apple, so you allow it once. See [Installing on a Mac](docs/user/macos.md). **New: not tested on a Mac yet.** |

These links always download the newest build. Every release, with its notes and older versions, is on the [Releases page](https://github.com/thesals/rbxport_ID/releases).

**Updates are automatic.** Every change to this repository builds a new release. Installed copies check for it when they start, or when you choose **Check for Updates…**, and update themselves.

## What this fork adds

### Its own library; rekordbox is only imported from
- rbxport ID keeps its library in its own folder (`%APPDATA%\rbxport\library` on Windows, `~/Library/Application Support/rbxport/library` on a Mac), with the same tables as rekordbox, so analysis, USB export and PRO DJ LINK work as before.
- **rekordbox's library is never written.** rbxport ID imports from a verified copy and checks that the original is unchanged afterwards.
- **Import again at any time.** New tracks come in whole. Tracks you already have get only what rbxport ID is missing, and tags you changed in rbxport ID are never replaced. The import report lists what was taken and what was kept. For tracks you choose, **Take rekordbox's tags** adopts rekordbox's version instead.
- Read more in [Library and import](docs/user/library-and-import.md).

### Track identification and tag correction
- **Identify Tracks** on a selection, a playlist or the whole library. It fingerprints the audio and looks the track up in free services only: **AcoustID**, **MusicBrainz**, **Cover Art Archive** and **Discogs**.
- It fills in title and mix name, artist, album, label, year, catalogue number, ISRC and artwork. Discogs styles (sub-genres such as Tech House) are added to the comment.
- **Nothing is changed silently.** Results wait in a review queue with a field-by-field comparison. Auto-apply (off by default) takes only matches scoring 90 % or more, and only into empty fields. Every change can be undone.
- Writing tags into the audio files themselves stays off until you turn it on.
- Identify needs a free AcoustID key, and optionally a Discogs personal token, entered in Preferences.

### Intelligent playlists
- **Create New Intelligent Playlist** (right-click **Playlists**, a folder or a playlist) builds a playlist from rules such as genre, BPM range, key, rating, date added and My Tags. It updates itself as your library changes.

### USB export
- **Export Playlist**, **Export Folder** and **Export Track** list your connected USB drives, so you pick the stick instead of hunting for it in a file browser. **Other location…** still lets you choose any folder.
- Drives are named as Explorer names them, label and letter (**DJ (G:)**), so two sticks with the same label can be told apart. On a Mac a drive keeps its name.
- **Devices** and **Sync Manager** list only USB drives and drives that already hold an export, not your internal disks.
- **Works on older players.** Sticks now load on the XDJ-1000 mk1 (firmware 1.13), which used to freeze at the My Settings screen or on the browse screen. See [Fixed in v1.2.14](#fixed-in-v1214).

### Installers and updates
- A Windows installer and a Mac app are built for every release on GitHub, and each is signed for the built-in updater.

### Fixed in v1.2.14

An XDJ-1000 mk1 (firmware 1.13) froze on rbxport ID's USB sticks while it read rekordbox's sticks of the same tracks. Two dozen rounds of test sticks traced it to one cause: **text with accents or symbols (é, Ø, a no-break space) was stored a few bytes off the 4-byte boundary rekordbox always uses**, in album names and in track titles and paths. A single misplaced title was enough to freeze the player. The whole investigation is in [the XDJ freeze record](docs/identify/xdj-freeze-investigation.md).

- Exports now store that text exactly as rekordbox does.
- **Update sticks you exported before v1.2.14**: sync them once, or export them again, and they load on the XDJ. **Verify** reports an older stick as needing it.
- Tracks renamed because two shared a file name no longer get a Windows backslash in their path on the stick.
- Earlier in this run of fixes, each matching what rekordbox writes:
  - file names are cut to rekordbox's 48 characters;
  - track records carry the header values rekordbox writes;
  - playlists of more than 255 tracks record their row count correctly.

### Coming next
- **Playlists from a track**: pick a track and get a playlist of tracks that go with it by genre, tempo, key and energy, like iTunes' Genius. The design is drafted and waiting on decisions.

## Status

This fork is developed and tested on Windows, and its USB exports are tested on an XDJ-1000 mk1 (firmware 1.13). The Mac build is new and waiting for its first test on a real Mac; if you try it, the [Mac guide](docs/user/macos.md#testing-checklist-for-whoever-tries-it-first) has a checklist of what to report.

Problems and suggestions: [open an issue](https://github.com/thesals/rbxport_ID/issues). Please don't report this fork's issues to upstream rbxport.

## For developers

The fork's specs:
- [Own library](docs/library/OWN_LIBRARY_SPEC.md);
- [Track identification](docs/identify/IDENTIFY_SPEC.md);
- [The XDJ-1000 freeze investigation](docs/identify/xdj-freeze-investigation.md): how the USB export fix was proven, round by round.

Releases come from [`.github/workflows/fork-release.yml`](.github/workflows/fork-release.yml), which runs on every push to `main`.

Everything below this line is upstream rbxport's own README. It still applies to the code this fork is built on.

---

# About upstream rbxport

rbxport is a music library management app inspired by rekordbox. It keeps its
feature set deliberately small, aiming for a faster, simpler user experience.

Its scope is limited to library management, USB exporting, and PRO DJ LINK.

## Project vision

The core vision of rbxport is **fewer features**.

Because rbxport is open source, you are free to fork it and build a version of
rekordbox customized by the community. The core project does not aim to be that.
A project that keeps adding features eventually becomes what rekordbox already is: full-featured
DJ software. rbxport stays small on purpose. Any contribution that adds a feature
is weighed carefully against this vision, and a good feature can still be
declined.

Two commitments define what "small" must not cost:

- **Compatibility.** rbxport aims to be fully compatible with the current
  version of rekordbox and all the hardware that rekordbox supports.
- **Performance.** rbxport maintains its speed through a performance budget,
  defined in `perf-budgets.json` and enforced by `pnpm budget` and CI. See
  [Development conventions](docs/development/conventions.md).

If you want to propose a feature, read
[Feature proposals](CONTRIBUTING.md#feature-proposals) in the contributing guide
first.

## Tech stack

| Layer | Technology |
| --- | --- |
| Desktop app | Tauri 2. |
| Frontend | React and TypeScript, with TanStack Virtual for track browsing. |
| Backend | Rust, with independent `rbl-*` crates for library, audio, export, and LINK logic. |
| Database | SQLCipher for rekordbox libraries and OneLibrary USB exports. |
| Build tooling | Vite, pnpm, and Cargo. |
| Testing | Vitest, Playwright, and Rust tests with temporary library fixtures. |

See [Architecture](docs/development/architecture.md) for the repository layout
and how the frontend, desktop shell, and Rust crates fit together.

## Start here

For a first look at the interface, use the browser with a mock library:

```sh
pnpm install
pnpm dev:web
```

This requires Node 26 and pnpm 10.17.1. It does not open an installed rekordbox
library or require a Rust build. For desktop prerequisites and safe library
setup, read [Getting started](docs/development/getting-started.md).

## Developer reading path

Read these in order to understand the project and make your first change:

1. [Getting started](docs/development/getting-started.md): prerequisites, run modes, and library safety.
2. [Architecture](docs/development/architecture.md): repository map, crate responsibilities, IPC, and edit flow.
3. [Development conventions](docs/development/conventions.md): code style, performance, translation, and evidence rules.
4. [Testing](docs/development/testing.md): checks, test setup, and what each result proves.
5. [Contributing](CONTRIBUTING.md): issues, branches, implementation, validation, and review.
6. [Debugging](docs/development/debugging.md): logs, diagnostics, environment variables, and cleanup.
7. [Releases](docs/development/releases.md): versions, CI, packaging, and publication.

The [documentation index](docs/README.md) also links to user guides and detailed
analysis, USB-format, and hardware references.

## Common commands

| Command | Purpose |
| --- | --- |
| `pnpm dev:web` | Browser UI with the mock backend. |
| `pnpm dev` | Desktop app; can open your installed library. Read the safety guide first. |
| `pnpm lint` | Frontend lint rules. |
| `pnpm build` | TypeScript check and production frontend bundle. |
| `pnpm test` | Vitest tests. |
| `pnpm e2e` | Playwright browser tests against the mock backend. |
| `RB_LITE_TEST=1 cargo test --workspace` | Rust tests with installed-library writes refused. |

## License and trademarks

rbxport is GPL-2.0-or-later. See [LICENSE](LICENSE) and
[Licensing](LICENSING.md) for bundled components and distribution terms.
Third-party product names belong to their respective owners. rbxport is an
independent project and is not affiliated with, endorsed by, or sponsored by
their owners.

## Disclaimer

rbxport is provided "as is", without warranty to the extent permitted by
applicable law. Use it at your own risk and keep backups of your music library
and USB drives. Unless required by
applicable law or agreed to in writing, the authors and contributors are not
liable for damages arising from using or being unable to use rbxport,
including lost or corrupted data, equipment damage, or financial losses.
See [LICENSE](LICENSE) for the full warranty and liability terms.

rbxport is an independent project and is not affiliated with, endorsed by,
or sponsored by AlphaTheta or Pioneer DJ. References to rekordbox,
PRO DJ LINK, and other products describe compatibility only. All trademarks
and product names belong to their respective owners.

## Special thanks

evanpurkhiser, Maddix, Morgan Page, nichi, profbx, Sean Tyas, shiz, syl, trancejesus, xorbxbx, and
AlphaTheta.
