//! Access to rbxport's own library, a `master.db` in rekordbox's layout kept
//! in rbxport's folder, and read access to rekordbox's for importing it.
//!
//! # Safety around the user's libraries
//!
//! Three rules are enforced here rather than left to callers:
//!
//! - Opening is **read-only** unless [`OpenMode::ReadWrite`] is asked for
//!   explicitly.
//! - **rekordbox's library is never written.** Read-write is refused for
//!   rekordbox's installed library and for any file inside a folder rekordbox
//!   keeps its library in, whatever the location says about itself
//!   ([`write_refusal_reason`]).
//! - With `RBXPORT_TEST=1` set, read-write against the user's own library is
//!   refused outright, so a test can never write to it even by mistake.

pub mod details;
pub mod export_info;
pub mod fixture;
pub mod import;
pub mod itunes;
pub mod key;
pub mod new_library;
pub mod rekordbox_import;
pub mod write;
pub mod xml;
mod schema;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

/// Prevents tests from opening the detected rekordbox library read-write.
pub const TEST_ENV: &str = "RBXPORT_TEST";
/// Accepted so older test scripts retain their installed-library protection.
const LEGACY_TEST_ENV: &str = "RB_LITE_TEST";

/// Whether this process must refuse writes to the detected rekordbox library.
pub fn test_mode() -> bool {
    std::env::var_os(TEST_ENV).is_some() || std::env::var_os(LEGACY_TEST_ENV).is_some()
}
use serde::{Deserialize, Serialize};

pub use schema::{SchemaProbe, SchemaSupport};

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("rekordbox does not appear to be installed: {0}")]
    NotInstalled(String),
    #[error("could not derive the database key: {0}")]
    KeyDerivation(String),
    #[error("could not open the database: {0}")]
    Open(String),
    #[error("refusing to open the library for writing: {0}")]
    WriteRefused(String),
    #[error("{0}")]
    Import(String),
    #[error("unexpected database schema: {0}")]
    Schema(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, DbError>;

/// Where a library's database is and how to unlock it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryLocation {
    pub master_db: PathBuf,
    /// Root of the `share/` tree holding ANLZ files and artwork.
    pub share_root: PathBuf,
    /// Decrypted `SQLCipher` passphrase.
    #[serde(skip)]
    pub passphrase: String,
    /// True for the library the user's real data lives in: rbxport's own.
    /// Test mode refuses to write it.
    #[serde(alias = "is_real_install")]
    pub is_user_library: bool,
    /// True for rekordbox's own installed library, which is never written.
    #[serde(default)]
    pub is_rekordbox_install: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    ReadOnly,
    ReadWrite,
}

/// Names an `options.json` to use instead of the installed agent's.
///
/// What points the compiled app at a fixture library on a test machine —
/// see `scripts/e2e-win/`. When set, [`own_location`] opens the library it
/// names as the user's library. Unset in ordinary use.
pub const OPTIONS_ENV: &str = "RBXPORT_OPTIONS";

/// Names the folder rbxport keeps its own library in, instead of
/// [`own_library_dir`]'s default.
pub const LIBRARY_DIR_ENV: &str = "RBXPORT_LIBRARY_DIR";

/// The agent's options file, which holds the db path and the wrapped passphrase.
fn options_path() -> Result<PathBuf> {
    let path = options_location()?;
    if path.is_file() {
        return Ok(path);
    }
    Err(DbError::NotInstalled(if std::env::var_os(OPTIONS_ENV).is_some() {
        format!("{OPTIONS_ENV} names {}, which is not a file", path.display())
    } else {
        format!("{} not found", path.display())
    }))
}

/// Where the agent's options file is, or goes when there is none yet.
pub(crate) fn options_location() -> Result<PathBuf> {
    if let Some(chosen) = std::env::var_os(OPTIONS_ENV) {
        return Ok(PathBuf::from(chosen));
    }
    agent_options_location()
}

/// The installed agent's options file, whatever [`OPTIONS_ENV`] says.
fn agent_options_location() -> Result<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        dirs::config_dir().map(|p| p.join("Pioneer"))
    } else {
        dirs::home_dir().map(|p| p.join("Library/Application Support/Pioneer"))
    }
    .ok_or_else(|| DbError::NotInstalled("no home directory".into()))?;
    Ok(base.join("rekordboxAgent/storage/options.json"))
}

