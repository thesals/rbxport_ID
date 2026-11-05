# Library and import

rbxport keeps its own library, in its own folder:

| | Folder |
|---|---|
| Windows | `%APPDATA%\rbxport\library\` |
| macOS | `~/Library/Application Support/rbxport/library/` |

It holds `master.db` (the same layout as rekordbox's, so analysis, USB export and LINK work as before), `share/` (analysis files and artwork), `import.db` (what was imported from rekordbox) and `import-log/`.

**rekordbox's library is never written.** rbxport only reads it, from a copy it checks, to import from it. Your audio files are never moved, renamed or retagged by an import.

## The first import

On the first start, with no library yet, rbxport offers to **Import from rekordbox** or **Create a New Library**. Importing copies rekordbox's whole library, with its analysis files and artwork, into rbxport's folder. Track IDs stay the same as rekordbox's. When it's done, rbxport shows what was imported and confirms that rekordbox's library is unchanged.

## Importing again

**File › Import from rekordbox…** imports from rekordbox again, into the library rbxport already has. You can do this as often as you like; only what's new in rekordbox comes over.

- **Tracks rbxport doesn't have** come over in full: tags, cues, analysis, artwork, My Tags. They get new rbxport IDs.
- **Tracks rbxport has** are found by the record of the last import, or else by their audio file's path.
  - **Tags** (title, artist, album, album artist, remixer, original artist, composer, lyricist, genre, label, year, release date, track and disc number, comment, rating, colour, ISRC, My Tags) are taken from rekordbox **only if you haven't changed any of them in rbxport** since the last import. If you have (by hand, with Identify, from a CDJ over LINK, any way), the track keeps all its tags, and the report shows where rekordbox's differ.
  - **Cues, hot cues, beat grid and waveforms, key and artwork** are taken only where rbxport has none of that kind. Nothing of rbxport's is replaced.
  - A track matched only by its file's path keeps its tags: there's no earlier import to tell whether you changed them.
- **Playlists** rekordbox has made since are made in rbxport. Tracks added to a rekordbox playlist are added to the end of rbxport's, unless you changed that playlist in rbxport since the last import. Then it's left as it is, and the report says so.
- **History sessions** rekordbox has recorded since are added. Each play adds one to the track's play count in rbxport.
- **Nothing is removed.** A track or playlist you deleted in rbxport stays deleted, and one rekordbox has removed stays in rbxport.

The library is backed up before the import starts. When it's done, the report lists the new tracks, the tracks whose tags were updated, the tracks kept as you edited them (with rekordbox's values beside yours, not applied), what was filled in, and the playlists and history sessions added. The same, track by track, is in the import log in `import-log/`.

If rekordbox has changes it hasn't saved into its library file yet, or keeps changing the file while rbxport copies it, the import asks you to close rekordbox and try again. Closing rekordbox first avoids that.

## rekordbox XML and iTunes

**File › Import rekordbox xml…** and **Import iTunes Library xml…** follow the same rules, as far as a document allows:

- tracks rbxport doesn't have are imported;
- a track already in rbxport keeps its tags (a document has no record of earlier imports), and gets the document's memory cues or hot cues only when it has none of that kind;
- a folder or playlist with the same name in the same place is reused and gets the tracks it's missing, so importing the same document twice doesn't make duplicates;
- nothing is removed.
