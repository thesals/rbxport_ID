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

**Result (user, 2026-11-07): G froze; F worked.** The freeze is in **rbxport's `export.pdb`** [OBS]. `exportExt.pdb` is ruled out.

### Page counters, rekordbox against rbxport (`pdb_groups.py`)

On every data page of the tables it writes row by row, rekordbox 7.2.14 writes:
- `0x20 = 1`;
- `0x22` = the index of the **last row written**: 283 on a page of 284 rows, 7 on a page of 8, and other values where rows went in out of order;
- in each row group, the word after the present flags is that row's bit, and 0 in every other group.

rbxport writes `0x20` = rows and the word as a copy of the present flags. It writes `0x22` = 0 unless the page has more than 255 rows, and then it writes **`0x22` = rows**. That rule (`build.rs`, `finish_with`) follows the older "row count when over 255" reading of the field. rekordbox's file says the field is an index.

**On the 283-row `playlist_entries` page, rbxport's `0x22` = 283 names a row that doesn't exist (the last is 282).** A 353-track playlist always fills one page past 255 rows, so every export of it has carried this, before and after v1.2.11. This is now the leading suspect [ASSUME until Round 4].

## Round 4: the last-row index

`pdb_patch.py` writes patched copies (never in place), and `round4.ps1` puts each on rekordbox's export, hash-checked:

| Stick | rekordbox's export, plus rbxport's `export.pdb` patched |
|---|---|
| **G** | `last-row`: **one byte**. `0x22` on the 283-row page set to 282 |
| **F** | `row-style`: all 51 row-by-row data pages in rekordbox's style (`0x20` = 1, `0x22` = rows − 1, row-group word = last row's bit); includes G's change |

- G works: **`0x22` pointing past the last row is the cause.**
- G freezes, F works: the cause is rekordbox's page style, but more than that one value.
- Both freeze: the cause is in the file header or the page bookkeeping (header `0x10`, empty candidates past the end, next-unused page), or in row contents. Those come next.

**Result (user, 2026-11-07): both froze.** Rekordbox's page style is ruled out, the last-row index included [OBS]. rbxport's `0x22` = rows is still wrong against rekordbox, but it does not cause this freeze; fix it anyway when the writer is next changed.

## Round 5: the file's bookkeeping

rbxport's header names empty-candidate pages 94 to 97 and next-unused page 98, in a file of 94 pages. Four pages the header relies on are past the end of the file. rekordbox's names one, page 106, the page right after its file ends. The player writes to the file at mount, so it may read or write a page that isn't there.

| Stick | rekordbox's export, plus rbxport's `export.pdb` patched (`round5.ps1`) |
|---|---|
| **G** | `pad`: four zeroed pages appended, so the file holds every page through 97 |
| **F** | `pad,header5,row-style`: the same, plus header `0x10` = 5 and rekordbox's page style |

- G works: the **empty candidates past the end of the file** are the cause.
- G freezes, F works: the cause is header `0x10` (the page style is already ruled out).
- Both freeze: every structural difference between the files is ruled out. **The cause is in the rows' contents** (track-row fields, strings, the ISRC encoding, table sizes and rows per page) or in the table layout. That is next.

**Result (user, 2026-11-07): both froze, but G got further.**
- **G** (`pad` only) went **past the My Settings screen** and froze on the next screen, the browser for folders and playlists.
- **F** froze at My Settings as before.

**Padding the file to cover every empty candidate moves the freeze** [OBS]. rbxport's file stopping short of the pages its header names is **a real cause, the first of at least two.** (`build.rs` `finish` drops trailing zeroed pages on purpose, from an [OBS 7.2.11] blank export. That observation still holds, but the XDJ-1000 does not cope with it.)

### What the player wrote (`pdb_diff.py`, `player-written/`)

- rekordbox's file, Round 2, worked: sequence +2 and `0x10` unchanged at 5.
- Round 5 F, froze at My Settings: **`0x10` 5 → 4, sequence +1**, and nothing else.
- rbxport's unpatched file, Round 2 G, froze at My Settings: **nothing written** (hash unchanged).

`0x10` is not a page count: both files account for every page in their chains or empty candidates, with none loose. The pattern fits a **use counter or "stick open" marker**: the player lowers it when it starts using the database and restores it (with a second sequence bump) when it is done. [ASSUME, from three samples.] On F it froze between the two writes. rbxport writes 1 there, so the player would lower it to 0.

The user confirmed that F froze at the My Settings screen. **With the padding in, F still froze early, so `header5` or `row-style` brings back an early freeze** on the XDJ, although rekordbox's own file carries both. Not explained yet; neither goes into a fix until it is.

**Stick G stopped enumerating after Round 5:** Windows lists it as "Unknown USB Device (Port Reset Failed)". The stick's USB controller isn't answering, which no file on it can cause; it is likely hung after the player was power-cycled with it in. Until it comes back, rounds run on F alone.

### Row by row (`pdb_rows.py`)

Byte-identical rows in both files: genres, labels, keys, colors, **playlist entries**, artwork, **columns**, **unk17**, and 16 of 17 unk18 rows. Different: tracks (all), artists and albums (nearly all), the one playlist-tree row, one unk18 row (`15 00 07 00 01 …`, where rbxport writes `… 03 …`) and the **history** table.

**The history table (type 19)**, on pages 39 and 40 in both files:
- **rekordbox** fills its data page: **96 row slots of 40 bytes**, 3,840 bytes used and 0 free, flags `0x34`, one row present (slot 1) and 95 set aside. The player left it unchanged in Round 2.
- **rbxport** writes one slot and one row, with 4,010 bytes free.

