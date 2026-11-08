# XDJ-1000 mk1 freeze on rbxport USB exports

Status: open. Round 1 handed to the user (2026-11-07). No release until the root cause is backed by evidence.

## The symptom

- **Player:** Pioneer XDJ-1000 (mk1), firmware 1.13.
- **Freezes:** an rbxport export of a 353-track playlist (an intelligent playlist, exported as a plain playlist). The player locks up completely as it reads the stick, at or right after "My Settings found, press MENU to load". BACK does nothing, and only power-cycling recovers it.
- **Works:** rekordbox 7.2.14's export of the same 353 tracks (stick F).
- **Freeze point** (user, 2026-11-07, stick G with rbxport v1.2.11's export): the player boots normally with the stick in or out. On pressing **USB**, the "My Settings data" screen appears and the player freezes there completely. **Same on both of the user's XDJ-1000 mk1 units**, so it is not one unit's fault.
- What that rules in: the player is still at mount and first read. It has found and read the My Settings files (byte-identical on both sticks) and is reading the rest of the stick, `export.pdb` most likely, behind that screen. Track loading (ANLZ, audio) has not started, so the `.EXT` tag order (item 5 below) and the audio formats are unlikely.

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
5. **ANLZ `.EXT` tag order** (every file; now low, since the freeze comes before any track loads). rekordbox: `PPTH PWV3 PCOB PCOB PCO2 PCO2 PQT2 PWV5 PWV4 PSSI`. rbxport: `PPTH PWV3 PQT2 PWV5 PWV4 PSSI PCOB PCOB PCO2 PCO2`. The tag set and lengths are the same. This is **lower for a freeze on reading the stick**, since a player reads ANLZ on loading a track, and **high if the freeze is on track load**. `.DAT` sequences are identical. `.2EX` lacks `PVDI` in 338 files (the mk1 doesn't read `.2EX`).

Still to do: parse both `export.pdb` files with an independent parser (crate-digger's `rekordbox_pdb.ksy`) and compare every table. So far the comparison used rbxport's own reader plus a raw decode of the page headers.

## Round 1: separate the stick from the export

One test, one variable. It decides whether item 1 above is the cause.

**Built by copying (2026-11-07, user approved wiping both sticks).** Both sticks were first backed up in full to `C:\dev\xdj-investigation\full\F` and `full\G` (2,729 files and 22.57 GB each). `swap.ps1` then mirrored the backups across with robocopy `/MIR`; neither stick was reformatted, and `PIONEER` was hash-checked after the copy.

| Stick | Layout (unchanged) | Now holds |
|---|---|---|
| **G** | Samsung 256 GB, superfloppy | **rekordbox 7.2.14's export**, byte for byte from F |
| **F** | SanDisk 128 GB, MBR | **rbxport v1.2.11's export**, byte for byte from G |

The only thing a copy does not reproduce is the order the files were written in on the FAT. Both swaps give an answer in one trip, tested one stick at a time:

- G freezes and F works: **the stick or its layout.**
- G works and F freezes: **rbxport's export.**
- Both work: the write order or file timing of a real export matters (unlikely, but it would itself be the lead).
- Both freeze: more than one cause, or the players' state.

Test steps:

1. Plug **G** into one XDJ-1000 and press USB.
2. Then plug **F** in and press USB.

**Result (user, 2026-11-07): G loaded fine; F froze as before.** The freeze follows rbxport's export onto a stick that works with rekordbox's. **The stick, its size and the superfloppy layout are ruled out** [OBS]; item 1 of the ranking is closed.

## Export file-by-file diff (`diff_trees.py`, full backups)

- Identical: all 1,304 Artwork files, the My Settings files, 330 of 353 audio files, and the ANLZ folders of the matching tracks (331 files).
- **23 audio paths differ.** In names cut to 48 characters rekordbox keeps the space before the extension (`… (Original .aiff`), where rbxport trims it. rekordbox numbers duplicate names `-1`, `-2`, `-3`, where rbxport uses the track ID (`-159`). It writes a folder name's trailing dot as `_` (`Anna V_`), where rbxport drops the dot. The 69 ANLZ files of those tracks sit in different folders to match. These are read when a track loads, not as the stick mounts. [OBS] Worth matching later, but they don't cause this freeze.
- Different bytes on a shared path: `export.pdb`, `exportExt.pdb`, `exportLibrary.db`, `DEVSETTING.DAT` and 660 ANLZ files.
- Only rekordbox's: `RBFLTR.DAT`, `extracted/gcred.dat`, `exportLibrary.db-shm` and `-wal`. Only rbxport's: `rbxport/manifest.json`, `playlists3.sync`, `playlists3Plus.sync`, `/.rbxport-write.lock`.

## Round 2: databases against everything else

Both sticks start as rekordbox's working export, mirrored from the backup. Each then takes one half of rbxport's mount-time differences (`round2.ps1`, hash-checked):

| Stick | rekordbox's export, plus rbxport's |
|---|---|
| **G** | `export.pdb` and `exportExt.pdb` |
| **F** | `DEVSETTING.DAT`, `exportLibrary.db`, `playlists3*.sync`, `rbxport/manifest.json` and `.rbxport-write.lock`, without `RBFLTR.DAT`, `extracted/` or `exportLibrary.db-shm`/`-wal` |

Audio and ANLZ stay rekordbox's on both, since the freeze comes before a track loads. Whichever stick freezes holds the cause.

**Result (user, 2026-11-07): G froze; F worked.** The freeze is in **rbxport's databases** (`export.pdb` or `exportExt.pdb`) [OBS]. DEVSETTING, OneLibrary, the extra and missing files, ANLZ and audio are all ruled out for this freeze.

### The player writes to `export.pdb`

Building Round 3, the hash check found that F's `export.pdb` no longer matched rekordbox's backup, though its size and date did (robocopy skipped it for that reason; it has been restored and the check now passes). The changed file is saved as `player-written/round2-F-export.pdb`. **The XDJ changed exactly one field:** the file header's sequence at `0x14`, from 2026 to 2028. Nothing else changed, and no history rows were written [OBS, XDJ-1000 fw 1.13].

So the player **writes** to the database, not only reads it, and it does so at mount or while browsing. A freeze in that write path is possible, which makes the file header and the page bookkeeping (empty candidates, next-unused page) more likely suspects than the row contents.

### Independent walk of both `export.pdb` files (`pdb_check.py`)

`pdb_check.py` was written from Deep Symmetry's published format notes, not rbxport's code (installing rekordcrate was blocked by the session's safety check). Both files' page chains run cleanly from first to last page. The type, index and present flags match the header row counts in both. Its free/used arithmetic flags nearly every page in **both** files equally, so that check is my misreading of those fields, not a lead. The differences that remain:

| | rekordbox 7.2.14 | rbxport v1.2.11 |
|---|---|---|
| header `0x10` | **5** | **1** (copied from a blank 7.2.11 export) |
| header sequence `0x14` | 2026 | 58 |
| pages in file / next unused | 106 / 107 | 94 / 98 |
| empty candidates past end of file | 1 (tracks: 106) | **4** (artwork 94, columns 95, unk17 96, unk18 97) |
| tracks data pages | 45; 4 with flags `0x34` and `0x20`/`0x22` = 8191 | 37, all flags `0x24` |
| history page flags | `0x34`, `0x20` = 2 | `0x24`, `0x20` = rows |
| data page `0x20`/`0x22` | 1 / rows − 1 (written-row style) | rows / 0 (bulk style) |
| row group word after the present flags | one bit (last row written) | a copy of the present flags |

## Round 3: `export.pdb` against `exportExt.pdb`

`round3.ps1`: both sticks are rekordbox's export plus exactly one of rbxport's database files, hash-checked (every other non-audio file is rekordbox's).

| Stick | rekordbox's export, plus rbxport's |
|---|---|
| **G** | `export.pdb` |
| **F** | `exportExt.pdb` |

**Report back:**
- **Did it freeze?** Yes or no.
- **If yes, exactly when:** as the stick is inserted, on pressing the USB source button, at the My Settings prompt, on opening a playlist, or on loading a track. What the screen shows, and whether pulling the stick recovers it or it needs a power cycle.
- **If no:** open the playlist and load two tracks, including `ElezD - Quatro (Original Mix)`, the 32-bit file, and say what happens.

**What each answer means:**
- **Freezes:** it's the stick or its layout, not rbxport. Next round: G re-laid out with an MBR partition, then rbxport on F.
- **Works:** it's rbxport's export. Next round: hybrid sticks built from these two exports (rekordbox's `export.pdb` with our ANLZ files and the reverse, then the extra files added or removed), each halving what is left.
