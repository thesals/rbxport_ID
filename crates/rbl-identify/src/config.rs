//! Keys and the MusicBrainz User-Agent, read from `.env.identify` (or the
//! environment). A [`Secret`] never prints its value: not in `Debug`, not in
//! `Display`, not in an error.

use std::path::Path;

use crate::error::{IdentifyError, Result};

/// A value that must reach only its own service.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, for building the one request that carries it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub acoustid_key: Secret,
    /// Discogs is used from Phase 2; optional until then.
    pub discogs_token: Option<Secret>,
    /// Sent on every MusicBrainz request, as their rules require.
    pub mb_user_agent: String,
}

impl Config {
    /// Reads `KEY=value` lines from `path`; a variable already set in the
    /// environment wins over the file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| IdentifyError::Config(format!("cannot read {}: {e}", path.display())))?;
        // Notepad saves UTF-8 with a BOM, which `trim` does not remove.
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned();
        Self::from_lookup(|key| std::env::var(key).ok().filter(|v| !v.is_empty()).or_else(|| file_value(&text, key)))
    }

    /// Builds a config from any key lookup; `load` and tests both use it.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let required = |key: &str| lookup(key).ok_or_else(|| IdentifyError::Config(format!("{key} is not set")));
        Ok(Self {
            acoustid_key: Secret::new(required("ACOUSTID_API_KEY")?),
            discogs_token: lookup("DISCOGS_TOKEN").map(Secret::new),
            mb_user_agent: required("MB_USER_AGENT")?,
        })
    }
}

/// The value of `key` in dotenv-style text: `KEY=value`, optional quotes,
/// `#` comment lines and blank lines ignored, and an unquoted value's
/// trailing ` # comment` dropped (a quoted value keeps its `#`).
fn file_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return None;
        }
        let (name, value) = line.split_once('=')?;
        if name.trim() != key {
            return None;
        }
        let value = value.trim();
        let quoted = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')));
        let value = match quoted {
            Some(inner) => inner,
            None => value.split_once(" #").map_or(value, |(v, _)| v).trim_end(),
        };
        (!value.is_empty()).then(|| value.to_owned())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_prints_its_value() {
        let secret = Secret::new("hunter2");
        assert!(!format!("{secret:?}").contains("hunter2"));
        assert!(!format!("{secret}").contains("hunter2"));
        let config = Config::from_lookup(|k| match k {
            "ACOUSTID_API_KEY" => Some("hunter2".into()),
            "MB_USER_AGENT" => Some("rbxport-identify/0.1 ( me@example.com )".into()),
            _ => None,
        })
        .unwrap();
        assert!(!format!("{config:?}").contains("hunter2"));
    }

    #[test]
    fn the_file_format_is_read_as_dotenv() {
        let text = "# keys\nACOUSTID_API_KEY = \"abc\"\n\nMB_USER_AGENT=ua 1.0\nDISCOGS_TOKEN=\n";
        assert_eq!(file_value(text, "ACOUSTID_API_KEY").as_deref(), Some("abc"));
        assert_eq!(file_value(text, "MB_USER_AGENT").as_deref(), Some("ua 1.0"));
        assert_eq!(file_value(text, "DISCOGS_TOKEN"), None);
        assert_eq!(file_value(text, "MISSING"), None);
    }

    #[test]
    fn a_file_saved_with_a_bom_and_inline_comments_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env.identify");
        std::fs::write(&path, "\u{feff}ACOUSTID_API_KEY=abc # the key\r\nMB_USER_AGENT=\"ua 1.0 # not a comment\"\r\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.acoustid_key.expose(), "abc");
        assert_eq!(config.mb_user_agent, "ua 1.0 # not a comment");
    }

    #[test]
    fn a_missing_required_key_names_the_key() {
        let error = Config::from_lookup(|_| None).unwrap_err().to_string();
        assert!(error.contains("ACOUSTID_API_KEY"), "{error}");
    }
}
