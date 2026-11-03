//! Identification's settings: the service keys, the genre policy and
//! auto-apply.
//!
//! Secrets (the AcoustID key, the Discogs token) and the MusicBrainz user
//! agent live in `.env.identify` in the identify data folder; the rest in
//! `settings.json` beside it. Each key is resolved on its own: an
//! environment variable wins over the data-folder file, which wins over a
//! development checkout's file (debug builds only). The settings DTO says
//! where each key comes from, NEVER its value; no secret is logged or put
//! in an error.
//!
//! Both files are rewritten atomically (staged in the same folder, renamed
//! over), and `.env.identify` keeps every line it does not change.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rbl_identify::config::{file_value, Config};
use rbl_identify::decide::AUTO_APPLY_THRESHOLD;
use rbl_identify::pipeline::{self, GenrePolicy};
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{AppError, AppResult, ErrorKind};

pub(crate) const ACOUSTID_KEY: &str = "ACOUSTID_API_KEY";
pub(crate) const DISCOGS_TOKEN: &str = "DISCOGS_TOKEN";
pub(crate) const MB_USER_AGENT: &str = "MB_USER_AGENT";
/// The non-secret settings, beside `.env.identify`.
pub(crate) const SETTINGS_FILE: &str = "settings.json";

/// Reads one environment variable; injectable so tests never see the real
/// environment (which may hold a real key).
pub(crate) type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Where a key comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KeySource {
    /// The data-folder `.env.identify`, which the settings write.
    Settings,
    /// A development checkout's `.env.identify` (debug builds only).
    DevFile,
    /// An environment variable, which wins over both files.
    Environment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GenrePolicyDto {
    #[default]
    FillEmpty,
    Replace,
    Never,
}

impl From<GenrePolicyDto> for GenrePolicy {
    fn from(policy: GenrePolicyDto) -> Self {
        match policy {
            GenrePolicyDto::FillEmpty => Self::FillEmpty,
            GenrePolicyDto::Replace => Self::Replace,
            GenrePolicyDto::Never => Self::Never,
        }
    }
}

/// What `identify_settings` answers. Never holds a secret.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentifySettingsDto {
    pub acoustid_key: Option<KeySource>,
    pub discogs_token: Option<KeySource>,
    /// Not a secret: shown as it is; "" when unset.
    pub mb_user_agent: String,
    pub mb_user_agent_source: Option<KeySource>,
    pub genre_policy: GenrePolicyDto,
    pub auto_apply: bool,
    pub auto_apply_threshold: f64,
    /// The data-folder `.env.identify`, for display.
    pub settings_file: String,
}

/// What an update does to a secret: absent keeps it, `null` clears it, a
/// string replaces it (trimmed; an empty one clears it).
#[derive(Clone, Default, PartialEq, Eq)]
pub enum SecretUpdate {
    #[default]
    Keep,
    Clear,
    Set(String),
}

impl SecretUpdate {
    /// Adds this change of `key` to `changes` (`None` to remove the line):
    /// nothing to keep it, a trimmed value to set it, an empty one clears.
    fn push_to<'a>(&self, key: &'a str, changes: &mut Vec<(&'a str, Option<String>)>) {
        match self {
            Self::Keep => {}
            Self::Clear => changes.push((key, None)),
            Self::Set(value) => {
                let value = value.trim();
                changes.push((key, (!value.is_empty()).then(|| value.to_owned())));
            }
        }
    }
}

impl std::fmt::Debug for SecretUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Keep => "Keep",
            Self::Clear => "Clear",
            Self::Set(_) => "Set(<redacted>)",
        })
    }
}

/// A member that is present: `null` clears, a string sets. An absent one
/// takes the default, keep.
fn secret_update<'de, D: Deserializer<'de>>(d: D) -> Result<SecretUpdate, D::Error> {
    Ok(Option::<String>::deserialize(d)?.map_or(SecretUpdate::Clear, SecretUpdate::Set))
}

/// `identify_save_settings`'s change. Every member is optional: absent
/// keeps the setting. For the two secrets, `null` (or an empty string)
/// clears the data-folder value and a string replaces it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IdentifySettingsUpdateDto {
    #[serde(default, deserialize_with = "secret_update")]
    pub acoustid_key: SecretUpdate,
    #[serde(default, deserialize_with = "secret_update")]
    pub discogs_token: SecretUpdate,
    #[serde(default)]
    pub mb_user_agent: Option<String>,
    #[serde(default)]
    pub genre_policy: Option<GenrePolicyDto>,
    #[serde(default)]
    pub auto_apply: Option<bool>,
    #[serde(default)]
    pub auto_apply_threshold: Option<f64>,
}

