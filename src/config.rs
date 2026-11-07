//! Where Discover's requests go.
//!
//! Noir Player's backend holds the service keys. Someone with keys of their
//! own can enter them in Settings; any service they have a key for is then
//! called directly, and the rest go through the backend.

use std::sync::LazyLock;

use crate::app::store::Store;

/// Noir Player's backend. `NOIR_API_BASE` points a build at another
/// deployment of it; only HTTPS is accepted, like every other request here.
const DEFAULT_BACKEND: &str = "https://noir-player-api.vercel.app";

static BACKEND: LazyLock<String> = LazyLock::new(|| {
    std::env::var("NOIR_API_BASE")
        .ok()
        .map(|base| base.trim().trim_end_matches('/').to_owned())
        .filter(|base| base.starts_with("https://"))
        .unwrap_or_else(|| DEFAULT_BACKEND.to_owned())
});

pub fn backend() -> &'static str {
    &BACKEND
}

/// A developer's `.env`, read once.
static DOTENV: LazyLock<()> = LazyLock::new(|| {
    dotenvy::dotenv().ok();
});

/// The key Discover calls Last.fm with directly. An empty key means charts
/// and search go through the backend instead. (Audio no longer uses any key:
/// it is found and downloaded on this computer, see `ytdlp`.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiKeys {
    pub lastfm: String,
}

impl ApiKeys {
    /// The key entered in Settings, falling back to the environment.
    pub fn from_store(store: &Store) -> Self {
        Self {
            lastfm: key(&store.lastfm_api_key, "LASTFM_API_KEY"),
        }
    }
}

fn key(saved: &str, variable: &str) -> String {
    let saved = saved.trim();
    if !saved.is_empty() {
        return saved.to_owned();
    }
    LazyLock::force(&DOTENV);
    std::env::var(variable)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_entered_in_settings_is_used_as_entered_less_whitespace() {
        let store = Store {
            lastfm_api_key: "  typed-with-spaces  ".into(),
            ..Store::default()
        };
        let keys = ApiKeys::from_store(&store);
        assert_eq!(keys.lastfm, "typed-with-spaces");
    }

    #[test]
    fn the_backend_is_https() {
        assert!(backend().starts_with("https://"));
        assert!(!backend().ends_with('/'));
    }
}
