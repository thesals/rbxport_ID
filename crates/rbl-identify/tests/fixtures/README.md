# rbl-identify test fixtures

Response bodies the integration tests replay through a fake transport. No
test makes a live call. No file here holds a key, a token or a real
User-Agent contact address.

## AcoustID

| File | Origin |
| --- | --- |
| `acoustid_lookup_documented.json` | **Hand-written.** The example response from <https://acoustid.org/webservice> (`meta=recordings+releasegroups+compress`), extended with a second recording that has a `joinphrase` and no release groups, a release group with `secondarytypes`, and a match with no `recordings` at all. IDs starting `00000000-` are made up. Replace with a recorded lookup once fingerprinting exists. |
| `acoustid_lookup_error.json` | **Hand-written.** AcoustID's error object (`status: error`, `error.code`, `error.message`); the documentation does not show one, this is the shape the service returns for a bad key. |
