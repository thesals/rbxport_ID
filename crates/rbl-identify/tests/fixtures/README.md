# rbl-identify test fixtures

Response bodies (and one image) the integration tests replay through a fake transport. No
test makes a live call. No file here holds a key, a token or a real
User-Agent contact address.

## AcoustID

| File | Origin |
| --- | --- |
| `acoustid_lookup_documented.json` | **Hand-written.** The example response from <https://acoustid.org/webservice> (`meta=recordings+releasegroups+compress`), extended with a second recording that has a `joinphrase` and no release groups, a release group with `secondarytypes`, and a match with no `recordings` at all. IDs starting `00000000-` are made up. Hand-written; the pipeline's live verification against AcoustID is separate. |
| `acoustid_lookup_error.json` | **Hand-written.** AcoustID's error object (`status: error`, `error.code`, `error.message`); the documentation does not show one, this is the shape the service returns for a bad key. |

## MusicBrainz

**Recorded** from `https://musicbrainz.org/ws/2/` on 2026-11-01 by
`tests/record_musicbrainz.rs` (ignored; run with `RBL_IDENTIFY_RECORD=1`),
through `MusicBrainzClient` and its 1 req/s limiter, with the User-Agent
`rbxport-identify-tests/0.1 ( https://github.com/ )`. Bodies are saved
verbatim. `musicbrainz_index.json` maps each request's path and query to
its file; the replay tests route by it, so a changed URL fails loudly.

| File | Request |
| --- | --- |
| `musicbrainz_search_deepack_the_music.json` | Search `artist:"Deepack" AND recording:"The Music"`. **Empty**: neither version of the track is in MusicBrainz. Kept for the no-hit path. |
| `musicbrainz_search_boral_kibil_end_of_life.json` | Search `artist:"Boral Kibil" AND recording:"End of Life"`. **Empty**, likewise. |
| `musicbrainz_search_eg_spirit_aerofoil_remix.json` | Search `artist:"E\&G" AND recording:"Spirit \(Aerofoil Remix\)"`: one hit, the remix. |
| `musicbrainz_recording_a6087c9d-….json` | Lookup of E&G vs. Andromedha "Spirit (Aerofoil remix)", with a `remixer` relationship to Aerofoil. |
| `musicbrainz_releases_a6087c9d-….json` | Its release browse: Fraction Records, FRA224. |
| `musicbrainz_recording_91bd1ecd-….json` | Lookup of Deepack "Music Is the Key (extended mix)", standing in for the missing "The Music" as a Deepack extended mix with an ISRC. |
| `musicbrainz_releases_91bd1ecd-….json` | Its release browse: Dirty Workz, DWX1417. |
| `musicbrainz_recording_41f4b491-….json` | Lookup of Mahmut Orhan & Boral Kibil "Fringe", an appearance on a DJ-mix (release group Album + Compilation, DJ-mix), standing in for the missing "End of Life". |
| `musicbrainz_releases_41f4b491-….json` | Its release browse: George V Records, 3263132. |

The searches and release browses were recorded first; the three recording
lookups were re-recorded the same day with `artist-rels` added to `inc`
(`RBL_IDENTIFY_RECORD_RECORDINGS=...`). The two stand-in MBIDs were found
with four manual searches by artist ID. None of the recorded recordings has
genres or tags, so genre ordering is tested on hand-written bodies.

## Discogs

**Hand-written**, in the shape of Discogs' API responses
(<https://www.discogs.com/developers>). Every artist, label, title and ID is
made up (IDs `9000001` and up). They could not be recorded: on 2026-11-01
the one live request `tests/record_discogs.rs` made (the Recondite "Nadsat"
search) was answered 401, because the `DISCOGS_TOKEN` in `.env.identify` is
20 characters, the length of a consumer key, not of a personal access token.

| File | Content |
| --- | --- |
| `discogs_search_example.json` | A search page: a full release hit (string year, two labels), one with `null` labels and styles, an empty year and catno `none`, a `master` the client skips, and a compilation with a numeric year and `Not On Label`. |
| `discogs_search_empty.json` | A search with no results, as for Deepack "The Music". |
| `discogs_release_example.json` | A release: an artist with a ` (2)` suffix, a `Not On Label` / `none` label credit, headings, an index track with sub-tracks, a remixer and an engineer in `extraartists`, a track-level artist, `null` duration and lists, and a primary and a secondary image on `i.discogs.com`. |

The Discogs image test serves the Cover Art Archive JPEG below under an
`i.discogs.com` path; only its bytes matter.

With a working token, the recorder (`RBL_IDENTIFY_RECORD=1`; the token comes
from `.env.identify` at the workspace root, or the file `RBL_IDENTIFY_ENV`
names) records the searches for Recondite "Nadsat", Floe "Hysteresis", Alex
Guesta "Voodoo" and Deepack "The Music"; `RBL_IDENTIFY_RECORD_RELEASES=id,id`
records release lookups instead and `RBL_IDENTIFY_RECORD_IMAGE=uri` one image
(saved only under 100 KB), at most 15 Discogs requests per run. Before a
body is saved it is checked not to contain the token, and every
`community`, `contributors`, `submitter` and `user_data` object is removed:
they hold other Discogs users' usernames and the token owner's collection
and wantlist flags. `discogs_index.json` then maps each request's path and
query to its file.

## Cover Art Archive

**Recorded** from `https://coverartarchive.org/` on 2026-11-01 by
`tests/record_discogs.rs` (`record_coverart_fixtures`), through
`CoverArtClient::live` (so its redirect-limited transport and the 1 req/s
limiter) with the test User-Agent. Four requests:

| Request | Result |
| --- | --- |
| `/release/0c59bcac-…/front-500` (E&G vs. Andromedha "Spirit") | 404: no artwork, as its MusicBrainz release says (`cover-art-archive.count` 0). Not saved; the fake transport's 404 stands in. |
| `/release-group/a0fb1254-…/front-500` (its group) | 404, likewise. |
| `/release/c3132648-…/front-500` (Deepack "Music Is the Key") | 200 after the archive.org redirect: a JPEG. The recorded image was real album artwork, so the committed `coverart_release_c3132648-…_front-500.jpg` is a 2 048-byte synthetic stand-in (JPEG markers and a comment, no picture): the tests only need its first bytes and its size. |
| `/release-group/fdde1ef4-…/front-500` (its group) | The same image, byte for byte, so `coverart_index.json` routes both paths to the one file. |
