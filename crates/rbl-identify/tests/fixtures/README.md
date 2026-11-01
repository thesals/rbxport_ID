# rbl-identify test fixtures

Response bodies the integration tests replay through a fake transport. No
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
