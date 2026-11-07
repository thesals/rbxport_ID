# Installing on a Mac

rbxport ID runs on macOS 11 (Big Sur) and later, on Apple silicon and Intel Macs. The same download works on both.

The app isn't signed by Apple, because that needs a paid Apple developer account. macOS therefore refuses to open it the first time, and you allow it once. After that it opens like any other app and updates itself.

## Install

1. Open the [latest release](https://github.com/thesals/rbxport_ID/releases/latest) and download the file ending in **`.dmg`**.
2. Open the `.dmg` and drag **rbxport** into **Applications**.
3. Open **rbxport** from Applications. macOS says it can't verify the app. Click **Done** (or **OK**), not **Move to Trash**.
4. Open **System Settings → Privacy & Security**. Scroll down to the message about rbxport and click **Open Anyway**. Enter your password if asked.
5. Open rbxport again and click **Open**.

If macOS says the app **"is damaged and can't be opened"**, it's the same block reported a different way. Open **Terminal**, run this once, then open the app again:

```sh
xattr -cr /Applications/rbxport.app
```

## Updates

rbxport checks for a new version when it starts, and from **rbxport → Check for Updates…**. Updates install without going through Privacy & Security again.

## Where rbxport keeps things

| What | Where |
|---|---|
| Library | `~/Library/Application Support/rbxport/library/` |
| Logs | `~/Library/Application Support/rbxport/logs/` |
| rekordbox's library (only read, never written) | `~/Library/Pioneer/rekordbox/` |

## Testing checklist (for whoever tries it first)

The Mac build is new and has not been tried on a real Mac. Please report what happens at each step, with a screenshot of anything that looks wrong, and send the newest file from the logs folder above.

1. The install steps above: which message macOS showed, and whether **Open Anyway** worked.
2. The first start: **Create a new library**, or **Import from rekordbox** if rekordbox is on the Mac.
3. Browsing: tracks show, playlists open, search works, and a track plays. Then play a track on each of two decks at once, both loud, and listen for crackle or distortion. The automated audio test for this is skipped on Mac builds.
4. Analysis: analyse a few tracks, and check that the waveform and BPM appear.
5. Intelligent playlists: right-click **Playlists → Create New Intelligent Playlist**, then make one with a genre and a BPM range. It should fill with tracks.
6. Identify, if it's set up: identify a few tracks and review the results.
7. USB export to a spare stick, never one that matters. Export a playlist and check that the stick reads on a player, if one is available.
8. The next update: after a new release, the app should offer it and install it.