/// The non-secret settings, as `settings.json` holds them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct Stored {
    pub genre_policy: GenrePolicyDto,
    pub auto_apply: bool,
    pub auto_apply_threshold: f64,
}

impl Default for Stored {
    fn default() -> Self {
        Self { genre_policy: GenrePolicyDto::FillEmpty, auto_apply: false, auto_apply_threshold: AUTO_APPLY_THRESHOLD }
    }
}

impl Stored {
    /// What the job's pipeline runs with.
    pub(crate) fn pipeline(self) -> pipeline::Settings {
        pipeline::Settings {
            auto_apply: self.auto_apply,
            genre: self.genre_policy.into(),
            auto_apply_threshold: self.auto_apply_threshold,
        }
    }
}

fn valid_threshold(t: f64) -> bool {
    (AUTO_APPLY_THRESHOLD..=1.0).contains(&t)
}

/// Where the keys are looked for.
#[derive(Clone)]
pub(crate) struct Sources {
    /// The identify data folder: `.env.identify` and `settings.json`.
    pub root: PathBuf,
    /// Development checkouts' `.env.identify`, in order (none in release).
    pub dev_files: Vec<PathBuf>,
    pub env: EnvLookup,
}

impl Sources {
    pub(crate) fn env_file(&self) -> PathBuf {
        self.root.join(super::ENV_FILE)
    }

    fn settings_json(&self) -> PathBuf {
        self.root.join(SETTINGS_FILE)
    }

    /// One key's value and where it came from: the environment, then the
    /// data-folder file, then the development files. Empty values do not
    /// count.
    pub(crate) fn resolve(&self, key: &str) -> Option<(String, KeySource)> {
        if let Some(value) = (self.env)(key).filter(|v| !v.trim().is_empty()) {
            return Some((value, KeySource::Environment));
        }
        if let Some(value) = read_env_text(&self.env_file()).and_then(|text| file_value(&text, key)) {
            return Some((value, KeySource::Settings));
        }
        self.dev_files
            .iter()
            .find_map(|path| read_env_text(path).and_then(|text| file_value(&text, key)))
            .map(|value| (value, KeySource::DevFile))
    }

    /// The services' config. The error names what is missing, never a value.
    pub(crate) fn config(&self) -> AppResult<Config> {
        Config::from_lookup(|key| self.resolve(key).map(|(value, _)| value)).map_err(|e| {
            AppError::new(
                ErrorKind::NotFound,
                format!(
                    "Identification needs an AcoustID API key and a MusicBrainz user agent. \
                     Add them in the identification settings, or set {ACOUSTID_KEY} and {MB_USER_AGENT} in {}.",
                    self.env_file().display()
                ),
            )
            .with_detail(e.to_string())
        })
    }