/// The folder rekordbox keeps `master.db` and `share/` in when nothing says
/// otherwise: `~/Library/Pioneer/rekordbox` on macOS,
/// `%APPDATA%\Pioneer\rekordbox` on Windows [OBS 7.2.11, 7.2.14].
pub(crate) fn default_library_dir() -> Result<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        dirs::config_dir()
    } else {
        dirs::home_dir().map(|p| p.join("Library"))
    };
    base.map(|p| p.join("Pioneer/rekordbox"))
        .ok_or_else(|| DbError::NotInstalled("no home directory".into()))
}

/// The folder rbxport keeps its own library in: `master.db` and `share/`.
///
/// `%APPDATA%\rbxport\library` on Windows,
/// `~/Library/Application Support/rbxport/library` on macOS, or what
/// [`LIBRARY_DIR_ENV`] names.
pub fn own_library_dir() -> Result<PathBuf> {
    if let Some(chosen) = std::env::var_os(LIBRARY_DIR_ENV) {
        return Ok(PathBuf::from(chosen));
    }
    dirs::data_dir()
        .map(|p| p.join("rbxport").join("library"))
        .ok_or_else(|| DbError::NotInstalled("no data directory".into()))
}

/// rbxport's own library, which the application opens at startup.
///
/// The database need not exist yet: opening it then fails, and
/// [`new_library::plan`] offers to make it. With [`OPTIONS_ENV`] set, the
/// library that options file names is used instead, as a test machine's
/// fixture.
pub fn own_location() -> Result<LibraryLocation> {
    if std::env::var_os(OPTIONS_ENV).is_some() {
        let mut found = detect_from(&options_path()?)?;
        found.is_user_library = true;
        found.is_rekordbox_install = false;
        return Ok(found);
    }
    own_location_in(&own_library_dir()?)
}

/// rbxport's library kept in `dir`.
pub fn own_location_in(dir: &Path) -> Result<LibraryLocation> {
    Ok(LibraryLocation {
        master_db: dir.join("master.db"),
        share_root: dir.join("share"),
        passphrase: new_library::passphrase()?,
        is_user_library: true,
        is_rekordbox_install: false,
    })
}

/// Finds rekordbox's installed library and unwraps its passphrase, for
/// reading it: importing it, or studying rekordbox's behaviour. It is never
/// opened for writing.
pub fn detect() -> Result<LibraryLocation> {
    detect_from(&options_path()?)
}

pub fn detect_from(options_json: &Path) -> Result<LibraryLocation> {
    let text = std::fs::read_to_string(options_json)?;
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| DbError::NotInstalled(format!("options.json is not valid JSON: {e}")))?;

    // `options` is an array of [key, value] pairs.
    let entries = parsed
        .get("options")
        .and_then(|v| v.as_array())
        .ok_or_else(|| DbError::NotInstalled("options.json has no `options` array".into()))?;

    let mut db_path: Option<String> = None;
    let mut dp: Option<String> = None;
    for entry in entries {
        let Some(pair) = entry.as_array() else { continue };
        let (Some(k), Some(v)) = (pair.first().and_then(|k| k.as_str()), pair.get(1)) else {
            continue;
        };
        match k {
            "db-path" => db_path = v.as_str().map(str::to_owned),
            "dp" => dp = v.as_str().map(str::to_owned),
            _ => {}
        }
    }

    let master_db = PathBuf::from(
        db_path.ok_or_else(|| DbError::NotInstalled("options.json has no db-path".into()))?,
    );
    let passphrase = key::derive_password(
        &dp.ok_or_else(|| DbError::NotInstalled("options.json has no dp".into()))?,
    )?;

    let share_root = master_db
        .parent()
        .map_or_else(|| PathBuf::from("share"), |p| p.join("share"));

    Ok(LibraryLocation { master_db, share_root, passphrase, is_user_library: false, is_rekordbox_install: true })
}

/// The folders rekordbox keeps its library in on this machine: its default
/// folder, and the folder of the database its agent's `options.json` names.
///
/// Read from the installed agent, never from [`OPTIONS_ENV`], so a fixture
/// named that way is not mistaken for rekordbox's library.
#[must_use]
pub fn rekordbox_library_dirs() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = default_library_dir().into_iter().collect();
    let named = agent_options_location()
        .ok()
        .and_then(|options| detect_from(&options).ok())
        .and_then(|location| location.master_db.parent().map(Path::to_path_buf));
    found.extend(named);
    found
}