The browser screen where Round 5 G froze is where the player shows the HISTORY folder. It's also where it would begin recording the session's history, into space rekordbox had reserved and rbxport hadn't.

## Round 6: the history table (stick F only)

`round6.ps1`: rekordbox's export, plus Round 5 G's padded `export.pdb` with **rekordbox's history pages 39 and 40 copied verbatim** (`history-from`). The pages' sequence numbers are set to the header's 58, so no page is newer than the file. Nothing else differs from Round 5 G.

- Gets past the browser screen: **the history table's layout is the second cause.** The fix is to pad the file to its empty candidates and lay out history as rekordbox does.
- Freezes on the browser screen: history is ruled out. Next are the playlist-tree row, the unk18 row, then tracks, artists and albums.
- Freezes at My Settings: the copied pages themselves break something.

**Result (user, 2026-11-07): froze at My Settings.** G enumerates again in a different port.

### What the player wrote, Rounds 5 and 6

| Stick | `export.pdb` written by the player | `exportExt.pdb` (rekordbox's) written by the player | froze at |
|---|---|---|---|
| Round 5 G: pad | nothing | `0x10` 5 → 4, sequence 14 → 15 | browser screen |
| Round 6 F: pad + history pages (sequence set to 58) | **page 39's sequence 58 → 0**, nothing else | `0x10` 5 → 4, sequence 14 → 15 | My Settings |

**The player "opens" every database at mount** [OBS]. It lowers header `0x10` (5 → 4) and bumps the header sequence, and on a clean session it later restores both (Round 2: rekordbox's `export.pdb` sequence +2, `0x10` back at 5). On rbxport's `export.pdb`, whose `0x10` is 1, it never writes the header at all.

**Page sequences against the header sequence:**

| File | header seq | highest page seq | pages with seq ≥ header |
|---|---|---|---|
| rekordbox `export.pdb` | 2026 | 2016 | none |
| rekordbox `exportExt.pdb` | 14 | 10 | none |
| rbxport `export.pdb` | 58 | 58 | **page 38 (unk18)** |
| rbxport `exportExt.pdb` (worked, Round 3) | 3 | 2 | none |

rekordbox's header sequence is always **ahead** of every page's. rbxport's `finish` writes "the sequence the pages reached", so its last page equals the header. In Round 6 I gave the history pages the header's sequence, and the player zeroed one of them, then froze earlier than Round 5 G. The pattern fits a reader that treats a page **not older than the header** as an unfinished write and tries to roll it back. [ASSUME, now the leading suspect for what is left.] It would also explain why Round 6's result can't clear the history table: that round added two more such pages.

## Round 7: the header sequence ahead of every page

`round7.ps1`, both on rekordbox's export, against Round 5 G's file (`pad`):

| Stick | Round 5 G's padded `export.pdb`, plus |
|---|---|
| **G** | `seq-ahead`: header sequence 58 → 68 (one byte) |
| **F** | `seq-ahead,header5`: the same and header `0x10` 1 → 5 |

- G works: **the header sequence not being ahead of every page is the second cause.**
- G freezes on the browser screen and F works: `0x10` matters once the sequence is right.
- Both freeze on the browser screen: the sequence is ruled out; back to the history table (copied with its own sequences) and the remaining rows.

**Result (user, 2026-11-07): both froze at My Settings.** The player's writes:
- **G** (`pad,seq-ahead`): `export.pdb` untouched, `exportExt.pdb` opened (`0x10` 5 → 4). That is exactly Round 5 G's pattern, though Round 5 G froze a screen later.
- **F** (`pad,seq-ahead,header5`): both files opened (`0x10` 5 → 4, sequence +1), and neither closed.

**Correction: the screen a stick freezes on is not a reliable progress measure.** Round 5 G and Round 7 G left identical writes. Most likely a button press moved the screen on just before the hang. The player's writes are the objective measure, so the Round 5 claim that padding "moves the freeze" is withdrawn; padding alone is not shown to help [OBS]. Across Rounds 5 to 7:

| Write pattern | Files |
|---|---|
| `export.pdb` never opened | every file with rbxport's `0x10` = 1 |
| opened and never closed (hang) | rbxport's file with `0x10` = 5, padded, with or without the page style or sequence change |
| opened and closed (works) | rekordbox's file |

So the hang comes **after the player opens `export.pdb`**, while it reads the tables. The structural patches (page style, padding, sequence, header) each leave it hanging, so the cause is in the **tables' contents**.

## Round 8: whole tables, rekordbox's file with rbxport's tables in

`pdb_transplant.py` takes rekordbox's working `export.pdb` and writes rbxport's data pages for the chosen tables into the page slots rekordbox's own chain uses. It rewrites each page's index and next pointer, keeps rekordbox's index pages, empty candidates and header (`0x10` = 5, sequence 2026, ahead of every page), and zeroes leftover slots (8 in tracks and 1 in artists). Rows checked with `pdb_rows.py`: the transplanted tables match rbxport's row for row, and every other table matches rekordbox's. Track, playlist and artwork IDs agree between the two files (playlist entries and artwork are byte-identical), so the cross-references hold.

The six tables whose rows differ, split in two (`round8.ps1`):

| Stick | rekordbox's `export.pdb`, with rbxport's |
|---|---|
| **G** | tracks, artists, albums (types 0, 2, 3) |
| **F** | playlist tree, unk18, history (types 7, 18, 19) |

Whichever hangs holds the cause. The measure is the player's writes, read back from the stick (closed means it worked), together with what the user sees.

**Result (user, 2026-11-07):**
- **G froze at My Settings.** The player opened `export.pdb` (`0x10` 5 → 4, sequence +1) and never closed it.
- **F loaded the playlist and played two tracks.** `export.pdb` was opened and closed (sequence +2, `0x10` back at 5).

**The cause is in rbxport's tracks, artists or albums table** [OBS]. Playlist tree, unk18 and history are ruled out. So are page layout, padding and the header: F carried rbxport's pages for its three tables in rekordbox's file, and it worked.

### Track rows field by field (`pdb_tracks.py`, decoded from the published layout)

| Field | rekordbox | rbxport | tracks |
|---|---|---|---|
| `0x02` **index_shift** | **0x20 × the row's slot in its page** | **0** | 308 (all 353 rows are 0) |
| `0x14` | a per-track number | 0 | 353 |
| strings 2, 3 | `4`, `4` etc. | empty | 353 |
| analyze_date | `2025-06-29` etc. | empty | 353 |
| kuvo_public | `ON` | empty | 306 |
| ISRC | `0x90` form | short ASCII | 280 |
| artist_id, remixer_id | | numbered differently, all resolve in their own file | 335, 50 |
| UTF-16 strings | | length includes a trailing NUL | 6 |
| file name, path, ANLZ path | | the 23 renamed files | 21 to 27 |

**index_shift, all three failing tables:**
- rekordbox: 0, 32, 64… per page in tracks, artists and albums, and 32 for history's one row (slot 1).
- rbxport: **0 on every row of all three.**

The tables that worked are either rowless in this sense (genres, labels, keys, artwork and playlist entries have no such field) or hold one row in slot 0, where 0 is right (playlist tree, history). That lines up with the failing group exactly. The rule (`pdb_patch.py index-shift`) **changes 0 bytes when run on rekordbox's own file** across tracks, artists, albums and history, so it reproduces rekordbox exactly [OBS].

## Round 9: index_shift

On Round 8 G's hybrid (rekordbox's file, rbxport's tracks, artists and albums; froze), `round9.ps1`:

| Stick | Round 8 G's `export.pdb`, plus |
|---|---|
| **G** | index_shift = 0x20 × slot on tracks, artists and albums (1,196 bytes) |
| **F** | the same on tracks only (341 bytes) |

- G works: **index_shift is the cause.** F then says whether tracks alone is enough.
- Both freeze: index_shift is ruled out; next are `0x14`, the empty strings, the ISRC and UTF-16 encodings.

**Result (user, 2026-11-07): both still hang, but the behaviour changed.**
- **G** "lagged through My Settings", reached the browser, then froze.
- **F** "acted like it loaded a track with no data" when USB was pressed, then reached the browser and froze.
- **Writes, both sticks:** `export.pdb` and `exportExt.pdb` opened (`0x10` 5 → 4, sequence +1) and never closed.

**index_shift is necessary but not sufficient** [OBS: it changes what the player does, and it still hangs]. Keep it in the fix.

### Artists and albums against rekordbox's, matched by name

Apart from index_shift (and the order the rows come in), every artist and album row is identical to rekordbox's, except **3 album names in UTF-16 whose stored length counts a trailing NUL**. The same NUL turns up in 6 track strings. rekordbox never counts one (`utf16-nul` changes 0 bytes in its file).

## Round 10: the remaining track-row differences that can be patched in place

On Round 9 G's file (`round10.ps1`):

| Stick | Round 9 G's `export.pdb`, plus |
|---|---|
| **G** | `utf16-nul`: the 9 UTF-16 strings lose the NUL counted in their length (9 bytes) |
| **F** | `u14-from` rekordbox (each track's `0x14`, by ID) and `isrc-blank` (the 280 ISRCs emptied, as rekordbox writes 73 of its own) |

**Result (user, 2026-11-07): both froze at My Settings.** Writes: `export.pdb` opened (`0x10` 5 → 4, sequence +1) on both, and never closed.

### Correction: the Round 8 to 10 hybrids carried a stale index

rekordbox's tracks **index page** (page 1) is not filler. Its header has `0x26` = 13, its head ends in a count of 5, and its first entries are `(u16, u16)` pairs whose second half is a page number shifted left by 3: 92, 99, 100, 103 and 101, with `0x1fff`/`0xfff8` as filler. **Those are exactly rekordbox's five `0x34` track pages, and each has deleted row slots** (92: slot 8 free; 99: 5 and 9; 100: 5; 101: 1; 103: 1, 3, 5 and 7). The index lists pages with free slots, rekordbox's own leftovers from editing the table [OBS]. rbxport never deletes rows, so its empty index is right for its own file. That matches rekordbox's 61-track export, which `build.rs` cites.

`pdb_transplant.py` kept rekordbox's index page while writing rbxport's pages into those slots, or zeroing them. **In Rounds 8 G, 9 and 10, the index pointed the player at pages that no longer matched it**, which may be enough on its own to hang a reader that follows it. Their results are therefore weaker than recorded:
- Round 8's split still stands: F, with rekordbox's tracks and consistent index, worked; rbxport's own file, with consistent index, hangs.
- **The "still hangs" of Rounds 9 and 10 do not rule out index_shift, the NULs, `0x14` or the ISRC.**

From Round 11 on, no more hybrids. Sticks carry **rbxport's own export** (`full\G`: its paths, ANLZ files and index pages consistent) with only its `export.pdb` patched.

## Round 11: rbxport's own export, patched (`round11.ps1`)

| Stick | rbxport's export, its `export.pdb` with |
|---|---|
| **G** | `index-shift=0+2+3` only |
| **F** | `index-shift=0+2+3,header5,pad,seq-ahead,utf16-nul,u14-from,isrc-blank`: every in-place fix found so far |

- G works: **index_shift is the root cause.**
- G hangs and F works: one of the other in-place fixes is needed as well. Split them next.
- Both hang: the empty strings or the numbering, which needs a page rebuild.

rekordbox's values for the empty slots, for that rebuild:
- strings 2 and 3 are small numbers: `4`/`4` on 272 tracks, `2`/`2` on 47, `5`/`4` on 25;
- string 4 is `1` on 5 tracks;
- kuvo_public is `ON` on 306 tracks and empty on 47;
- analyze_date is the analysis date.

### The writer already has the tool

`FileBuilder::add_table_numbered` gives each row its index on the page, and `exportExt.pdb`'s tag table uses it (`ext_pdb.rs`: `row[2..4] = index * 32`). That is rbxport's `exportExt.pdb`, the file that worked in Round 3. `export.pdb`'s tracks, artists and albums go through plain `add_table` (`rbl-export/src/lib.rs`, `export_pdb`), so their index_shift stays 0. If Round 11 confirms it, the fix is to add tables 0, 2 and 3 with `add_table_numbered` and the same closure, plus a test against rekordbox's rule.

### Ready if Round 11 hangs on both: Round 12

`pdb_retrack.py` rebuilds rbxport's tracks pages with chosen string slots filled from rekordbox's file by track ID, using rbxport's own packing rules. **With nothing to fill, its output matches rbxport's file byte for byte**, apart from the padding to next-unused that it adds. Prepared in `round12/`:

| Stick | rbxport's `export.pdb` |
|---|---|
| **G** | tracks rebuilt with strings 2, 3 and 4, kuvo_public and analyze_date from rekordbox (1,370 strings; 38 pages, the new one at 98), index_shift on all three tables |
| **F** | the same, plus header5, seq-ahead, utf16-nul, u14 and isrc-blank |

Round 12 F differs from rekordbox's file only in:
- artist and remixer numbering;
- the blanked ISRCs;
- 25 date_added values;
- the 23 renamed files' paths, which match rbxport's files on that stick;
- index_shift values, which follow the same rule over a different packing.

Not patchable in place, so left for a page rebuild if these both hang:
- strings 2 and 3, analyze_date and kuvo_public, which rbxport leaves empty;
- the artist and remixer numbering.

**Round 11 result (user, 2026-11-08): both froze at My Settings.** F's writes: `export.pdb` opened (`0x10` 5 → 4, sequence 68 → 69) and never closed (`player-written/round11-F`). G was not plugged back in.

So on rbxport's own, consistent file, **index_shift is not sufficient**, and neither are header5, padding, sequence, the UTF-16 NULs, `0x14` and the ISRC together with it [OBS].

### What else differs (checked 2026-11-08)

- **Index pages:** identical in form in both files for every table except tracks and history, where rekordbox's list its pages with deleted slots. Not a difference for rbxport's own file.
- **Header and chains:** same 20 tables, same index pages, same first pages; rbxport's data pages sit in a different order and its empty candidates lie past the end of the file (tested with `pad`).
- **Row order:** rbxport writes tracks in ID order. rekordbox's are shuffled (1, 5, 2, 141, 3, 55, …) and run 7 to 9 to a page, the marks of rows rewritten in place.
- **Row slack:** every rekordbox track row ends with at least 40 spare bytes (40 to 517); rbxport's end within 3. Album rows also carry 4 to 9 spare bytes. Never tested.
- **Artists:** rekordbox has 256 (remixers included, and tracks' `remixer_id` set on 50); rbxport 239 with `remixer_id` 0.

## Round 12, revised: row content against page packing (`round12.ps1`)

Round 11 leaves two kinds of difference in tracks, artists and albums: what the rows say, and how rbxport packs them. Round 12 splits those.

| Stick | rbxport's export, its `export.pdb` with |
|---|---|
| **G** | as prepared above: tracks rebuilt with strings 2, 3, 4, kuvo_public and analyze_date from rekordbox, index_shift on all three tables |
| **F** | **rekordbox's own rows** for tracks, artists and albums (`pdb_rowswap.py`: each row's bytes, slack included, in ID order), packed by rbxport's rules into rbxport's page slots, plus header5 and seq-ahead |

Verified: F's 353 track, 256 artist and 239 album rows equal rekordbox's byte for byte apart from index_shift (re-set per slot); every other table is rbxport's. Tracks take 45 pages (8 appended at 98 to 105), artists 3 (one appended at 106). The previous F (G plus header5, seq-ahead, NULs, `0x14`, ISRC) is kept as `round12/F-export.fill-plus.pdb`.

F's track rows name rekordbox's file names, so the 23 renamed files will not load from F; any other track should.

- **F works:** the cause is in the row content. G then says whether the empty strings are it; if G hangs, the rest is numbering, remixers, slack or ISRC, split next.
- **F hangs:** rbxport's packing hangs the player even with rekordbox's rows: page fill, row order or the page header fields (`0x20`, `0x22`, group word) are next.
- **Both work:** the empty strings are enough.

**Round 12 result (user, 2026-11-08): F worked (playlist loaded, two tracks played). G froze at the browser.**
- **F:** `export.pdb` opened and closed (sequence 68 → 70, `0x10` back at 5). `exportExt.pdb` opened and closed too, and the player set its `0x10` from 1 to 5 (`player-written/round12-F`).
- **G was never rewritten for Round 12.** It still carried Round 11 G's file byte for byte, which the player did not write to (`0x10` = 1). So G repeated Round 11 G: the same file froze at My Settings last time and at the browser this time. The screen it freezes on is not a reliable measure; the writes are.

**rekordbox's rows for tracks, artists and albums, packed by rbxport's writer into rbxport's file, work** [OBS]. Page packing, row order, page header fields (`0x20`, `0x22`, group word), page placement, and every other table as rbxport writes them are ruled out. **The cause is in the content of rbxport's track, artist or album rows.**

## Round 13: the empty strings, from both sides (`round13.ps1`)

| Stick | rbxport's export, its `export.pdb` with |
|---|---|
| **G** | rbxport's rows, with strings 2, 3, 4, kuvo_public and analyze_date filled from rekordbox; index_shift, header5, seq-ahead, UTF-16 NULs, `0x14`, ISRCs blanked (`round12/F-export.fill-plus.pdb`) |
| **F** | Round 12 F's rekordbox rows, compacted (no slack), with **strings 2, 3, 4, kuvo_public and analyze_date blanked** as rbxport writes them; header5, seq-ahead |

Checked with `pdb_tracks.py` against rekordbox's file:
- F differs only in those five slots (and index_shift).
- G differs only in artist and remixer numbering (rbxport's 239 artists), the blanked ISRCs, 25 date_added values and the renamed files' paths.

What each result means:
- **F hangs, G works:** the empty strings are the root cause, seen from both sides.
- **F works, G hangs:** the empty strings are not it. The cause is in the artist and remixer numbering, the ISRC or date_added; split next.
- **Both hang:** the empty strings are one cause and something in G's list is another.
- **Both work:** the empty strings matter only together with something else that G fixes.

**Round 13 result (user, 2026-11-08): G froze at My Settings; F worked and played three tracks.** Writes: G opened `export.pdb` (`0x10` 5 → 4, sequence 68 → 69) and never closed it; F opened and closed it (sequence 68 → 70) (`player-written/round13-*`).

- **The empty strings are not the cause** [OBS]: rekordbox's rows with strings 2, 3, 4, kuvo_public and analyze_date blanked exactly as rbxport writes them, and with no slack, work.
- Row slack is ruled out with them (F's rows were compacted).
- G still hangs with those strings filled, so the cause is in what G kept of rbxport's: **the artists table with its numbering and the tracks' `remixer_id`, the albums table, 25 date_added values, or the renamed files' paths**. The blanked ISRC is ruled out too: rekordbox itself writes 73 empty ones, and F14 below carries 280.

## Round 14: artists against the rest (`round14.ps1`)

Both start from Round 13 F (works): rekordbox's rows, compacted, the five strings blanked, header5, seq-ahead.

| Stick | plus, from rbxport's file |
|---|---|
| **G** | the artists table (239 rows), and each track's `artist_id` and `remixer_id` (`pdb_rowswap.py 0+3 --fields=artist_id+remixer_id`) |
| **F** | the albums table, and each track's date_added, filename, file_path and analyze_path (`pdb_rowswap.py 0+2 --strings=…`), with `utf16-nul` and `isrc-blank` |

Checked against rekordbox's file: G's artists are rbxport's 239 rows byte for byte and its tracks differ only in the five blanked strings, artist_id and remixer_id. F's tracks differ only in those strings, the blanked ISRCs, date_added and the 23 renamed files' names and paths; its albums are rbxport's (3 NULs dropped).

- **G hangs, F works:** the cause is rbxport's artists: their numbering, the 17 missing remixer rows, or `remixer_id` = 0. Split next.
- **F hangs, G works:** albums, date_added or the renamed paths.
- **Both work:** the cause needs both halves together.

**Round 14 result (user, 2026-11-08): G worked and played tracks; F froze at My Settings.** Writes: G opened and closed `export.pdb` (sequence 68 → 70); F opened it (`0x10` 5 → 4, 68 → 69) and never closed it (`player-written/round14-*`).

- **rbxport's artists, their numbering, the 17 missing remixers and `remixer_id` = 0 are ruled out** [OBS].
- The cause is in F's half: rbxport's albums table, its date_added on 25 tracks, or its names and paths for the 23 renamed files.

### The backslash

Listing F's path differences track by track (2026-11-08) shows three tracks whose rbxport `file_path` contains a **backslash as a directory separator**:

```
159  /Contents/Abstract Division/UnknownAlbum\Abstract Division - Contemporary Spaces EP-159.aiff
160  /Contents/Abstract Division/UnknownAlbum\Abstract Division - Contemporary Spaces EP-160.aiff
161  /Contents/Abstract Division/UnknownAlbum\Abstract Division - Contemporary Spaces EP-161.aiff
```

These are the three tracks whose names collided with another track's, so rbxport gave them a `-<id>` suffix. The code that does it, `layouts()` in `crates/rbl-export/src/lib.rs`, builds the new path with `parent.join(&name)`, and on Windows `Path::join` inserts `\` [OBS: code and file]. Every other path is built with `format!` and `/`. The same backslash is in those tracks' ANLZ files (the UTF-16 path in `ANLZ0000.DAT`, `.EXT`, `.2EX`) and in `PIONEER/rbxport/manifest.json`. rekordbox names the same files `E-1`, `E-2`, `E-3` with `/` throughout. A macOS export would get `/`, because `Path::join` there uses `/`.

The other differences in F's half look harmless: the other 20 renames only drop a space before the extension, date_added is a day off on 25 tracks, and the album rows match rekordbox's by name.

## Round 15: the backslash alone (`round15.ps1`, `round15_fix.py`)

| Stick | |
|---|---|
| **G** | **rbxport's v1.2.11 export exactly as it wrote it**, with only those backslashes made `/`: 3 bytes in `export.pdb` and 1 byte in each of the 9 ANLZ files. No index_shift, header or any other patch. |
| **F** | Round 14 F (froze) with only its 3 backslashes made `/`, in place. |

Each stick differs from one that froze by the backslash alone.

- **Both work:** the backslash is the root cause [then OBS], and on its own. The fix: build the collision path with `/` in `layouts()`, plus a test that no exported path contains `\`.
- **G hangs, F works:** the backslash is a cause, and rbxport's own file has a second one that rekordbox's rows hid.
- **F hangs:** it's not the backslash; next are the albums table and date_added.

**Round 15 result (user, 2026-11-08): both hung.** F froze at My Settings: `export.pdb` opened (`0x10` 5 → 4, 68 → 69) and never closed. G showed a "Wait" circle, reached the browser and froze (no writes; its `0x10` is 1). **The backslash is not the cause** [OBS]. It is still a bug (Windows-only, in `layouts()`) and goes in the fix.

### Every stick so far against rekordbox's file

Differences from rekordbox's `export.pdb` (track counts, or row counts for albums and artists), by result:

| Stick | Result | u14 | remixer | artist_id | ISRC | strings 2–4, kuvo, analyze_date | date_added | paths, names | **album rows** | artist rows | `0x10` |
|---|---|---|---|---|---|---|---|---|---|---|---|
| rekordbox | works | 0 | 0 | 0 | 0 | 0 | 0 | 0 | **0** | 0 | 5 |
| R12 F | works | 0 | 0 | 0 | 0 | 0 | 0 | 0 | **0** | 0 | 5 |
| R13 F | works | 0 | 0 | 0 | 0 | 353 | 0 | 0 | **0** | 0 | 5 |
| R14 G | works | 0 | 50 | 335 | 0 | 353 | 0 | 0 | **0** | 250 | 5 |
| rbxport, R11 G, R15 G | hang | 353 | 50 | 335 | 280 | 353 | 25 | 23 | **3** | 250 | 1 |
| R11 F | hang | 0 | 50 | 335 | 280 | 353 | 25 | 23 | **3** | 250 | 5 |
| R13 G | hang | 0 | 50 | 335 | 280 | 0 | 25 | 23 | **3** | 250 | 5 |
| R14 F, R15 F | hang | 0 | 0 | 0 | 280 | 353 | 25 | 23 | **3** | 0 | 5 |

ISRC, date_added, paths and the 3 album rows all split the results perfectly, and those 3 album rows are the only difference with a mechanism:

### UTF-16 strings sit on a 4-byte boundary in rekordbox's file

Three albums have names that need UTF-16 (`Millésimes #1`, `Lost Love EP` with no-break spaces, `RITUALØ3`).

- **rekordbox** writes the name at **`0x18`** (near offset `0x15` = `0x18`), with 2 zero bytes after the `0x16` that ASCII names use.
- **rbxport** writes it at **`0x16`**, straight after the prefix (`rows.rs`, `album_row`, `ALBUM_NAME_AT = 0x16`).

Rows start on 4-byte boundaries, so rbxport's UTF-16 album names start 2 bytes past one. Across rekordbox's whole file, **every** `0x90` string is on a 4-byte page offset: 286 in tracks, 3 in albums. Short ASCII strings are not aligned (all 236 ASCII album names sit at ≡ 2). rbxport aligns 2 of its 6 UTF-16 track strings and 0 of its 3 album names [OBS]. A firmware reading the UTF-16 header's u16 length or its code units with aligned loads would fault or misread on an odd-halfword address.

Track UTF-16 strings seem not to matter on their own: the working R13 F and R14 G each carry 4 unaligned ones (compacting moved them). Artist rows put ASCII names at `0x0a`; this library has no UTF-16 artist name, so artists are untested.

## Round 16: the three album rows alone (`round16.ps1`, `pdb_rowswap.py --align-album --keep-shift`)

| Stick | |
|---|---|
| **G** | **rbxport's v1.2.11 export exactly as written**, except the 3 UTF-16 album names moved from `0x16` to `0x18` (2 zero bytes inserted, near offset changed). Pages 8 and 82 only; same length; no other patch; ANLZ files as rbxport wrote them. |
| **F** | Round 15 F (hung) with only the same 3 album rows changed. |

`pdb_rows.py`: 236 of 239 album rows and every other table byte-identical to the stick each one came from.

- **Both work:** the misaligned UTF-16 album name is the root cause [then OBS]. The fix: `album_row` pads before a UTF-16 name so it starts on a 4-byte boundary (`0x18`), the same rule applied wherever a row places a `0x90` string (tracks, artists), and a test against rekordbox's rule.
- **G hangs, F works:** the album rows are a cause, and rbxport's own track rows carry a second one (ISRC, date_added or paths).
- **Both hang:** not the album rows; ISRC, date_added and paths are next.

**Round 16 result (user, 2026-11-08): F worked without issue; G froze at the browser.** Writes: F opened and closed `export.pdb` (68 → 70); G none (`0x10` = 1) (`player-written/round16-*`).

**First root cause, proven [OBS]: rbxport writes UTF-16 album names 2 bytes off a 4-byte boundary.** Round 15 F hung and Round 16 F worked, and the only difference between them is those 3 album rows (`pdb_rows.py`: every other row byte-identical). rekordbox always aligns `0x90` strings.

**A second cause remains in rbxport's own file**, since G16 (rbxport's file, albums fixed) still hangs. F16 works with blanked ISRCs, rbxport's date_added and rbxport's names and paths, and earlier rounds cleared the artists, numbering, remixers, empty strings, slack and packing. What G16 still has that F16 does not:
- `0x10` = 1 (every working stick had 5; with 1 the player never writes the file);
- index_shift 0 on tracks and artists;
- `0x14` = 0 on every track;
- rbxport's own ISRC form (short ASCII; F16's were blank);
- the UTF-16 lengths that count a NUL (6 track strings, 3 album names).

## Round 17: the header against the rest (`round17.ps1`)

| Stick | Round 16 G's `export.pdb` (rbxport's, albums aligned), plus |
|---|---|
| **G** | `header5` only: 1 byte |
| **F** | every in-place fix: index-shift on 0, 2, 3; header5; pad; seq-ahead; utf16-nul; `0x14` from rekordbox; ISRCs blanked |

- **F works:** the fix set is sufficient on rbxport's own output. G says whether `0x10` is the second cause on its own.
- **G works:** `0x10` = 1 is the second cause.
- **F hangs:** something not patchable in place is left; rebuild and split next.

**Round 17 result (user, 2026-11-08): both froze at the browser** (G let the user start scrolling first). The in-place fixes are not the second cause [OBS].

### The second cause: the backslash, masked in Round 15

Working Round 16 F and hanging Round 17 F differ, in tracks, only in artist numbering (cleared by Round 14 G), index_shift (same rule, different packing) and **the 3 backslash paths**. F16's tracks came from Round 15, where the backslashes had been fixed; F17's came from rbxport's original file. Round 15 could not show the backslash, because both its sticks still carried the misaligned album names.

Model: **rbxport's file hangs the XDJ-1000 mk1 if a UTF-16 album name is misaligned *or* a track path contains a backslash.** Checked against every stick tested from Round 4 on (28 files):

| Sticks | Backslash paths | Misaligned UTF-16 albums | Result |
|---|---|---|---|
| rekordbox, R8 F, R12 F, R13 F, R14 G, R16 F | 0 | 0 | all work |
| rbxport, R4–R11 (all), R8 G, R13 G, R14 F | 3 | 3 | all hang |
| R15 F, R15 G | 0 | 3 | hang |
| R16 G, R17 G, R17 F | 3 | 0 | hang |

No stick contradicts it.

## Round 18: both causes, from both sides (`round18.ps1`)

| Stick | |
|---|---|
| **G** | **rbxport's v1.2.11 export with only the two fixes**: the 3 UTF-16 album names aligned and the 3 backslashes made `/` (in `export.pdb` and the 9 ANLZ files). No other patch. Predicted: works. |
| **F** | Round 16 F (worked) with **only the 3 backslashes put back** (3 bytes). Predicted: hangs. |

- **G works, F hangs:** both root causes proven; G shows the two fixes are sufficient for rbxport's own output and F shows the backslash is necessary on its own. Then fix the writer.
- **Anything else:** the model is wrong; re-split from the closest pair.

**Round 18 result (user, 2026-11-08): G froze at the browser; F worked without issue.** The opposite of the prediction. Writes: F opened and closed `export.pdb` (68 → 70); G none (`0x10` = 1).

- **The backslash is not a cause** [OBS]: F16 worked, and F16 plus the backslash still works. It stays in the fix as a Windows-only bug.
- **The album fix and the backslash fix together do not fix rbxport's own file** [OBS: G18].
- The two-cause model is withdrawn.

### Feature search over every stick

`feat.py` (scratch) measured 15 candidate features on all 28 tested files and searched for boolean models that fit every result. No single feature fits, and no OR of up to three. Only two models fit, and they say the same thing:

> hang = misaligned UTF-16 album name **or** (rbxport's artists, i.e. its numbering with the 17 remixers missing and `remixer_id` 0, **and** rbxport's date_added/names/paths, which have always moved together)

Every track gets the same artist name in both files, and rbxport's artists table has no duplicates, empty names or dangling references. No mechanism links the two halves of that conjunction, so it may be overfitted.

**A hidden variable is possible.** From Round 8 on, stick F worked 6 times and stick G once (Round 14 G). Every file has been tested once, on one stick. Round 1 showed the stick itself is not to blame for the original hang, but not that results never depend on the stick.

## Round 19: swap Round 18's files between the sticks (`round19.ps1`)

| Stick | |
|---|---|
| **G** | Round 18 F's `export.pdb` (worked on F) |
| **F** | Round 18 G's `export.pdb` (froze on G), with its 9 fixed ANLZ files |

- **G works, F freezes:** results follow the file; the player is deterministic per file. Split R18 G against R18 F next (`0x14`, ISRC form, artists and remixers, header, index_shift).
- **G freezes, F works:** results follow the stick. Every G result since Round 8 needs re-reading, and each future file must be tested on both sticks.
- **Both work or both freeze:** the player is not deterministic. Repeat each file to measure it.

**Round 19 result (user, 2026-11-08):**
- **G (R18 F's file) froze the first time**, when the stick went in with the XDJ already in USB mode. After a restart it **worked without issue**: `export.pdb` opened and closed (68 → 70).
- **F (R18 G's file) froze at the browser** as soon as scrolling started (no writes; `0x10` = 1).

**Results follow the file, not the stick** [OBS]: R18 F's file works on both sticks and R18 G's freezes on both. The procedure matters, though: a working file froze once when inserted with the player already in USB mode. A single "froze" is therefore weaker evidence than a single "worked". From Round 20 on, every checklist uses one routine: player powered on with no stick, stick in, then USB.

## Round 20: artist content against page layout (`round20.ps1`)

R17 F (froze) and R18 F (worked) differ in the artists table and each track's `artist_id`/`remixer_id`, and in how their track, artist and album pages were packed: R17 F's are rbxport's own pages patched in place, R18 F's were repacked by `pdb_rowswap.py`. The backslash was cleared in Round 18. Round 20 swaps the artist side:

| Stick | |
|---|---|
| **G** | R18 F (worked) with rbxport's artists table and rbxport's `artist_id`/`remixer_id` (`pdb_rowswap.py … 2`, `pdb_patch.py ids-from=`). Its tracks equal R17 F's apart from index_shift. |
| **F** | R17 F (froze) with rekordbox's artists table and rekordbox's `artist_id`/`remixer_id`. Its tracks equal R18 F's apart from index_shift. |

So G is R17 F's content in R18 F's layout, and F the reverse. One more difference rides along: album index_shift (G's albums 0, as R18 F's; F's set, as R17 F's).

- **G freezes, F works:** the cause is in the artist content.
- **G works, F freezes:** the cause is in how rbxport's pages are laid out, or in the album index_shift.
- **Both work:** the cause needs the artist content and rbxport's layout together.
- **Both freeze:** repeat both before reading anything into it.

**Round 20 result (user, 2026-11-08): G worked flawlessly; F froze at the browse screen, "specifically when the cursor hit the Album field".** Writes: G opened and closed (68 → 70). F's `0x10` went 5 → **1** (not 4 as in every earlier hang) and its sequence 68 → 69, never closed (`player-written/round20-*`).

- **The artist content is cleared** [OBS]: G carries rbxport's artists and numbering and works.
- **F's album pages are byte-identical to G's except each row's index_shift** (checked over every byte of pages 8, 82 and 83). F's are `0x20` × slot (from Round 17's `index-shift=0+2+3`), G's are 0. The tracks' pages also differ in layout.

### Rows per page and index_shift, every stick

| | track index_shift | album rows per page | album index_shift |
|---|---|---|---|
| rekordbox | set | ≤ 87 | set (max 2752) |
| R12 F, R13 F, R14 G (rekordbox's album rows) | set | ≤ 86 | set (max 2720) |
| R16 F, R18 F, R20 G | set | 98 | **0** |
| rbxport, R16 G, R17 G, R18 G | **0** | 98 | 0 |
| R11 G/F, R13 G, R17 F, R20 F | set | **98** | **set (max 3104)** |

The working sticks always have the track index_shift set, and either album index_shift 0 or album pages of at most 87 rows. rekordbox's album rows carry 4 to 9 spare bytes each, so they never reach 98 to a page; rbxport's compact rows do. A model that fits all 30 files:

> hang = misaligned UTF-16 album name **or** track index_shift all 0 **or** (album index_shift set **and** album pages beyond rekordbox's ~87 rows)

Artists tolerate a set index_shift at 144 rows to a page (R20 G works with 4576), so the third clause is album-specific, or this is a different mechanism.

## Round 21: the two index_shift clauses, each alone (`round21.ps1`)

| Stick | Round 20 G (worked), plus |
|---|---|
| **G** | track index_shift zeroed (`index-zero=0`; 340 bytes on the track pages only) |
| **F** | album index_shift set, `0x20` × slot on pages of up to 98 rows (`index-shift=3`; 423 bytes on pages 8, 82, 83 only) |

- **G freezes:** rbxport's index_shift of 0 on tracks is a cause (Round 9's "changed behaviour" fits).
- **F freezes, especially on Album:** album index_shift on rbxport's full pages is a cause. Next: where the limit is (rows per page or shift value).
- **Either works:** that clause is wrong.

**Round 21 result (user, 2026-11-08): both worked flawlessly.** Writes: both opened and closed (68 → 70).

- **Track index_shift of 0 is not a cause** [OBS: G21].
- **Album index_shift set on 98-row pages is not a cause** [OBS: F21]. The Round 20 model's second and third clauses are withdrawn.

### Where that leaves the evidence

- `pdb_rowswap.py 0 --compact --keep-shift` run on R18 G rebuilds rbxport's track pages **byte for byte**. rbxport's own layout is what the repacker produces, so layout is not a variable for R18 G.
- F20 (froze) and F21 (works) carry the same 21 strings in every track. They differ only in `remixer_id`, string offsets, and the unreferenced bytes left by in-place `isrc-blank`, which both carry in different forms.
- Every "froze" before Round 18 was a single try, and Round 19 showed a working file can freeze once. **R18 G's file is the only freeze confirmed twice** (R18 on G, R19 on F).

R18 G against the working G21: decoded content is equal except what G21 still carries and Round 18 cleared, plus `0x10` = 1, no seq-ahead or pad, `0x14` = 0, rbxport's ISRC form, and the UTF-16 lengths counting a NUL. R17 F had all of those fixed and froze once; if that freeze was real, no difference found so far explains it.

## Round 22: the content fixes on the confirmed freeze, and a repeat (`round22.ps1`)

| Stick | |
|---|---|
| **G** | R18 G (froze twice) + header5, pad, seq-ahead, utf16-nul, `0x14` from rekordbox, ISRCs blanked; its fixed ANLZ files. Decoded content equal to the working G21's. |
| **F** | **R17 F again, unchanged** (froze once). |

Each stick is tried twice from a fresh power-on.

- **G works, F works:** R17 F's freeze was not reproducible. The content fixes (one or more of `0x10`, `0x14`, ISRC, NUL) fix R18 G; split them next.
- **G works, F freezes twice:** R17 F has a real cause not yet found; compare it with G22 byte for byte.
- **G freezes:** the content fixes are not enough for rbxport's own file, so the difference is in something not yet decoded.
