# XDJ-1000 mk1 freeze on rbxport USB exports

Status: open. Round 1 handed to the user (2026-11-07). No release until the root cause is backed by evidence.

## The symptom

- **Player:** Pioneer XDJ-1000 (mk1), firmware 1.13.
- **Freezes:** an rbxport export of a 353-track playlist (an intelligent playlist, exported as a plain playlist). The player locks up completely as it reads the stick, at or right after "My Settings found, press MENU to load". BACK does nothing, and only power-cycling recovers it.
- **Works:** rekordbox 7.2.14's export of the same 353 tracks (stick F).
- **Freeze point:** still to be confirmed exactly (see Round 1).

## Sticks

| Label | Stick | Layout | Contents | Player |
|---|---|---|---|---|
| **F** | SanDisk 3.2 Gen1, 114.5 GB (128 GB) | **MBR**, one FAT32 (LBA, 0x0C) partition at 16 KB; FAT32 hidden sectors 32; 32 KB clusters; no volume label; formatted by Windows (MSDOS5.0) | rekordbox 7.2.14 export | **works** |
| **G** | Samsung Flash Drive, 239 GB (256 GB) | **No partition table (superfloppy):** FAT32 starts at sector 0, hidden sectors 0; 32 KB clusters; no volume label; formatted by Windows (MSDOS5.0) | rbxport exports (v1.2.6, v1.2.9, v1.2.11) | **freezes** every time |

Every rbxport export tested so far went onto G, and every rekordbox export onto F. **The stick and its layout have never been separated from the export.** Snapshots of both sticks' `PIONEER` folders: `C:\dev\xdj-investigation\` (F-rekordbox, G-rbxport-1.2.11), with the scripts used.

## Ruled out, with evidence

1. **Audio codecs and containers.** Both sticks hold the same 353 files with the same size multiset: 315 AIFF 16-bit, 1 AIFF 24-bit, 1 AIFF **32-bit** (`ElezD - Quatro (Original Mix).aiff`) and 36 MP3 (MPEG-1 Layer III), all 44.1 kHz stereo. rbxport copies the bytes as rekordbox does (`audio_formats.py`). The 32-bit AIFF is outside the mk1's listed formats, but it is on the working stick too, so it does not explain a freeze on reading the stick. It may still fail when loaded.
2. **My Settings files.** `MYSETTING.DAT`, `MYSETTING2.DAT`, `DJMMYSETTING.DAT` and `djprofile.nxs` are byte-identical on both sticks; rbxport copies rekordbox's own. `DEVSETTING.DAT` differs only in the version string (7.2.11 vs 7.2.14); its body and CRC are identical.
3. **File-name length** (fixed in v1.2.9: names cut to rekordbox's 48 characters). It still froze.
4. **Page row-count word past 255 rows** (fixed in v1.2.11: `0x18`–`0x1a` now packed as rekordbox packs it, `1c 81 23` for 284 rows). It still froze.

## Differences remaining, ranked by how likely each is to freeze the player as it reads the stick

1. **No partition table on G (superfloppy), and a 256 GB stick.** These are a property of the stick, not of rbxport, and are present on every failing run and absent on the working one. An older player's USB mass-storage or FAT code is the most likely to choke on a volume without an MBR, or on a large FAT (61,174 sectors per FAT on G against 29,320 on F). **Highest:** it would explain why no export-side fix changed anything. Round 1 tests it.
2. **`export.pdb` page bookkeeping.** rekordbox's data pages carry `0x20 = 1`, `0x22 = rows − 1` (pages it appends to), while rbxport writes `0x20 = rows`, `0x22 = 0` (the style rekordbox uses for tables it writes in one go, like colours). The page sequence numbers and table empty-candidate pages differ too. Both styles occur in rekordbox's own file.
3. **`export.pdb` track rows.** rbxport leaves these zero or empty where rekordbox fills them:
   - `0x02` index_shift (rekordbox: 0x20 per row in the page);
   - `0x14` (a per-track value);
   - string slots 2 and 3 (`"4"`, `"2"`, `"5"`), 6 (`"ON"`) and 15 (analysis date).

   ISRC is plain short ASCII in rbxport, and rekordbox's form in rekordbox (`0x90`, length, `0x03`, ASCII, `0x00`). Remixer and original-artist IDs are 0, with 17 fewer artist rows.
4. **Files on the stick.**
   - rekordbox writes and rbxport doesn't: `PIONEER/rekordbox/RBFLTR.DAT` (232 bytes, dated 2014) and `PIONEER/extracted/gcred.dat`.
   - rbxport writes and rekordbox doesn't: `PIONEER/rbxport/manifest.json` (697 KB), `PIONEER/rekordbox/playlists3*.sync` and `/.rbxport-write.lock`.
5. **ANLZ `.EXT` tag order** (every file). rekordbox: `PPTH PWV3 PCOB PCOB PCO2 PCO2 PQT2 PWV5 PWV4 PSSI`. rbxport: `PPTH PWV3 PQT2 PWV5 PWV4 PSSI PCOB PCOB PCO2 PCO2`. The tag set and lengths are the same. This is **lower for a freeze on reading the stick**, since a player reads ANLZ on loading a track, and **high if the freeze is on track load**. `.DAT` sequences are identical. `.2EX` lacks `PVDI` in 338 files (the mk1 doesn't read `.2EX`).

Still to do: parse both `export.pdb` files with an independent parser (crate-digger's `rekordbox_pdb.ksy`) and compare every table. So far the comparison used rbxport's own reader plus a raw decode of the page headers.

## Round 1: separate the stick from the export

One test, one variable. It decides whether item 1 above is the cause.

**Stick G, with a rekordbox export:**

1. On the PC, delete **everything** on G (select all, delete). Don't reformat it: the layout under test must stay as it is.
2. In rekordbox, export the **same playlist** to G.
3. Plug G into the XDJ-1000.

**Report back:**
- **Did it freeze?** Yes or no.
- **If yes, exactly when:** as the stick is inserted, on pressing the USB source button, at the My Settings prompt, on opening a playlist, or on loading a track. What the screen shows, and whether pulling the stick recovers it or it needs a power cycle.
- **If no:** open the playlist and load two tracks, including `ElezD - Quatro (Original Mix)`, the 32-bit file, and say what happens.

**What each answer means:**
- **Freezes:** it's the stick or its layout, not rbxport. Next round: G re-laid out with an MBR partition, then rbxport on F.
- **Works:** it's rbxport's export. Next round: hybrid sticks built from these two exports (rekordbox's `export.pdb` with our ANLZ files and the reverse, then the extra files added or removed), each halving what is left.
