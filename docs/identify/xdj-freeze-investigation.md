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

**Report back:**
- **Did it freeze?** Yes or no.
- **If yes, exactly when:** as the stick is inserted, on pressing the USB source button, at the My Settings prompt, on opening a playlist, or on loading a track. What the screen shows, and whether pulling the stick recovers it or it needs a power cycle.
- **If no:** open the playlist and load two tracks, including `ElezD - Quatro (Original Mix)`, the 32-bit file, and say what happens.

**What each answer means:**
- **Freezes:** it's the stick or its layout, not rbxport. Next round: G re-laid out with an MBR partition, then rbxport on F.
- **Works:** it's rbxport's export. Next round: hybrid sticks built from these two exports (rekordbox's `export.pdb` with our ANLZ files and the reverse, then the extra files added or removed), each halving what is left.