    /// The non-secret settings; the defaults when the file is absent or
    /// unreadable, a threshold out of range brought back into it.
    pub(crate) fn stored(&self) -> Stored {
        self.read_stored().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "identify: settings.json is unreadable; using the defaults");
            Stored::default()
        })
    }

    /// `settings.json` as saved (the defaults when there is none), a
    /// threshold out of range brought back into it; an error when the file
    /// is there but cannot be read or parsed.
    fn read_stored(&self) -> Result<Stored, String> {
        let mut stored = match std::fs::read(self.settings_json()) {
            Ok(bytes) => serde_json::from_slice::<Stored>(&bytes).map_err(|e| e.to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Stored::default(),
            Err(e) => return Err(e.to_string()),
        };
        if !stored.auto_apply_threshold.is_finite() {
            stored.auto_apply_threshold = AUTO_APPLY_THRESHOLD;
        }
        stored.auto_apply_threshold = stored.auto_apply_threshold.clamp(AUTO_APPLY_THRESHOLD, 1.0);
        Ok(stored)
    }

    pub(crate) fn dto(&self) -> IdentifySettingsDto {
        let stored = self.stored();
        let user_agent = self.resolve(MB_USER_AGENT);
        IdentifySettingsDto {
            acoustid_key: self.resolve(ACOUSTID_KEY).map(|(_, source)| source),
            discogs_token: self.resolve(DISCOGS_TOKEN).map(|(_, source)| source),
            mb_user_agent_source: user_agent.as_ref().map(|(_, source)| *source),
            mb_user_agent: user_agent.map(|(value, _)| value).unwrap_or_default(),
            genre_policy: stored.genre_policy,
            auto_apply: stored.auto_apply,
            auto_apply_threshold: stored.auto_apply_threshold,
            settings_file: self.env_file().display().to_string(),
        }
    }

    /// Validates the whole update, then writes what it changes. Nothing is
    /// written when any part is refused. `before_write` runs once, just
    /// before the first file is written, so a caller holding state built
    /// from the old settings can drop it even if a later write fails.
    ///
    /// `settings.json` is rewritten only when a non-secret setting changes,
    /// and then refused (never reset to the defaults) when the file there
    /// cannot be read.
    pub(crate) fn save(&self, update: &IdentifySettingsUpdateDto, before_write: impl FnOnce()) -> AppResult<()> {
        if let Some(t) = update.auto_apply_threshold {
            if !valid_threshold(t) {
                return Err(AppError::new(
                    ErrorKind::Malformed,
                    "The auto-apply threshold must be between 0.92 and 1.",
                ));
            }
        }
        let mut changes: Vec<(&str, Option<String>)> = Vec::new();
        update.acoustid_key.push_to(ACOUSTID_KEY, &mut changes);
        update.discogs_token.push_to(DISCOGS_TOKEN, &mut changes);
        if let Some(agent) = &update.mb_user_agent {
            let agent = agent.trim();
            changes.push((MB_USER_AGENT, (!agent.is_empty()).then(|| agent.to_owned())));
        }
        let non_secret = update.genre_policy.is_some() || update.auto_apply.is_some() || update.auto_apply_threshold.is_some();
        let json = if non_secret {
            let mut stored = self.read_stored().map_err(|e| {
                AppError::new(
                    ErrorKind::Malformed,
                    format!(
                        "The identification settings file {} cannot be read, so it was not changed. Fix or delete it, \
                         then save again.",
                        self.settings_json().display()
                    ),
                )
                .with_detail(e)
            })?;
            if let Some(policy) = update.genre_policy {
                stored.genre_policy = policy;
            }
            if let Some(on) = update.auto_apply {
                stored.auto_apply = on;
            }
            if let Some(t) = update.auto_apply_threshold {
                stored.auto_apply_threshold = t;
            }
            Some(serde_json::to_vec_pretty(&stored).map_err(|e| AppError::internal(format!("identify settings: {e}")))?)
        } else {
            None
        };

        // Everything is checked (and the new file built) before anything is
        // written.
        let env_text = if changes.is_empty() {
            None
        } else {
            let path = self.env_file();
            let text = match std::fs::read(&path) {
                Ok(bytes) => String::from_utf8(bytes).map_err(|_| {
                    AppError::new(ErrorKind::Malformed, "The identification settings file is not UTF-8 text.")
                        .with_detail(path.display().to_string())
                })?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => return Err(write_failed(&path, &e)),
            };
            let borrowed: Vec<(&str, Option<&str>)> = changes.iter().map(|(k, v)| (*k, v.as_deref())).collect();
            Some(rewrite_env(&text, &borrowed)?)
        };
        if json.is_none() && env_text.is_none() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.root).map_err(|e| write_failed(&self.root, &e))?;
        before_write();
        if let Some(json) = json {
            write_atomic(&self.settings_json(), &json)?;
        }
        if let Some(text) = env_text {
            write_atomic(&self.env_file(), text.as_bytes())?;
        }
        Ok(())
    }
}

/// A `.env.identify`'s text, without a BOM; `None` when it cannot be read.
fn read_env_text(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(text))
}

/// Whether a line sets `key` (as [`file_value`] reads it).
fn sets(line: &str, key: &str) -> bool {
    let line = line.trim();
    !line.starts_with('#') && line.split_once('=').is_some_and(|(name, _)| name.trim() == key)
}

/// A value as a `.env.identify` line holds it, quoted when it would not
/// read back as itself unquoted. The error names the key, never the value.
fn encode(key: &str, value: &str) -> AppResult<String> {
    if value.chars().any(char::is_control) {
        return Err(AppError::new(
            ErrorKind::Malformed,
            format!("The value for {key} cannot be saved: it holds a line break or a control character."),
        ));
    }
    let plain = !value.contains(" #") && !value.starts_with(['"', '\'']) && !value.ends_with(['"', '\'']);
    if plain {
        Ok(value.to_owned())
    } else if !value.contains('"') {
        Ok(format!("\"{value}\""))
    } else if !value.contains('\'') {
        Ok(format!("'{value}'"))
    } else {
        Err(AppError::new(
            ErrorKind::Malformed,
            format!("The value for {key} cannot be saved: it holds both kinds of quote mark."),
        ))
    }
}