/// Whether `path` is one of `roots` or inside one: compared after resolving
/// links and `..`, and without regard to case on Windows and macOS, whose
/// file systems ignore it by default. Parts of `path` that do not exist yet
/// are compared as written.
#[must_use]
pub fn is_within_any(path: &Path, roots: &[PathBuf]) -> bool {
    let target = comparable(path);
    roots.iter().any(|root| target.starts_with(comparable(root)))
}

fn comparable(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    };
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    let resolved = loop {
        if let Ok(found) = existing.canonicalize() {
            break rest.iter().rev().fold(found, |mut at, part| {
                at.push(part);
                at
            });
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => break absolute.clone(),
        }
    };
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        PathBuf::from(resolved.to_string_lossy().to_lowercase())
    } else {
        resolved
    }
}

/// True when rekordbox (or its agent) is running.
///
/// rbxport's own library never waits on this. It matters only where rekordbox
/// really holds something: the LINK ports, and a USB stick it may write.
pub fn is_rekordbox_running() -> bool {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System};
    let sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new()),
    );
    sys.processes().values().any(|p| {
        let name = p.name().to_string_lossy().to_ascii_lowercase();
        // Match the app and its agent, not Electron helpers.
        name == "rekordbox" || name == "rekordbox.exe"
            || name == "rekordboxagent" || name == "rekordboxagent.exe"
    })
}

/// What rbxport says when asked to change rekordbox's library.
pub const REKORDBOX_REFUSAL: &str =
    "This is rekordbox's library. rbxport never changes it; import it into rbxport's library instead.";

/// True when the library at `location` is open for reading only: rekordbox's,
/// or the user's own under test mode. Nothing else makes it read-only.
#[must_use]
pub fn is_read_only(location: &LibraryLocation) -> bool {
    write_refusal_reason(location, test_mode()).is_some()
}

/// Why writing the library at `location` must be refused, if it must.
///
/// rekordbox's library is refused by what the location says and, so that a
/// mislabelled location cannot slip through, by where its database is.
#[must_use]
pub fn write_refusal_reason(location: &LibraryLocation, test_mode: bool) -> Option<&'static str> {
    let rekordbox = location.is_rekordbox_install
        || is_within_any(&location.master_db, &rekordbox_library_dirs());
    refusal(rekordbox, location.is_user_library, test_mode)
}

/// [`write_refusal_reason`]'s rule apart from the file system, so it can be
/// tested without mutating process-global state.
fn refusal(rekordbox: bool, user_library: bool, test_mode: bool) -> Option<&'static str> {
    if rekordbox {
        return Some(REKORDBOX_REFUSAL);
    }
    // A fixture in a temp directory is neither: writing it is what the write
    // tests do.
    if user_library && test_mode {
        return Some("RBXPORT_TEST is set and this is the user's library; tests must use a fixture");
    }
    None
}

/// An open handle to the library.
#[derive(Debug)]
pub struct Library {
    conn: Connection,
    mode: OpenMode,
    location: LibraryLocation,
    schema: SchemaProbe,
}

