//! Cover thumbnails for Discover rows, keyed by `api::art_key`.
//!
//! The renderer keeps a texture for every image it has drawn until the window
//! is told to drop it, so every thumbnail that leaves the cache is queued in
//! `retired` for the next frame to release.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use gpui_kit::RenderImage;

/// Rows that fit a few Discover pages; each thumbnail is about 36 KiB.
pub const CAPACITY: usize = 120;

/// Downloads in flight at once, so a page of results doesn't hog the pool.
pub const MAX_IN_FLIGHT: usize = 4;

#[derive(Clone)]
pub enum ArtState {
    Loading,
    Ready(Arc<RenderImage>),
    /// No service has art for the song, or it couldn't be decoded.
    None,
}

pub struct DiscoverArt {
    entries: HashMap<String, (ArtState, u64)>,
    clock: u64,
    capacity: usize,
    /// Songs waiting for a download slot, in the order they are listed.
    pending: VecDeque<(String, crate::api::Track)>,
    in_flight: HashSet<String>,
    retired: Vec<Arc<RenderImage>>,
}

impl Default for DiscoverArt {
    fn default() -> Self {
        Self::with_capacity(CAPACITY)
    }
}

impl DiscoverArt {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            capacity: capacity.max(1),
            pending: VecDeque::new(),
            in_flight: HashSet::new(),
            retired: Vec::new(),
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// The thumbnail to draw for `key`, if it has arrived. Counts as a use.
    pub fn image(&mut self, key: &str) -> Option<Arc<RenderImage>> {
        let now = self.tick();
        let (state, used) = self.entries.get_mut(key)?;
        *used = now;
        match state {
            ArtState::Ready(image) => Some(image.clone()),
            ArtState::Loading | ArtState::None => None,
        }
    }

    /// Stops queued downloads, e.g. while new results are on their way.
    pub fn hold(&mut self) {
        self.pending.clear();
    }

    /// Replaces the listed songs: art for songs no longer listed is released
    /// and songs still without art are queued for download, top row first.
    pub fn show(&mut self, tracks: &[crate::api::Track]) {
        let keys: Vec<String> = tracks.iter().map(crate::api::art_key).collect();
        self.pending.clear();
        let stale: Vec<String> = self
            .entries
            .keys()
            .filter(|key| !keys.contains(key))
            .cloned()
            .collect();
        for key in stale {
            self.remove(&key);
        }
        for (key, track) in keys.into_iter().zip(tracks) {
            match self.entries.get(&key) {
                Some((ArtState::Loading, _)) if !self.in_flight.contains(&key) => {
                    self.pending.push_back((key, track.clone()));
                }
                Some(_) => {}
                None => {
                    let now = self.tick();
                    self.entries.insert(key.clone(), (ArtState::Loading, now));
                    self.pending.push_back((key, track.clone()));
                    self.evict();
                }
            }
        }
    }

    /// The next song to download, if a slot is free. The caller must report
    /// back through `finish`.
    pub fn next_download(&mut self) -> Option<(String, crate::api::Track)> {
        if self.in_flight.len() >= MAX_IN_FLIGHT {
            return None;
        }
        while let Some((key, track)) = self.pending.pop_front() {
            if matches!(self.entries.get(&key), Some((ArtState::Loading, _)))
                && self.in_flight.insert(key.clone())
            {
                return Some((key, track));
            }
        }
        None
    }

    /// Records a finished download. `Err` means it couldn't be fetched, so the
    /// entry is forgotten and the song is tried again next time it is listed.
    pub fn finish(&mut self, key: &str, outcome: Result<Option<Arc<RenderImage>>, ()>) {
        self.in_flight.remove(key);
        let Some((state @ ArtState::Loading, _)) = self.entries.get_mut(key) else {
            // Evicted or no longer listed while downloading; never drawn, so nothing to release.
            return;
        };
        match outcome {
            Ok(Some(image)) => *state = ArtState::Ready(image),
            Ok(None) => *state = ArtState::None,
            Err(()) => {
                self.entries.remove(key);
            }
        }
    }

