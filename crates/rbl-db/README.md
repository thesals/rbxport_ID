# rbl-db

[Crate index](../README.md) · [Architecture](../../docs/development/architecture.md)

Opens rbxport's own library (a SQLCipher `master.db` in rekordbox's layout, kept in rbxport's folder) and, read-only, rekordbox's. Probes the schema and provides library reads, edits, imports, and temporary fixture creation.

## Start here

Follow `LibraryLocation`, `own_location`, `OpenMode`, and `Library::open` in [`src/lib.rs`](src/lib.rs), then the operation-specific module. `rbl-index` loads its browse snapshot from this crate.

Read [`tests/writes.rs`](tests/writes.rs) for existing cases and expected behavior.
The [manifest](Cargo.toml) lists dependencies and feature flags.

## Code map

| File in `src/` | Responsibility |
| --- | --- |
| [`lib.rs`](src/lib.rs) | Location detection, connection opening, and write guards. |
| [`schema.rs`](src/schema.rs) | Schema support probing. |
| [`write.rs`](src/write.rs) | Library edits. |
| [`fixture.rs`](src/fixture.rs) | Temporary test library. |
| [`details.rs`](src/details.rs) | Track detail queries. |
| [`import.rs`](src/import.rs) | Import operations. |
| [`new_library.rs`](src/new_library.rs) | New library creation, in rbxport's folder. |
| [`rekordbox_import.rs`](src/rekordbox_import.rs) | The first import from rekordbox: a hash-checked copy of its `master.db` becomes rbxport's, `share/PIONEER/` is copied, and `import.db` records the mappings and baselines. rekordbox's files are only read. |
| [`xml.rs`](src/xml.rs) | Rekordbox XML. |
| [`itunes.rs`](src/itunes.rs) | Music-library input. |
| [`export_info.rs`](src/export_info.rs) | Export bookkeeping. |

## Contracts and safety

Opening the user's library is real data access. Never use it for write tests: set `RB_LITE_TEST=1` and use `fixture::build` in a temporary directory. Read-write access is explicit. `write_refusal_reason` always refuses rekordbox's library, both when a location is marked `is_rekordbox_install` and when its database is inside a folder rekordbox keeps its library in; under test mode it also refuses rbxport's own (`is_user_library`). Do not bypass those guards or document key material.

## Run focused checks

From the repository root:

```sh
RB_LITE_TEST=1 cargo test -p rbl-db
cargo clippy -p rbl-db --all-targets -- -D warnings
```

Follow the [test guide](../../docs/development/testing.md) for broader checks
and the [contribution guide](../../CONTRIBUTING.md) before preparing a change.