impl Library {
    /// Opens the library. See the module docs for the write rules.
    pub fn open(location: LibraryLocation, mode: OpenMode) -> Result<Self> {
        if mode == OpenMode::ReadWrite {
            if let Some(reason) = write_refusal_reason(&location, test_mode()) {
                return Err(DbError::WriteRefused(reason.into()));
            }
        }

        let flags = match mode {
            OpenMode::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            OpenMode::ReadWrite => OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        };

        let conn = Connection::open_with_flags(&location.master_db, flags)
            .map_err(|e| DbError::Open(format!("{}: {e}", location.master_db.display())))?;

        // Order matters: cipher settings must precede the key.
        conn.pragma_update(None, "cipher", "sqlcipher")?;
        conn.pragma_update(None, "legacy", 4)?;
        conn.pragma_update(None, "key", &location.passphrase)?;
        // Readers use committed snapshots. Writers explicitly require durable
        // journaling rather than depending on a connection's defaults.
        if mode == OpenMode::ReadWrite {
            configure_durability(&conn)?;
        }

        // The first read is what actually proves the key: a wrong passphrase
        // fails here rather than at open time.
        // Opening a hot rollback journal read-only cannot replay it. Recover
        // through the normal guarded writable opener, then return a fresh RO
        // handle. Never bypass the rekordbox/test write gates for recovery.
        if mode == OpenMode::ReadOnly {
            if let Err(rusqlite::Error::SqliteFailure(error, _)) = conn.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0)) {
                if matches!(error.extended_code, 264 | 776) {
                    drop(conn);
                    drop(Self::open(location.clone(), OpenMode::ReadWrite)?);
                    return Self::open(location, mode);
                }
            }
        }
        let schema = SchemaProbe::probe(&conn)?;

        Ok(Self { conn, mode, location, schema })
    }

    /// Convenience: detect and open rekordbox's installed library read-only,
    /// for the development tools that study it.
    pub fn open_installed_read_only() -> Result<Self> {
        Self::open(detect()?, OpenMode::ReadOnly)
    }

    pub fn schema(&self) -> &SchemaProbe {
        &self.schema
    }

    pub fn location(&self) -> &LibraryLocation {
        &self.location
    }

    pub fn mode(&self) -> OpenMode {
        self.mode
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Mutable access, for the transaction the writer runs each action in.
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Live (not soft-deleted) track count.
    pub fn live_track_count(&self) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM djmdContent WHERE rb_local_deleted = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Live playlist count.
    pub fn live_playlist_count(&self) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM djmdPlaylist WHERE rb_local_deleted = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }
}

/// Where a track whose `FolderPath` starts with `/contents_<id>/` really
/// is: rekordbox's Cloud Library Sync keeps those under
/// `<DropboxSharingPath>/rekordbox/`, and `DropboxSharingPath` is a value
/// in `rekordbox3.settings` [OBS 7.2.11]. `None` when there is no such
/// setting, in which case such a track has no file this machine can see.
#[must_use]
pub fn cloud_contents_root() -> Option<PathBuf> {
    let settings = std::fs::read_to_string(rbl_core::paths::rekordbox_settings_dir()?.join("rekordbox3.settings")).ok()?;
    let marker = "<VALUE name=\"DropboxSharingPath\" val=\"";
    let start = settings.find(marker)? + marker.len();
    let end = settings[start..].find('"')? + start;
    let value = settings[start..end].replace("&amp;", "&");
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value).join("rekordbox"))
}

/// A library `FolderPath` as a path on this machine: a cloud-library path
/// (`/contents_<id>/…`) placed under the cloud root when one is known,
/// anything else as it is.
#[must_use]
pub fn resolve_folder_path(folder_path: &str, cloud_root: Option<&Path>) -> String {
    match (folder_path.strip_prefix("/contents_"), cloud_root) {
        (Some(_), Some(root)) => root.join(folder_path.trim_start_matches('/')).to_string_lossy().into_owned(),
        _ => folder_path.to_owned(),
    }
}