    /// Thumbnails whose textures the window should drop on the next frame.
    pub fn take_retired(&mut self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut self.retired)
    }

    fn remove(&mut self, key: &str) {
        if let Some((ArtState::Ready(image), _)) = self.entries.remove(key) {
            self.retired.push(image);
        }
    }

    fn evict(&mut self) {
        while self.entries.len() > self.capacity {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone())
            else {
                return;
            };
            self.remove(&oldest);
        }
    }

    #[cfg(test)]
    fn state(&self, key: &str) -> Option<&ArtState> {
        self.entries.get(key).map(|(state, _)| state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{art_key, Track};

    fn track(name: &str) -> Track {
        Track {
            name: name.into(),
            artist: "Artist".into(),
            url: None,
        }
    }

    fn picture() -> Arc<RenderImage> {
        Arc::new(RenderImage::new(vec![image::Frame::new(
            image::RgbaImage::new(1, 1),
        )]))
    }

    fn download_all(art: &mut DiscoverArt) {
        while let Some((key, _)) = art.next_download() {
            art.finish(&key, Ok(Some(picture())));
        }
    }

    #[test]
    fn listed_songs_are_queued_in_order_and_downloads_are_limited() {
        let tracks: Vec<Track> = (0..6).map(|i| track(&format!("Song {i}"))).collect();
        let mut art = DiscoverArt::default();
        art.show(&tracks);
        let started: Vec<String> = std::iter::from_fn(|| art.next_download())
            .map(|(key, _)| key)
            .collect();
        assert_eq!(started.len(), MAX_IN_FLIGHT);
        assert_eq!(started[0], art_key(&tracks[0]));
        assert!(art.image(&started[0]).is_none(), "still loading");
        art.finish(&started[0], Ok(Some(picture())));
        assert!(art.image(&started[0]).is_some());
        assert!(art.next_download().is_some(), "a freed slot is reused");
    }

    #[test]
    fn songs_still_queued_are_queued_again_when_relisted() {
        let tracks: Vec<Track> = (0..6).map(|i| track(&format!("Song {i}"))).collect();
        let mut art = DiscoverArt::default();
        art.show(&tracks);
        let started: Vec<String> = std::iter::from_fn(|| art.next_download())
            .map(|(key, _)| key)
            .collect();
        assert_eq!(started.len(), MAX_IN_FLIGHT);
        art.hold();
        art.show(&tracks);
        assert!(art.next_download().is_none(), "slots are still busy");
        for key in &started {
            art.finish(key, Ok(Some(picture())));
        }
        let rest: Vec<String> = std::iter::from_fn(|| art.next_download())
            .map(|(key, _)| key)
            .collect();
        assert_eq!(rest, [art_key(&tracks[4]), art_key(&tracks[5])]);
    }

    #[test]
    fn a_song_relisted_while_downloading_is_not_downloaded_twice() {
        let mut art = DiscoverArt::default();
        let tracks = [track("Once")];
        art.show(&tracks);
        let (key, _) = art.next_download().unwrap();
        art.show(&[]);
        art.show(&tracks);
        assert!(art.next_download().is_none());
        art.finish(&key, Ok(Some(picture())));
        assert!(art.image(&key).is_some(), "the running download fills it");
    }

    #[test]
    fn misses_are_remembered_and_failures_are_retried() {
        let mut art = DiscoverArt::default();
        let tracks = [track("Missing"), track("Failed")];
        art.show(&tracks);
        let (first, _) = art.next_download().unwrap();
        let (second, _) = art.next_download().unwrap();
        art.finish(&first, Ok(None));
        art.finish(&second, Err(()));
        assert!(matches!(art.state(&first), Some(ArtState::None)));
        assert!(art.state(&second).is_none());
        art.show(&tracks);
        let retried: Vec<String> = std::iter::from_fn(|| art.next_download())
            .map(|(key, _)| key)
            .collect();
        assert_eq!(retried, [second]);
    }

    #[test]
    fn evicted_and_unlisted_thumbnails_are_queued_for_release() {
        let mut art = DiscoverArt::with_capacity(3);
        let first: Vec<Track> = (0..3).map(|i| track(&format!("A{i}"))).collect();
        art.show(&first);
        download_all(&mut art);
        assert!(art.take_retired().is_empty());
        art.image(&art_key(&first[0]));

        // Replacing the results releases every song that is not listed again.
        let second = [first[0].clone(), track("B0")];
        art.show(&second);
        assert_eq!(art.take_retired().len(), 2);
        assert!(
            art.image(&art_key(&first[0])).is_some(),
            "kept while listed"
        );
        download_all(&mut art);

        // Listing more songs than fit evicts the least recently drawn.
        art.image(&art_key(&second[1]));
        let mut more = second.to_vec();
        more.extend([track("C0"), track("C1")]);
        art.show(&more);
        let retired = art.take_retired();
        assert_eq!(retired.len(), 1);
        assert!(
            art.state(&art_key(&first[0])).is_none(),
            "oldest went first"
        );
        assert!(art.state(&art_key(&second[1])).is_some());
    }

    #[test]
    fn a_download_that_lands_after_its_song_left_is_discarded() {
        let mut art = DiscoverArt::default();
        art.show(&[track("Gone")]);
        let (key, _) = art.next_download().unwrap();
        art.show(&[]);
        art.finish(&key, Ok(Some(picture())));
        assert!(art.state(&key).is_none());
        assert!(art.take_retired().is_empty());
        assert!(art.next_download().is_none());
    }
}
