use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

/// Directories larger than this are truncated in the UI (a "…" row is shown
/// instead of silently hiding entries), so `cd /` or a huge `node_modules`
/// can't hang rendering.
pub const MAX_ENTRIES_PER_DIR: usize = 1000;

// ponytail: const instead of a setting; wire into SettingsContent if someone
// actually asks to toggle it from the UI.
const SHOW_HIDDEN: bool = false;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirListing {
    pub entries: Vec<TreeEntry>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirState {
    Loading,
    Loaded(DirListing),
    Error(String),
}

/// Reads one directory level (non-recursive). This does blocking IO and must
/// only be called from a background task (see `SessionsPanel::ensure_dir_loaded`
/// in panel.rs), never from the UI thread.
pub fn read_dir(dir: &Path) -> DirState {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(error) => return DirState::Error(error.to_string()),
    };

    let mut entries = Vec::new();
    let mut truncated = false;
    for dir_entry in read_dir {
        let dir_entry = match dir_entry {
            Ok(dir_entry) => dir_entry,
            // A single unreadable entry (e.g. removed mid-scan) shouldn't
            // fail the whole listing.
            Err(_) => continue,
        };
        let name = dir_entry.file_name().to_string_lossy().into_owned();
        if !SHOW_HIDDEN && name.starts_with('.') {
            continue;
        }
        if entries.len() >= MAX_ENTRIES_PER_DIR {
            truncated = true;
            break;
        }
        let is_dir = dir_entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        entries.push(TreeEntry { path: dir_entry.path(), name, is_dir });
    }

    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    DirState::Loaded(DirListing { entries, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_directories_first_then_alphabetical_case_insensitive() {
        let dir = tempfile_dir();
        let directories = ["Apple", "zebra"];
        let files = ["banana.txt", "Cherry.txt"];
        for name in directories {
            fs::create_dir(dir.join(name)).unwrap();
        }
        for name in files {
            fs::write(dir.join(name), b"").unwrap();
        }

        let DirState::Loaded(listing) = read_dir(&dir) else {
            panic!("expected Loaded state");
        };
        let names: Vec<&str> = listing.entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["Apple", "zebra", "banana.txt", "Cherry.txt"]);
        assert!(!listing.truncated);
    }

    #[test]
    fn skips_hidden_entries_by_default() {
        let dir = tempfile_dir();
        fs::write(dir.join(".hidden"), b"").unwrap();
        fs::write(dir.join("visible"), b"").unwrap();

        let DirState::Loaded(listing) = read_dir(&dir) else {
            panic!("expected Loaded state");
        };
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "visible");
    }

    #[test]
    fn unreadable_dir_is_an_error_not_a_panic() {
        let dir = tempfile_dir().join("does-not-exist");
        match read_dir(&dir) {
            DirState::Error(_) => {}
            other => panic!("expected Error, got {other:?}"),
        }
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hive-file-tree-test-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