/// `text` with each `(key, value)` set (`None`: removed), every other line
/// (comments, blank lines, unrelated keys) kept as it was, in place. A key
/// set on several lines keeps the first, now changed; a new key goes at the
/// end. The BOM and the line endings are kept.
fn rewrite_env(text: &str, changes: &[(&str, Option<&str>)]) -> AppResult<String> {
    let (bom, body) = match text.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", text),
    };
    let newline = if body.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = body.lines().map(str::to_owned).collect();
    for (key, value) in changes {
        let line = value.map(|v| encode(key, v).map(|v| format!("{key}={v}"))).transpose()?;
        let mut out = Vec::with_capacity(lines.len() + 1);
        let mut done = false;
        for existing in lines {
            if !sets(&existing, key) {
                out.push(existing);
            } else if !done {
                done = true;
                out.extend(line.clone());
            }
        }
        if !done {
            out.extend(line);
        }
        lines = out;
    }
    let mut result = String::from(bom);
    for line in &lines {
        result.push_str(line);
        result.push_str(newline);
    }
    Ok(result)
}

#[allow(clippy::needless_pass_by_value, reason = "used as a map_err adapter")]
fn write_failed(path: &Path, e: &std::io::Error) -> AppError {
    AppError::new(ErrorKind::NotFound, "The identification settings could not be saved.")
        .with_detail(format!("{}: {e}", path.display()))
}

