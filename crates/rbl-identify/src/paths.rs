//! One spelling per file. The store keys tracks by path exactly as given
//! ([`crate::store::TrackKey`]), so every path is canonicalised before it
//! gets there: `C:\Music\.\A.mp3`, `c:/music/a.mp3` and a path through a
//! symlink or junction all become the one the file system reports.
//!
//! On Windows `std::fs::canonicalize` returns a verbatim path
//! (`\\?\C:\Music\A.mp3`). The `\\?\` prefix is removed so the key reads
//! as users write paths, unless that would change which file it names: a
//! component Windows would reinterpret without it (a trailing dot or space,
//! a device name such as `CON` or `NUL.txt`) keeps the verbatim form. Long
//! paths lose nothing: Rust's file APIs add the prefix again themselves for
//! a path over the legacy 260-character limit.

use std::path::{Path, PathBuf};

use crate::error::{IdentifyError, Result};

/// The canonical form of an existing file's path: absolute, symlinks and
/// `.`/`..` resolved, the case the file system stores, and without the
/// Windows verbatim prefix where that is safe ([`strip_verbatim`]).
pub fn canonical(path: &Path) -> Result<PathBuf> {
    let resolved = std::fs::canonicalize(path)
        .map_err(|e| IdentifyError::Io(std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))))?;
    Ok(strip_verbatim(&resolved))
}

/// [`canonical`] as the text the store keys a track by. A path that is not
/// valid Unicode is refused rather than stored lossily, which could give two
/// files one key.
pub fn canonical_key(path: &Path) -> Result<String> {
    let canonical = canonical(path)?;
    canonical
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| IdentifyError::Io(std::io::Error::other(format!("{}: the path is not valid Unicode", path.display()))))
}

/// A Windows verbatim path without its `\\?\` prefix when the result names
/// the same file: `\\?\C:\x` → `C:\x`, `\\?\UNC\server\share\x` →
/// `\\server\share\x`. Anything else (another platform's path, a volume
/// GUID path, a component Windows would reinterpret) is returned as it is.
#[must_use]
pub fn strip_verbatim(path: &Path) -> PathBuf {
    path.to_str().and_then(strip_verbatim_str).map_or_else(|| path.to_owned(), PathBuf::from)
}

fn strip_verbatim_str(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\")?;
    let (plain, components) = if let Some(unc) = rest.strip_prefix(r"UNC\") {
        let mut parts = unc.splitn(3, '\\');
        let (server, share) = (parts.next()?, parts.next()?);
        if server.is_empty() || share.is_empty() {
            return None;
        }
        (format!(r"\\{unc}"), parts.next().unwrap_or(""))
    } else {
        let bytes = rest.as_bytes();
        let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if !drive || (bytes.len() > 2 && bytes[2] != b'\\') {
            return None;
        }
        (rest.to_owned(), rest.get(3..).unwrap_or(""))
    };
    components.split('\\').filter(|c| !c.is_empty()).all(survives_without_verbatim).then_some(plain)
}

/// Whether Windows reads a path component the same without the verbatim
/// prefix: not `.` or `..`, no `/`, no trailing dot or space, and not a DOS
/// device name (with or without an extension).
fn survives_without_verbatim(component: &str) -> bool {
    const DEVICES: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    if component == "." || component == ".." || component.contains('/') || component.ends_with(['.', ' ']) {
        return false;
    }
    let stem = component.split('.').next().unwrap_or(component).trim_end().to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix).is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))
    };
    !(DEVICES.contains(&stem.as_str()) || numbered("COM") || numbered("LPT"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn strip(path: &str) -> String {
        strip_verbatim(Path::new(path)).to_str().unwrap().to_owned()
    }

    #[test]
    fn the_verbatim_prefix_goes_where_it_is_safe() {
        assert_eq!(strip(r"\\?\C:\Music\A.mp3"), r"C:\Music\A.mp3");
        assert_eq!(strip(r"\\?\C:\"), r"C:\");
        assert_eq!(strip(r"\\?\UNC\nas\music\A.mp3"), r"\\nas\music\A.mp3");
        let long = format!(r"\\?\D:\{}\track.flac", "deep folder name".repeat(30));
        assert_eq!(strip(&long), &long[4..], "long paths too: Rust re-adds the prefix itself");
    }

    #[test]
    fn a_path_windows_would_reinterpret_keeps_it() {
        for kept in [
            r"\\?\C:\Music\trailing dot.\A.mp3",
            r"\\?\C:\Music\trailing space \A.mp3",
            r"\\?\C:\Music\CON",
            r"\\?\C:\Music\nul.txt",
            r"\\?\C:\Music\com1.mp3",
            r"\\?\C:\Music\LPT9",
            r"\\?\C:\Music\a/b.mp3",
            r"\\?\C:\Music\..\A.mp3",
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\A.mp3",
            r"\\?\UNC\server",
            r"\\?\C:A.mp3",
        ] {
            assert_eq!(strip(kept), kept);
        }
        // Device-like names that are not devices are fine.
        assert_eq!(strip(r"\\?\C:\Music\CONSOLE.mp3"), r"C:\Music\CONSOLE.mp3");
        assert_eq!(strip(r"\\?\C:\Music\COM10.mp3"), r"C:\Music\COM10.mp3");
        // Not verbatim at all: unchanged.
        assert_eq!(strip("/home/user/a.mp3"), "/home/user/a.mp3");
        assert_eq!(strip(r"C:\Music\A.mp3"), r"C:\Music\A.mp3");
    }

    #[test]
    fn two_spellings_of_one_file_give_one_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let file = dir.path().join("Track.mp3");
        std::fs::write(&file, b"x").unwrap();
        let key = canonical_key(&file).unwrap();
        assert!(!key.starts_with(r"\\?\"), "{key}");
        assert!(Path::new(&key).is_absolute());
        let roundabout = dir.path().join("sub").join("..").join(".").join("Track.mp3");
        assert_eq!(canonical_key(&roundabout).unwrap(), key);
        // The canonical path opens the same file.
        assert_eq!(std::fs::read(&key).unwrap(), b"x");
        #[cfg(windows)]
        {
            // NTFS is case-insensitive; the key takes the stored case.
            let shouted = dir.path().join("TRACK.MP3");
            assert_eq!(canonical_key(&shouted).unwrap(), key);
        }
    }

    #[test]
    fn a_missing_file_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.mp3");
        let error = canonical(&missing).unwrap_err();
        assert!(error.to_string().contains("missing.mp3"), "{error}");
    }
}
