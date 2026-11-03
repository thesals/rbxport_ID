# Track identification — user guide

Track identification works out what a track is from its audio and its existing tags. It then proposes better tags: the catalogue number, release year, label, ISRC, MusicBrainz and Discogs IDs, artwork, and sub-genre styles. Nothing is written until you review and accept it, unless you turn on auto-apply, which is off by default and narrowly limited (see [Auto-apply](#auto-apply)).

It's a feature of this fork of rbxport only.

## What it changes, and what it never changes

Identification is **one-way**.

- **rekordbox's library is never changed.** When a track is identified, its rekordbox record (title, artist, album, label, genre, year, comment, ISRC) is copied, read-only, into identification's own database. It's used only to show you where rekordbox and the file disagree.
- **Only the audio file's tags are written.** That happens only when you apply a result, or when auto-apply does.
- **rekordbox shows the new tags only after you reload them.** In rekordbox, select the tracks, right-click and choose **Reload Tag**. Until then it keeps its old values. That's why the review screen points out where rekordbox's record differs from the file.

> **Before you use Reload Tag:** expect it to replace what rekordbox shows with what the file says, for every tag field. Any edit made only inside rekordbox for those tracks, shown in the review as "rekordbox: …", will most likely be lost. If you want to keep such an edit, untick that field before applying, or make the same edit in the review. We haven't yet confirmed exactly which fields rekordbox re-reads.

These are never changed: rating, colour, cue points, beat grid, BPM, key, or any analysis data. BPM and key always come from analysis, never from the internet. USB exports aren't affected.

**The comment is only added to, never replaced.** Sub-genre styles from Discogs (for example "Minimal Techno; Tech House") are appended to the end of the file's comment, the one rekordbox shows after Reload Tag, after "; ". What the comment already says is kept, and the same styles are never added twice.

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

### Genre

- **Fill an empty Genre with the Discogs styles** (default). A genre you've already set is never proposed for change.
- **Replace Genre with the Discogs styles.** The change is still reviewed like any other overwrite.
- **Never propose a genre.**

Whichever you choose, the styles are also offered for the comment, for review.

### Auto-apply

Auto-apply is off by default. When it's on, a result is written to the file during the run, without review, only if all of these are true:

- the match came from the audio fingerprint, not a text search or Discogs;
- the score is at least the threshold you set, between 0.92 and 1.00;
- every change fills an **empty** field.

Comment (sub-genre) changes and anything that would overwrite a value are never auto-applied. An auto-applied result appears under **Applied** and can be undone like any other. If its write fails, for example because the file is open in a player, it stays under **Needs review** with the reason as a warning. If the write was interrupted partway, the result waits until the next start, which settles it (see [Safety](#safety)).

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

Open the review queue with **Review Identified Tracks**. Filter it by **Needs review**, **Unidentified**, **Applied** (results you can still undo) or **All**. Each row shows the score and where the match came from: **Fingerprint**, **Text search** or **Discogs**.

Choose a row to see:

- **Current against proposed** for every field, with a checkbox for each. Untick anything you don't want.
- **rekordbox's value**, under any field where it differs from the file.
- **Edit**, to correct a proposed text value before it's written. The sub-genre and artwork can't be edited. BPM and key are never proposed.
- **Candidates:** every release considered, with its score and source.
- **Artwork**, offered only when the file has none, with its size and source.

Then:

- **Accept** writes the ticked fields to the file.
- **Reject** dismisses the result; nothing is written.
- **Accept above** accepts every pending result scoring at least the threshold you choose (0.92 by default). Nothing is written yet.
- **Apply accepted** writes accepted results after a confirmation, and **only fills empty fields**. A result with any overwrite or comment change is skipped entirely, and the confirmation says how many were skipped. Apply those from their own review.

## Undo

Results under **Applied** can be undone, but only if the file hasn't changed since identification wrote it. If something else has written to it since, undo is refused rather than overwriting that change.

What undo restores depends on the format:

- **MP3, AIFF and WAV (ID3 tags):** the exact original tag bytes.
- **FLAC, MP4/M4A and OGG:** the original value of each changed field. The tag block itself is rewritten, so its layout can differ from before.

## Safety

Before any write:

- The file's original tags are saved in the identify database.
- The file must still hold the tags you reviewed. If it has changed since, the write is refused; identify the track again.
- Read-only files, and files another program has open (in practice, a track loaded or playing in a player), are refused with a message saying which. Close or unload the file and try again.

The new tags are written to a temporary copy in the same folder, which then replaces the original. For MP3, AIFF and WAV, only the changed tag frames are rewritten, and the audio and all other frames are kept byte for byte. For FLAC, MP4/M4A and OGG, the audio is kept and the tag block is rewritten with every field it could read.

If rbxport or the computer stops in the middle of a write, the next start checks each interrupted file:

- **The file still holds its old tags:** the write is marked as not done.
- **The file holds the new tags:** the write is marked as done.
- **The file holds something else:** the write is closed as unknown and the result goes back to **Needs review**. Its original tags are kept.
- **The file can't be found or read:** for example, it's on a drive that isn't connected, or a player has it open. The write stays open, and the track can't be reviewed or written, until a later start finds the file again.

Any leftover temporary file is removed. The audio file itself is never rewritten during this check.

## Where things are kept

Everything lives in rbxport's data folder, under `rbxport\identify\` (on Windows, `%APPDATA%\rbxport\identify\`):

| File | Contents |
|---|---|
| `identify.db` | Results, candidates, the copied rekordbox records, the original tags used for undo, and history |
| `.env.identify` | Your service keys |
| `settings.json` | Genre policy and auto-apply settings |

Deleting the folder resets identification, without touching rekordbox or your music files. It also deletes your saved keys, and the original tags behind every applied change, so those changes can no longer be undone.