/// Replaces `path` with `bytes`: staged in the same folder, flushed, renamed
/// over it, so a crash leaves the old file or the new one.
fn write_atomic(path: &Path, bytes: &[u8]) -> AppResult<()> {
    use std::io::Write as _;
    let folder = path.parent().unwrap_or_else(|| Path::new("."));
    let mut staged = tempfile::Builder::new()
        .prefix(".rbxport-settings-")
        .suffix(".tmp")
        .tempfile_in(folder)
        .map_err(|e| write_failed(folder, &e))?;
    staged.write_all(bytes).map_err(|e| write_failed(path, &e))?;
    staged.as_file().sync_all().map_err(|e| write_failed(path, &e))?;
    staged.persist(path).map_err(|e| write_failed(path, &e.error))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const SECRET: &str = "sekrit-0123456789";

    fn sources(root: &Path, env: &[(&'static str, &'static str)], dev: Vec<PathBuf>) -> Sources {
        let env: Vec<(&'static str, &'static str)> = env.to_vec();
        Sources {
            root: root.to_owned(),
            dev_files: dev,
            env: Arc::new(move |key| env.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_owned())),
        }
    }

    fn update(json: &str) -> IdentifySettingsUpdateDto {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn absent_null_and_string_mean_keep_clear_and_replace() {
        let dir = tempfile::tempdir().unwrap();
        let s = sources(dir.path(), &[], Vec::new());
        s.save(&update(&format!(r#"{{"acoustidKey":"  {SECRET}  ","discogsToken":"tok","mbUserAgent":"ua/1 ( a@b.c )"}}"#)), || {})
            .unwrap();
        assert_eq!(s.resolve(ACOUSTID_KEY), Some((SECRET.to_owned(), KeySource::Settings)), "trimmed");
        // Absent: kept.
        s.save(&update(r#"{"autoApply":true}"#), || {}).unwrap();
        assert_eq!(s.resolve(ACOUSTID_KEY).map(|r| r.1), Some(KeySource::Settings));
        assert_eq!(s.resolve(DISCOGS_TOKEN).map(|r| r.1), Some(KeySource::Settings));
        // Null and the empty string: cleared.
        s.save(&update(r#"{"acoustidKey":null,"discogsToken":"   "}"#), || {}).unwrap();
        assert_eq!(s.resolve(ACOUSTID_KEY), None);
        assert_eq!(s.resolve(DISCOGS_TOKEN), None);
        let dto = s.dto();
        assert_eq!((dto.acoustid_key, dto.discogs_token), (None, None));
        assert_eq!((dto.mb_user_agent.as_str(), dto.mb_user_agent_source), ("ua/1 ( a@b.c )", Some(KeySource::Settings)));
        assert!(dto.auto_apply);
        assert_eq!(update(r#"{"acoustidKey":null}"#).acoustid_key, SecretUpdate::Clear);
        assert_eq!(update("{}").acoustid_key, SecretUpdate::Keep);
        // A member the contract does not have is refused.
        assert!(serde_json::from_str::<IdentifySettingsUpdateDto>(r#"{"acoustidKeyValue":"x"}"#).is_err());
    }

    #[test]
    fn a_secret_is_never_in_the_dto_the_debug_text_or_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = sources(dir.path(), &[], Vec::new());
        let set = update(&format!(r#"{{"acoustidKey":"{SECRET}","discogsToken":"{SECRET}"}}"#));
        assert!(!format!("{set:?}").contains(SECRET));
        s.save(&set, || {}).unwrap();
        let dto = s.dto();
        assert!(!serde_json::to_string(&dto).unwrap().contains(SECRET));
        assert!(!format!("{dto:?}").contains(SECRET));
        // An error about a key names the key, not the value.
        let bad = format!("{SECRET}\nINJECTED=1");
        let error = s.save(&IdentifySettingsUpdateDto { acoustid_key: SecretUpdate::Set(bad), ..Default::default() }, || {}).unwrap_err();
        let shown = format!("{error:?} {}", serde_json::to_string(&error).unwrap());
        assert!(shown.contains(ACOUSTID_KEY) && !shown.contains(SECRET), "{shown}");
        // And the refused update wrote nothing.
        assert_eq!(s.resolve(ACOUSTID_KEY).map(|r| r.0).as_deref(), Some(SECRET));
        assert!(!std::fs::read_to_string(s.env_file()).unwrap().contains("INJECTED"));
        // The missing-key error never shows a value either.
        let partial = sources(dir.path(), &[], Vec::new());
        partial.save(&update(r#"{"mbUserAgent":""}"#), || {}).unwrap();
        let error = format!("{:?}", partial.config().unwrap_err());
        assert!(error.contains(MB_USER_AGENT) && !error.contains(SECRET), "{error}");
    }

    #[test]
    fn the_env_file_is_rewritten_keeping_every_other_line() {
        let dir = tempfile::tempdir().unwrap();
        let s = sources(dir.path(), &[], Vec::new());
        let original = "\u{feff}# my keys\r\nOTHER=keep me # really\r\n\r\nACOUSTID_API_KEY=old\r\n# trailing comment\r\nACOUSTID_API_KEY=dupe\r\n";
        std::fs::write(s.env_file(), original).unwrap();
        s.save(&update(r#"{"acoustidKey":"new value # with hash","mbUserAgent":"ua"}"#), || {}).unwrap();
        let text = std::fs::read_to_string(s.env_file()).unwrap();
        assert_eq!(
            text,
            "\u{feff}# my keys\r\nOTHER=keep me # really\r\n\r\nACOUSTID_API_KEY=\"new value # with hash\"\r\n# trailing comment\r\nMB_USER_AGENT=ua\r\n"
        );
        assert_eq!(s.resolve(ACOUSTID_KEY).map(|r| r.0).as_deref(), Some("new value # with hash"));
        s.save(&update(r#"{"acoustidKey":null}"#), || {}).unwrap();
        let text = std::fs::read_to_string(s.env_file()).unwrap();
        assert!(!text.contains(ACOUSTID_KEY) && text.contains("OTHER=keep me # really"), "{text}");
        // Staged files never outlive the save.
        let names: Vec<String> =
            std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| !n.starts_with(".rbxport-settings-")), "{names:?}");
    }

    #[test]
    fn each_key_is_resolved_on_its_own_and_its_source_reported() {
        let dir = tempfile::tempdir().unwrap();
        let dev = dir.path().join("dev.env");
        std::fs::write(&dev, "ACOUSTID_API_KEY=dev\nDISCOGS_TOKEN=dev\nMB_USER_AGENT=dev-ua\n").unwrap();
        let root = dir.path().join("data");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(super::super::ENV_FILE), "ACOUSTID_API_KEY=file\nDISCOGS_TOKEN=file\n").unwrap();
        let s = sources(&root, &[(ACOUSTID_KEY, "env"), (DISCOGS_TOKEN, "  ")], vec![dev.clone()]);
        assert_eq!(s.resolve(ACOUSTID_KEY), Some(("env".to_owned(), KeySource::Environment)));
        assert_eq!(s.resolve(DISCOGS_TOKEN), Some(("file".to_owned(), KeySource::Settings)), "an empty variable is unset");
        assert_eq!(s.resolve(MB_USER_AGENT), Some(("dev-ua".to_owned(), KeySource::DevFile)));
        let dto = s.dto();
        assert_eq!(
            (dto.acoustid_key, dto.discogs_token, dto.mb_user_agent_source),
            (Some(KeySource::Environment), Some(KeySource::Settings), Some(KeySource::DevFile))
        );
        assert_eq!(dto.settings_file, root.join(super::super::ENV_FILE).display().to_string());
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["acoustidKey"], "environment");
        assert_eq!(json["mbUserAgentSource"], "devFile");
        assert_eq!(json["genrePolicy"], "fillEmpty");
        let config = s.config().unwrap();
        assert_eq!((config.acoustid_key.expose(), config.mb_user_agent.as_str()), ("env", "dev-ua"));
        // Nothing anywhere: null.
        let empty = sources(&dir.path().join("none"), &[], Vec::new());
        assert_eq!(empty.dto().acoustid_key, None);
        assert_eq!(empty.dto().mb_user_agent, "");
    }

    #[test]
    fn the_threshold_is_validated_not_clamped_and_defaults_hold() {
        let dir = tempfile::tempdir().unwrap();
        let s = sources(dir.path(), &[], Vec::new());
        let dto = s.dto();
        assert_eq!((dto.genre_policy, dto.auto_apply, dto.auto_apply_threshold), (GenrePolicyDto::FillEmpty, false, 0.92));
        for bad in [0.91, 1.01, -1.0, f64::NAN, f64::INFINITY] {
            let error = s
                .save(
                    &IdentifySettingsUpdateDto { auto_apply_threshold: Some(bad), auto_apply: Some(true), ..Default::default() },
                    || {},
                )
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::Malformed, "{bad}");
        }
        assert!(!s.dto().auto_apply, "a refused update changes nothing");
        s.save(&update(r#"{"autoApplyThreshold":0.97,"genrePolicy":"never"}"#), || {}).unwrap();
        let dto = s.dto();
        assert_eq!((dto.auto_apply_threshold, dto.genre_policy), (0.97, GenrePolicyDto::Never));
        s.save(&update(r#"{"autoApplyThreshold":1}"#), || {}).unwrap();
        assert_eq!(s.dto().auto_apply_threshold, 1.0);
        // A hand-edited file out of range is brought back into it.
        std::fs::write(dir.path().join(SETTINGS_FILE), r#"{"autoApplyThreshold":0.1}"#).unwrap();
        assert_eq!(s.dto().auto_apply_threshold, 0.92);
        std::fs::write(dir.path().join(SETTINGS_FILE), "not json").unwrap();
        assert_eq!(s.stored(), Stored::default());
    }

    #[test]
    fn a_corrupt_settings_json_is_refused_not_reset() {
        let dir = tempfile::tempdir().unwrap();
        let s = sources(dir.path(), &[], Vec::new());
        std::fs::write(dir.path().join(SETTINGS_FILE), "{ hand edited").unwrap();
        let error = s.save(&update(r#"{"autoApply":true}"#), || panic!("nothing is written")).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Malformed);
        assert!(error.message.contains(SETTINGS_FILE), "{}", error.message);
        assert_eq!(std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap(), "{ hand edited");
        // A key alone is saved, and leaves the file as it is.
        s.save(&update(r#"{"acoustidKey":"k"}"#), || {}).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap(), "{ hand edited");
        assert_eq!(s.resolve(ACOUSTID_KEY).map(|r| r.1), Some(KeySource::Settings));
    }

    #[test]
    fn before_write_runs_once_before_any_write_and_not_for_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let s = sources(dir.path(), &[], Vec::new());
        let calls = std::cell::Cell::new(0);
        s.save(&update(r#"{"autoApplyThreshold":0.1}"#), || calls.set(calls.get() + 1)).unwrap_err();
        s.save(&update("{}"), || calls.set(calls.get() + 1)).unwrap();
        assert_eq!(calls.get(), 0);
        s.save(&update(r#"{"autoApply":true,"acoustidKey":"k"}"#), || {
            assert!(!dir.path().join(SETTINGS_FILE).exists() && !s.env_file().exists(), "before anything is written");
            calls.set(calls.get() + 1);
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn the_stored_settings_become_the_pipeline_settings() {
        let stored = Stored { genre_policy: GenrePolicyDto::Replace, auto_apply: true, auto_apply_threshold: 0.95 };
        assert_eq!(
            stored.pipeline(),
            pipeline::Settings { auto_apply: true, genre: GenrePolicy::Replace, auto_apply_threshold: 0.95 }
        );
        assert_eq!(Stored::default().pipeline(), pipeline::Settings::default());
    }
}
