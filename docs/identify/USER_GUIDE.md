# Track identification — user guide

Track identification works out what a track is from its audio and its existing tags. It then proposes better tags: the catalogue number, release year, label, ISRC, MusicBrainz and Discogs IDs, artwork, and sub-genre styles. Nothing is written until you review and accept it, unless you turn on auto-apply, which is off by default and narrowly limited (see [Auto-apply](#auto-apply)).

It's a feature of this fork of rbxport only.

## What it changes, and what it never changes

An accepted result is written in two places, in this order:

1. **rbxport's own library.** The ticked fields it keeps (title, artist, album, label, genre, year, ISRC, the comment and artwork) are changed there at once, the same way an edit in the information panel is. The browser shows the new values straight away.
2. **The audio file's tags**, but only while **Write tags to audio files** is on (it's off by default). Fields rbxport's library doesn't keep, such as the catalogue number and the MusicBrainz and Discogs IDs, go to the file only.

If the library can't be written, nothing is written to the file either. If the file isn't written (writing is off, the file is open in a player, or the write fails), the library change stands, and the result says the file wasn't written.

**rekordbox's library is never changed.** rbxport keeps its own library, imported from rekordbox's. Results identified before that switch still carry the rekordbox record that was copied with them, read-only; the review now shows rbxport's own library instead.

**Library Protection** (Preferences → Advanced) locks Identify too: while it's on, Accept, Apply accepted, Undo and auto-apply write nothing.

These are never changed: rating, colour, cue points, beat grid, BPM, key, or any analysis data. BPM and key always come from analysis, never from the internet. USB exports aren't affected.

**The comment is only added to, never replaced.** Sub-genre styles from Discogs (for example "Minimal Techno; Tech House") are appended after "; " to the end of the comment: the file's comment in the file, and the library's own comment in the library. What either comment already says is kept, and the same styles are never added twice.

## Setting up

Identification uses free services only:

| Service | What it's for | What you need |
|---|---|---|
| AcoustID | Recognising the audio from its fingerprint | A free API key from **acoustid.org/new-application** (required) |
| MusicBrainz | Release, label, catalogue number, date and ISRC | A user agent naming this app and how to contact you, such as `rbxport-identify/1.0 ( you@example.com )` (required) |
| Discogs | Label, catalogue number, year, styles (sub-genre), and a fallback for tracks MusicBrainz doesn't know | A free personal access token from **discogs.com/settings/developers** (optional, but recommended for electronic music) |
| Cover Art Archive | Artwork, for files that have none | Nothing |

To enter them, open **Preferences → Identify**, then save.

- Keys are stored in a `.env.identify` file in rbxport's data folder; the pane shows its path.
- The AcoustID key and Discogs token are never displayed again, never written to logs, and never sent anywhere except their own service. The pane only says whether each one is set. The MusicBrainz user agent isn't secret and is shown.
- **Replace** changes a saved key; **Remove** deletes it.
- A key set as an environment variable (`ACOUSTID_API_KEY`, `DISCOGS_TOKEN`, `MB_USER_AGENT`) always takes priority over a saved one. The pane says when that's the case. To change such a key, change or remove the environment variable outside rbxport.

Each service's rate limit is built in, so a large batch takes a while. MusicBrainz allows one request a second.

### Write tags to audio files

**Write tags to audio files** is off by default. While it's off, identification only reads your files. Accept, Apply accepted and auto-apply write rbxport's library only, and a result asking only for fields the library doesn't keep (the catalogue number, say) is refused. Undo puts the library back, but a result whose file was written can't be undone until writing is on again, since its file would have to be put back too.

Turning it on asks you to confirm first. Turning it on doesn't write results you accepted while it was off; their files stay as they are. Waiting writes stay queued while it's off and are tried again once you turn it on. The lock is checked at every write, so switching it takes effect at once, even during a run.

### Genre

- **Fill an empty Genre with the Discogs styles** (default). A genre you've already set is never proposed for change.
- **Replace Genre with the Discogs styles.** The change is still reviewed like any other overwrite.
- **Never propose a genre.**

Whichever you choose, the styles are also offered for the comment, for review.

### Auto-apply

Auto-apply is off by default. When it's on, a result is written during the run, without review, to rbxport's library and (while **Write tags to audio files** is on) to the file, only if all of these are true:

- the match came from the audio fingerprint, not a text search or Discogs;
- the confidence score is at least the level you set, from 90% (the default) to 100%;
- every change fills a field that's **empty** in the file, and in rbxport's library too (or already holds the same value there).

Comment (sub-genre) changes and anything that would overwrite a value are never auto-applied. An auto-applied result appears under **Applied** and can be undone like any other. If the file is open in a player, the write waits until the file is free (see [Files in use](#files-in-use)); it isn't counted as identified until it's written. If its write fails for another reason, for example because the file is read-only, it stays under **Needs review** with the reason as a warning. If the write was interrupted partway, the result waits until the next start, which settles it (see [Safety](#safety)).

A run keeps the settings it started with. If you turn auto-apply off during a run, the change takes effect from the next run.

## Identifying tracks

1. Select tracks and right-click them, or right-click a playlist or a folder, and choose **Identify Tracks**. On **All Tracks**, choose **Identify All Tracks**.
2. While it runs, the status bar shows "Identifying *n* of *N*". Choose **Stop** to cancel; finished tracks are kept.
3. When it ends, the review panel's summary shows how many tracks were auto-applied, need review, were unidentified, or failed.

For each track, identification tries these in turn:

1. **Text search:** when the file has both an artist and a title, MusicBrainz is searched for them first.
2. **Fingerprint:** the first two minutes of audio are fingerprinted and looked up on AcoustID, then resolved through MusicBrainz.
3. **Discogs fallback:** if you have a Discogs token and no candidate scores at least 0.5, Discogs is searched for the existing artist and title.

Each candidate is scored from 0 to 1. The score combines:

- the AcoustID score;
- how closely the length matches (more than 10 seconds apart rejects the candidate);
- how well the names match;
- whether the mix agrees. An Extended Mix, a Radio Edit and a remix are never treated as the same track.

A track with no candidate scoring at least 0.5 is marked **Unidentified**, not guessed. Discogs fallback matches never score above 0.85, and never above 0.75 when Discogs doesn't list the track's length.

## Reviewing

Open the review queue with **Review Identified Tracks**. Filter it by **Needs review**, **Unidentified**, **Applied** (results you can still undo), **Waiting for file** (writes waiting for a file to be closed, newest first) or **All**. Each row shows the score and where the match came from: **Fingerprint**, **Text search** or **Discogs**.

Choose a row to see:

- **Current against proposed** for every field, with a checkbox for each. Untick anything you don't want.
- **The library's value** ("In library: …"), under any field where rbxport's library differs from the file. That's the value Accept replaces in the library.
- **Edit**, to correct a proposed text value before it's written. The sub-genre and artwork can't be edited. BPM and key are never proposed.
- **Candidates:** every release considered, with its score and source.
- **Artwork**, offered only when the file has none, with its size and source.

Then:

- **Accept** writes the ticked fields to rbxport's library and, while writing to audio files is on, to the file.
- **Reject** dismisses the result; nothing is written.
- **Accept above** accepts every pending result scoring at least the threshold you choose (0.92 by default). Nothing is written yet.
- **Apply accepted** writes accepted results after a confirmation, and **only fills empty fields**. A result with any overwrite or comment change is skipped entirely, and the confirmation says how many were skipped. Apply those from their own review.

## Undo

Results under **Applied** can be undone. Undo puts back both what the library held and, when the file was written, the file's tags. It's refused, and nothing is put back, if either has changed since: a field you edited in rbxport afterwards, or a file something else has written to. That way undo never overwrites a later change.

A result written to the library only says so in its review ("Written to rbxport's library only").

What undo restores depends on the format:

- **MP3, AIFF and WAV (ID3 tags):** the exact original tag bytes.
- **FLAC, MP4/M4A and OGG:** the original value of each changed field. The tag block itself is rewritten, so its layout can differ from before.

## Safety

Before any write:

- The file's original tags are saved in the identify database.
- The file must still hold the tags you reviewed. If it has changed since, the write is refused; identify the track again.
- Read-only files, and files you don't have permission to change, are refused with a message saying which.
- A file another program has open (in practice, a track loaded or playing in a player) isn't written yet: the write waits until the file is free. See [Files in use](#files-in-use).

The new tags are written to a temporary copy in the same folder, which then replaces the original. For MP3, AIFF and WAV, only the changed tag frames are rewritten, and the audio and all other frames are kept byte for byte. For FLAC, MP4/M4A and OGG, the audio is kept and the tag block is rewritten with every field it could read.

If rbxport or the computer stops in the middle of a write, the next start checks each interrupted file:

- **The file still holds its old tags:** the write is marked as not done.
- **The file holds the new tags:** the write is marked as done.
- **The file holds something else:** the write is closed as unknown and the result goes back to **Needs review**. Its original tags are kept.
- **The file opens, but its tags can't be understood:** the write is closed as unknown and the result goes back to **Needs review** with a warning. Its original tags are kept.
- **A player has the file open, or it can't be read right now:** the write stays open and is checked again when rbxport closes and at the next start.
- **The file can't be found:** for example, it's on a drive that isn't connected. The write stays open, and the track can't be reviewed or written, until a later start finds the file again.

### Files in use

When you apply a result, or auto-apply writes one, while another program has the file open, the library is written but the file isn't, and nothing fails. The result stays accepted and shows under **Waiting for file** with the fields to write. rbxport tries it again when it starts, when it closes (for up to about 10 seconds; a write that can't finish in time is stopped cleanly and waits for the next start), before every identification run and every apply, and when you choose **Retry now**. **Cancel write** drops the file write. The library change stands, so the result stays under **Applied** (library only) and can be undone; a result with nothing in the library goes back to **Needs review**.

Each try goes through the same checks as a normal apply. If the file still holds the tags you reviewed, the write is made and can be undone. If the file has changed meanwhile, the write is dropped and the result goes back to **Needs review** with a warning saying why. While a write waits you can't accept, reject or edit the result, or identify the track again; choose **Cancel write** first.

Undo is never deferred: undoing a file that's open in another program fails with a message, and you can try again once it's closed.

Any leftover temporary file is removed. The audio file itself is never rewritten during this check.

## Where things are kept

Everything lives in rbxport's data folder, under `rbxport\identify\` (on Windows, `%APPDATA%\rbxport\identify\`):

| File | Contents |
|---|---|
| `identify.db` | Results, candidates, the original tags and library values used for undo, history, and the rekordbox records copied with results made before the switch |
| `.env.identify` | Your service keys |
| `settings.json` | Genre policy, auto-apply and "Write tags to audio files" settings |

Deleting the folder resets identification, without touching rekordbox, rbxport's library or your music files. It also deletes your saved keys, and the original values behind every applied change, so those changes can no longer be undone.
