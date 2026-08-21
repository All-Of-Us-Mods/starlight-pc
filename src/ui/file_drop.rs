//! Sorting files dragged onto the app into the two kinds we can act on:
//! BepInEx plugins (`.dll`) and exported profiles (`.zip`).

use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DroppedFiles {
    /// Plugin DLLs, to copy into a profile's `BepInEx/plugins`.
    pub plugins: Vec<String>,
    /// Profile archives, to import as new profiles.
    pub archives: Vec<String>,
    /// Dropped paths that are neither — used to explain why nothing happened.
    pub rejected: usize,
}

impl DroppedFiles {
    pub fn classify(paths: &[PathBuf]) -> Self {
        let mut dropped = Self::default();
        for path in paths {
            if has_extension(path, "dll") {
                dropped.plugins.push(path.to_string_lossy().into_owned());
            } else if has_extension(path, "zip") {
                dropped.archives.push(path.to_string_lossy().into_owned());
            } else {
                dropped.rejected += 1;
            }
        }
        dropped
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty() && self.archives.is_empty()
    }
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .is_some_and(|found| found.eq_ignore_ascii_case(extension))
}

#[cfg(test)]
mod tests {
    use super::DroppedFiles;
    use std::path::PathBuf;

    #[test]
    fn classifies_by_extension_ignoring_case() {
        let dropped = DroppedFiles::classify(&[
            PathBuf::from("C:/mods/Reactor.dll"),
            PathBuf::from("C:/mods/TownOfUs.DLL"),
            PathBuf::from("C:/profiles/backup.Zip"),
            PathBuf::from("C:/notes.txt"),
        ]);

        assert_eq!(dropped.plugins.len(), 2);
        assert_eq!(dropped.archives, vec!["C:/profiles/backup.Zip".to_string()]);
        assert_eq!(dropped.rejected, 1);
        assert!(!dropped.is_empty());
    }

    #[test]
    fn a_drop_of_only_unsupported_files_is_empty() {
        let dropped = DroppedFiles::classify(&[
            PathBuf::from("C:/mods/readme.md"),
            // A disabled plugin is not a `.dll` — importing it would land a
            // file BepInEx never loads.
            PathBuf::from("C:/mods/Reactor.dll.disabled"),
        ]);

        assert!(dropped.is_empty());
        assert_eq!(dropped.rejected, 2);
    }
}
