//! On-disk cache of scanned track metadata, so a launch can show the library
//! before any file has been opened. Holds tags only, never artwork. Any problem
//! reading it falls back to an empty cache, which means a full scan; it never
//! touches the playlist store.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::media::Track;

/// Bump when the meaning of an entry changes; an older file is then ignored.
const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    size: u64,
    /// Modified time as seconds and nanoseconds since the Unix epoch.
    modified: (u64, u32),
    title: String,
    artist: String,
    album: String,
    duration: (u64, u32),
    year: Option<u32>,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: Vec<Entry>,
}

#[derive(Clone, Debug, Default)]
pub struct LibraryCache {
    entries: HashMap<PathBuf, Entry>,
}

fn epoch_parts(time: SystemTime) -> Option<(u64, u32)> {
    let since = time.duration_since(UNIX_EPOCH).ok()?;
    Some((since.as_secs(), since.subsec_nanos()))
}

fn from_parts((secs, nanos): (u64, u32)) -> SystemTime {
    UNIX_EPOCH + Duration::new(secs, nanos)
}

impl LibraryCache {
    pub fn default_path() -> Option<PathBuf> {
        Some(
            dirs::data_local_dir()?
                .join("noir-player")
                .join("library-cache.json"),
        )
    }

    /// An unreadable, corrupt or other-version file yields an empty cache.
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        match serde_json::from_slice::<CacheFile>(&bytes) {
            Ok(file) if file.version == VERSION => Self {
                entries: file
                    .entries
                    .into_iter()
                    .map(|entry| (entry.path.clone(), entry))
                    .collect(),
            },
            _ => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut entries: Vec<Entry> = self.entries.values().cloned().collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let bytes = serde_json::to_vec(&CacheFile {
            version: VERSION,
            entries,
        })
        .map_err(std::io::Error::other)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        let written = fs::File::create(&temporary).and_then(|mut file| {
            file.write_all(&bytes)?;
            file.sync_all()
        });
        match written.and_then(|()| fs::rename(&temporary, path)) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The cached track for `path`, only while the file is exactly as it was
    /// when it was read.
    pub fn lookup(&self, path: &Path, size: u64, modified: SystemTime) -> Option<Track> {
        let entry = self.entries.get(path)?;
        if entry.size != size || entry.modified != epoch_parts(modified)? {
            return None;
        }
        Some(entry.to_track())
    }

    /// Remembers `track` as read from a file of `size` bytes. A file with no
    /// readable modified time can never be validated, so it is not kept.
    pub fn insert(&mut self, track: &Track, size: u64) {
        let Some(modified) = track.added.and_then(epoch_parts) else {
            return;
        };
        self.entries.insert(
            track.path.clone(),
            Entry {
                path: track.path.clone(),
                size,
                modified,
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                duration: (track.duration.as_secs(), track.duration.subsec_nanos()),
                year: track.year,
            },
        );
    }

    /// Every cached track under one of `folders`, unverified: for showing
    /// something immediately while the real scan runs.
    pub fn tracks_under(&self, folders: &[PathBuf]) -> Vec<Track> {
        self.entries
            .values()
            .filter(|entry| folders.iter().any(|folder| entry.path.starts_with(folder)))
            .map(Entry::to_track)
            .collect()
    }

    /// Whether `other` holds exactly these entries.
    pub fn same_as(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Entry {
    fn to_track(&self) -> Track {
        Track {
            path: self.path.clone(),
            title: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            duration: Duration::new(self.duration.0, self.duration.1),
            artwork: None,
            artwork_image: None,
            year: self.year,
            added: Some(from_parts(self.modified)),
            search_key: Track::build_search_key(&self.title, &self.artist, &self.album),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "noir-cache-test-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn track(path: &str, modified: u64) -> Track {
        Track {
            path: path.into(),
            title: "Title".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            duration: Duration::new(185, 250),
            artwork: None,
            artwork_image: None,
            year: Some(1999),
            added: Some(UNIX_EPOCH + Duration::new(modified, 5)),
            search_key: Track::build_search_key("Title", "Artist", "Album"),
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = TempDir::new("roundtrip");
        let file = dir.0.join("library-cache.json");
        let mut cache = LibraryCache::default();
        cache.insert(&track("a.mp3", 100), 4096);
        cache.save(&file).unwrap();

        let loaded = LibraryCache::load(&file);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.same_as(&cache));
        let hit = loaded
            .lookup(Path::new("a.mp3"), 4096, UNIX_EPOCH + Duration::new(100, 5))
            .expect("unchanged file hits");
        assert_eq!(hit.title, "Title");
        assert_eq!(hit.duration, Duration::new(185, 250));
        assert_eq!(hit.year, Some(1999));
        assert_eq!(hit.added, Some(UNIX_EPOCH + Duration::new(100, 5)));
        assert_eq!(
            hit.search_key,
            Track::build_search_key("Title", "Artist", "Album")
        );
        assert!(!dir.0.join("library-cache.json.tmp").exists());
    }

    #[test]
    fn changed_mtime_or_size_is_a_miss() {
        let mut cache = LibraryCache::default();
        cache.insert(&track("a.mp3", 100), 4096);
        let same = UNIX_EPOCH + Duration::new(100, 5);
        assert!(cache.lookup(Path::new("a.mp3"), 4096, same).is_some());
        assert!(cache
            .lookup(Path::new("a.mp3"), 4096, UNIX_EPOCH + Duration::new(101, 5))
            .is_none());
        assert!(cache.lookup(Path::new("a.mp3"), 4097, same).is_none());
        assert!(cache.lookup(Path::new("b.mp3"), 4096, same).is_none());
    }

    #[test]
    fn corrupt_missing_or_old_version_falls_back_to_empty() {
        let dir = TempDir::new("corrupt");
        let file = dir.0.join("library-cache.json");
        assert!(LibraryCache::load(&file).is_empty());
        fs::write(&file, b"{ not json").unwrap();
        assert!(LibraryCache::load(&file).is_empty());
        fs::write(&file, br#"{"version":0,"entries":[]}"#).unwrap();
        assert!(LibraryCache::load(&file).is_empty());
        fs::write(&file, br#"{"version":1,"entries":[{"path":1}]}"#).unwrap();
        assert!(LibraryCache::load(&file).is_empty());
    }

    #[test]
    fn untimed_files_are_not_cached_and_folders_filter() {
        let mut cache = LibraryCache::default();
        let mut untimed = track("x/a.mp3", 1);
        untimed.added = None;
        cache.insert(&untimed, 1);
        assert!(cache.is_empty());
        cache.insert(&track("x/a.mp3", 1), 1);
        cache.insert(&track("y/b.mp3", 1), 1);
        let under = cache.tracks_under(&[PathBuf::from("x")]);
        assert_eq!(under.len(), 1);
        assert_eq!(under[0].path, PathBuf::from("x/a.mp3"));
    }
}
