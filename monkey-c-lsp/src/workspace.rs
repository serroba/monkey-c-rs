//! The set of Monkey C sources the server can resolve names against.
//!
//! Cross-file navigation has to see files the editor never opened, so the workspace seeds itself
//! from disk at startup and then lets client notifications take precedence: an open buffer is the
//! truth for its file, and closing one falls back to disk rather than forgetting the file.
//!
//! Sources are keyed by path rather than URI — see [`crate::uri`] for why.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Workspace {
    files: HashMap<PathBuf, String>,
}

impl Workspace {
    /// Seed from every `.mc` file under `root`, without disturbing buffers the client already sent.
    pub fn load_root(&mut self, root: &Path) {
        for path in sources(root) {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };

            self.files.entry(path).or_insert(text);
        }
    }

    pub fn set(&mut self, path: PathBuf, text: String) {
        self.files.insert(path, text);
    }

    /// Drop the client's buffer for `path`. The file stays in the workspace at its on-disk contents
    /// so other files can still resolve into it; only a file that has gone from disk is forgotten.
    pub fn close(&mut self, path: &Path) {
        match std::fs::read_to_string(path) {
            Ok(text) => self.files.insert(path.to_path_buf(), text),
            Err(_) => self.files.remove(path),
        };
    }

    pub fn text(&self, path: &Path) -> Option<&str> {
        self.files.get(path).map(String::as_str)
    }

    /// Every known source, in a stable path order so results don't shuffle between requests.
    pub fn sources(&self) -> Vec<(&Path, &str)> {
        let mut sources: Vec<_> = self
            .files
            .iter()
            .map(|(path, text)| (path.as_path(), text.as_str()))
            .collect();
        sources.sort_by_key(|(path, _)| *path);

        sources
    }
}

/// Every `.mc` file under `root`.
///
/// Build output is skipped: the Connect IQ toolchain copies sources into its output directory, and
/// indexing both copies would offer two identical destinations for every jump.
fn sources(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if !is_skipped(&path) {
                    pending.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == "mc") {
                found.push(path);
            }
        }
    }

    found
}

fn is_skipped(directory: &Path) -> bool {
    let Some(name) = directory.file_name().and_then(|name| name.to_str()) else {
        return true;
    };

    name.starts_with('.') || matches!(name, "bin" | "build" | "target" | "node_modules")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_buffer_wins_over_disk() {
        let mut workspace = Workspace::default();
        workspace.set(PathBuf::from("/src/Combo.mc"), "buffer".to_string());
        // A root scan must not clobber what the client already sent.
        workspace.load_root(Path::new("/definitely/not/a/directory"));

        assert_eq!(workspace.text(Path::new("/src/Combo.mc")), Some("buffer"));
    }

    #[test]
    fn closing_a_file_with_no_disk_copy_forgets_it() {
        let mut workspace = Workspace::default();
        workspace.set(PathBuf::from("/src/Gone.mc"), "buffer".to_string());
        workspace.close(Path::new("/src/Gone.mc"));

        assert_eq!(workspace.text(Path::new("/src/Gone.mc")), None);
    }

    #[test]
    fn sources_are_ordered_by_path() {
        let mut workspace = Workspace::default();
        workspace.set(PathBuf::from("/src/B.mc"), String::new());
        workspace.set(PathBuf::from("/src/A.mc"), String::new());

        let paths: Vec<_> = workspace.sources().into_iter().map(|(p, _)| p).collect();
        assert_eq!(paths, vec![Path::new("/src/A.mc"), Path::new("/src/B.mc")]);
    }

    #[test]
    fn build_output_is_not_indexed() {
        assert!(is_skipped(Path::new("/project/build")));
        assert!(is_skipped(Path::new("/project/bin")));
        assert!(is_skipped(Path::new("/project/.git")));
        assert!(!is_skipped(Path::new("/project/source")));
    }
}