/// Require crash-safe journals and durable commits on every writable handle.
pub fn configure_durability(conn: &Connection) -> Result<()> {
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    if !matches!(mode.to_ascii_lowercase().as_str(), "delete" | "truncate" | "persist" | "wal") {
        return Err(DbError::WriteRefused(format!("unsafe database journal mode: {mode}")));
    }
    conn.pragma_update(None, "synchronous", "EXTRA")?;
    conn.pragma_update(None, "fullfsync", true)?;
    conn.pragma_update(None, "checkpoint_fullfsync", true)?;
    conn.pragma_update(None, "read_uncommitted", false)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn detect_from_reports_a_missing_options_file_clearly() {
        let err = detect_from(Path::new("/nonexistent/options.json")).unwrap_err();
        assert!(matches!(err, DbError::Io(_)));
    }

    #[test]
    fn detect_from_rejects_options_without_a_db_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("options.json");
        std::fs::write(&path, r#"{"options":[["something","else"]]}"#).unwrap();
        assert!(matches!(detect_from(&path), Err(DbError::NotInstalled(_))));
    }

    #[test]
    fn rekordbox_s_library_is_always_refused_and_the_user_s_only_in_test_mode() {
        // Against the pure rule, so the test needs no global env mutation
        // (which is `unsafe` in edition 2024 and racy across threads).
        assert_eq!(refusal(true, false, false), Some(REKORDBOX_REFUSAL));
        assert_eq!(refusal(true, true, false), Some(REKORDBOX_REFUSAL));
        assert_eq!(refusal(true, false, true), Some(REKORDBOX_REFUSAL));
        // The user's own library: writable, except by a test.
        assert!(refusal(false, true, false).is_none());
        assert!(refusal(false, true, true).is_some());
        // A fixture is neither.
        assert!(refusal(false, false, false).is_none());
        assert!(refusal(false, false, true).is_none());
    }

    fn location_at(master_db: PathBuf, rekordbox: bool) -> LibraryLocation {
        LibraryLocation {
            master_db,
            share_root: PathBuf::new(),
            passphrase: String::new(),
            is_user_library: false,
            is_rekordbox_install: rekordbox,
        }
    }

    #[test]
    fn a_location_marked_as_rekordbox_s_is_refused_wherever_it_is() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(write_refusal_reason(&location_at(dir.path().join("master.db"), true), false), Some(REKORDBOX_REFUSAL));
        assert!(write_refusal_reason(&location_at(dir.path().join("master.db"), false), false).is_none());
    }

    #[test]
    fn a_file_inside_rekordbox_s_folder_counts_as_rekordbox_s() {
        let root = tempfile::tempdir().unwrap();
        let rekordbox = root.path().join("Pioneer").join("rekordbox");
        std::fs::create_dir_all(rekordbox.join("share")).unwrap();
        std::fs::create_dir_all(root.path().join("rbxport")).unwrap();
        let roots = [rekordbox.clone()];
        assert!(is_within_any(&rekordbox.join("master.db"), &roots));
        assert!(is_within_any(&rekordbox.join("share").join("x.db"), &roots), "nested, existing folder");
        assert!(is_within_any(&rekordbox.join("not-yet").join("master.db"), &roots), "nested, missing folder");
        assert!(is_within_any(&rekordbox, &roots), "the folder itself");
        let dotted = root.path().join("rbxport").join("..").join("Pioneer").join("rekordbox").join("master.db");
        assert!(is_within_any(&dotted, &roots), "through ..");
        assert!(!is_within_any(&root.path().join("rbxport").join("library").join("master.db"), &roots));
        assert!(
            !is_within_any(&root.path().join("Pioneer").join("rekordbox-other").join("master.db"), &roots),
            "a sibling whose name starts the same is not inside"
        );
        if cfg!(windows) {
            let shouted = PathBuf::from(rekordbox.to_string_lossy().to_uppercase()).join("MASTER.DB");
            assert!(is_within_any(&shouted, &roots), "case does not matter on Windows");
        }
    }

    #[test]
    fn the_own_library_is_kept_in_the_folder_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let location = own_location_in(dir.path()).unwrap();
        assert_eq!(location.master_db, dir.path().join("master.db"));
        assert_eq!(location.share_root, dir.path().join("share"));
        assert!(location.is_user_library);
        assert!(!location.is_rekordbox_install);
        assert_eq!(location.passphrase.len(), 64);
    }

    #[test]
    fn the_default_own_library_folder_is_rbxport_s_not_rekordbox_s() {
        if std::env::var_os(LIBRARY_DIR_ENV).is_some() {
            return;
        }
        let dir = own_library_dir().unwrap();
        assert!(dir.ends_with(Path::new("rbxport").join("library")));
        assert!(!is_within_any(&dir, &rekordbox_library_dirs()));
    }

    #[test]
    fn a_detected_install_is_rekordbox_s_and_refused() {
        let dir = tempfile::tempdir().unwrap();
        let options = dir.path().join("options.json");
        fixture::write_options_json(&options, &dir.path().join("master.db").to_string_lossy(), "k").unwrap();
        let found = detect_from(&options).unwrap();
        assert!(found.is_rekordbox_install);
        assert!(!found.is_user_library);
        assert_eq!(write_refusal_reason(&found, false), Some(REKORDBOX_REFUSAL));
    }

    #[test]
    fn a_location_saved_before_the_split_still_reads() {
        let old = r#"{"master_db":"m","share_root":"s","is_real_install":true}"#;
        let location: LibraryLocation = serde_json::from_str(old).unwrap();
        assert!(location.is_user_library);
        assert!(!location.is_rekordbox_install);
    }
}
