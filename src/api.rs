use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::ApiKeys;

#[derive(Debug, Clone)]
pub struct Track {
    pub name: String,
    pub artist: String,
    pub url: Option<String>,
}

pub fn fetch_trending(keys: &ApiKeys) -> Result<Vec<Track>> {
    request(keys, None)
}

pub fn search_tracks(keys: &ApiKeys, query: &str) -> Result<Vec<Track>> {
    if query.trim().is_empty() {
        return fetch_trending(keys);
    }
    request(keys, Some(query.trim()))
}

fn request(keys: &ApiKeys, query: Option<&str>) -> Result<Vec<Track>> {
    let root = if keys.lastfm.is_empty() {
        backend_json(
            &match query {
                Some(query) => format!("/api/tracks?q={}", urlencoding::encode(query)),
                None => "/api/tracks".to_owned(),
            },
            Duration::from_secs(15),
        )?
    } else {
        lastfm_direct(&keys.lastfm, query)?
    };
    parse_tracks(&root, query.is_some())
}

fn lastfm_direct(key: &str, query: Option<&str>) -> Result<Value> {
    let method = if query.is_some() {
        "track.search"
    } else {
        "chart.gettoptracks"
    };
    let mut url = format!(
        "https://ws.audioscrobbler.com/2.0/?method={method}&api_key={}&format=json&limit=30",
        urlencoding::encode(key)
    );
    if let Some(query) = query {
        url.push_str(&format!("&track={}", urlencoding::encode(query)));
    }
    let agent = network_agent(Duration::from_secs(15));
    let mut response = agent.get(&url).call().map_err(|_| {
        anyhow!("Could not reach Last.fm. Check your connection and API key, then retry.")
    })?;
    if response.status().as_u16() != 200 {
        bail!("Last.fm returned an unsuccessful response.");
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(1_048_576)
        .read_to_string()
        .map_err(|_| anyhow!("Could not read the Last.fm response."))?;
    serde_json::from_str(&body).map_err(|_| anyhow!("Last.fm returned an invalid response."))
}

fn parse_tracks(root: &Value, search: bool) -> Result<Vec<Track>> {
    if root.get("error").is_some() {
        bail!("Last.fm rejected the request. Check your API key or retry later.");
    }
    let entries = root
        .pointer(if search {
            "/results/trackmatches/track"
        } else {
            "/tracks/track"
        })
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Last.fm returned an unexpected track list."))?;
    entries
        .iter()
        .map(|entry| {
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Last.fm returned a track without a title."))?;
            let artist = entry
                .get("artist")
                .and_then(|artist| {
                    artist
                        .as_str()
                        .or_else(|| artist.get("name").and_then(Value::as_str))
                })
                .ok_or_else(|| anyhow!("Last.fm returned a track without an artist."))?;
            Ok(Track {
                name: name.to_owned(),
                artist: artist.to_owned(),
                url: entry
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|url| safe_track_url(url))
                    .map(str::to_owned),
            })
        })
        .collect()
}

pub fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("Audio request cancelled.");
    }
    Ok(())
}

fn network_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(20)))
        .build()
        .into()
}

/// GETs one of Noir Player's backend endpoints. Its errors are written for
/// the person using the app, so they are shown as they come.
fn backend_json(path: &str, timeout: Duration) -> Result<Value> {
    let url = format!("{}{path}", crate::config::backend());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        // The backend explains a failure in the body; read it instead of
        // replacing it with a generic transport error.
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(10)))
        .build()
        .into();
    let mut response = agent.get(&url).call().map_err(|_| {
        anyhow!("Couldn't reach Noir Player's server. Check your connection and try again.")
    })?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(1_048_576)
        .read_to_string()
        .map_err(|_| anyhow!("Couldn't read the answer from Noir Player's server."))?;
    let root: Value = serde_json::from_str(&body)
        .map_err(|_| anyhow!("Noir Player's server sent back something unexpected."))?;
    if status != 200 {
        let message = root
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Noir Player's server couldn't handle that. Try again later.");
        bail!("{message}");
    }
    Ok(root)
}

fn validate_audio_url(url: &str) -> Result<ureq::http::Uri> {
    let invalid = || anyhow!("Audio service returned an unsafe or unsupported HTTPS URL.");
    if url.len() > 8192
        || url
            .bytes()
            .any(|c| c <= 32 || c >= 127 || c == b'\\' || c == b'#')
    {
        return Err(invalid());
    }
    let uri: ureq::http::Uri = url.parse().map_err(|_| invalid())?;
    let authority = uri.authority().ok_or_else(invalid)?;
    let host = uri.host().ok_or_else(invalid)?.to_ascii_lowercase();
    if uri.scheme_str() != Some("https")
        || authority.as_str().contains('@')
        || uri.port_u16().is_some_and(|port| port != 443)
        || host.parse::<std::net::IpAddr>().is_ok()
        || !host.contains('.')
        || host.len() > 253
        || [
            ".localhost",
            ".local",
            ".internal",
            ".lan",
            ".home",
            ".localdomain",
        ]
        .iter()
        .any(|suffix| host.ends_with(suffix))
        || !host
            .rsplit('.')
            .next()
            .is_some_and(|label| label.len() >= 2 && label.bytes().all(|c| c.is_ascii_alphabetic()))
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
    {
        return Err(invalid());
    }
    Ok(uri)
}

pub fn safe_track_url(url: &str) -> bool {
    url.starts_with("https://www.last.fm/music/") || url.starts_with("https://last.fm/music/")
}

/// The image CDNs the backend's `/api/art` answers with.
const ART_HOSTS: [&str; 8] = [
    "lastfm.freetls.fastly.net",
    "cdn-images.dzcdn.net",
    "e-cdns-images.dzcdn.net",
    "is1-ssl.mzstatic.com",
    "is2-ssl.mzstatic.com",
    "is3-ssl.mzstatic.com",
    "is4-ssl.mzstatic.com",
    "is5-ssl.mzstatic.com",
];

/// Thumbnails are a couple of hundred pixels square; anything bigger is not one.
const MAX_ART_BYTES: u64 = 512 * 1024;
/// The larger cover embedded into a downloaded song.
const MAX_COVER_BYTES: u64 = 2 * 1024 * 1024;

/// Whether `url` is an HTTPS image on one of the known cover-art CDNs.
pub fn safe_art_url(url: &str) -> bool {
    let Ok(uri) = validate_audio_url(url) else {
        return false;
    };
    uri.port_u16().is_none()
        && uri
            .host()
            .is_some_and(|host| ART_HOSTS.contains(&host.to_ascii_lowercase().as_str()))
}

/// What Discover's art is cached under: one entry per song, however it is cased.
pub fn art_key(track: &Track) -> String {
    format!(
        "{}\u{1f}{}",
        track.artist.trim().to_lowercase(),
        track.name.trim().to_lowercase()
    )
}

/// The cover art URL for a Discover song, or `None` when no service has one.
/// Always asked of the backend: it combines services no key in Settings covers.
pub fn fetch_art_url(track: &Track) -> Result<Option<String>> {
    let root = backend_json(
        &format!(
            "/api/art?artist={}&track={}",
            urlencoding::encode(track.artist.trim()),
            urlencoding::encode(track.name.trim())
        ),
        Duration::from_secs(10),
    )?;
    parse_art(&root)
}

fn parse_art(root: &Value) -> Result<Option<String>> {
    match root.get("image") {
        Some(Value::Null) => Ok(None),
        Some(Value::String(url)) if safe_art_url(url) => Ok(Some(url.clone())),
        _ => bail!("Noir Player's server sent back unusable cover art."),
    }
}

/// Downloads a cover thumbnail, refusing anything that isn't a small JPEG or PNG.
pub fn fetch_art_bytes(url: &str) -> Result<Vec<u8>> {
    fetch_image(url, MAX_ART_BYTES)
}

/// The cover to embed into a Discover song: the largest size the image
/// service offers within reason, falling back to the thumbnail itself.
/// `None` when the song has no art or none of it could be fetched.
pub fn fetch_cover_art(track: &Track) -> Option<Vec<u8>> {
    let url = fetch_art_url(track).ok()??;
    let large = large_art_url(&url);
    if large != url {
        if let Ok(bytes) = fetch_image(&large, MAX_COVER_BYTES) {
            return Some(bytes);
        }
    }
    fetch_image(&url, MAX_COVER_BYTES).ok()
}

/// iTunes names the size in the file name (`.../200x200bb.jpg`) and serves
/// any size asked for; the other services' URLs are left alone.
pub fn large_art_url(url: &str) -> String {
    let itunes = safe_art_url(url)
        && validate_audio_url(url).is_ok_and(|uri| {
            uri.host()
                .is_some_and(|host| host.to_ascii_lowercase().ends_with(".mzstatic.com"))
                && uri.query().is_none()
        });
    match url.rsplit_once('/') {
        Some((base, file)) if itunes => match file.strip_prefix("200x200bb.") {
            Some(extension) => format!("{base}/600x600bb.{extension}"),
            None => url.to_owned(),
        },
        _ => url.to_owned(),
    }
}

fn fetch_image(url: &str, limit: u64) -> Result<Vec<u8>> {
    if !safe_art_url(url) {
        bail!("Cover art URL is not on a known image host.");
    }
    let mut response = network_agent(Duration::from_secs(10))
        .get(url)
        .call()
        .map_err(|_| anyhow!("Could not download cover art."))?;
    if response.status().as_u16() != 200 {
        bail!("Cover art download was unsuccessful.");
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !art_content_type(content_type) {
        bail!("Cover art was not a JPEG or PNG image.");
    }
    if response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit)
    {
        bail!("Cover art is too large.");
    }
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|_| anyhow!("Could not read cover art."))
}

fn art_content_type(value: &str) -> bool {
    let essence = value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    essence == "image/jpeg" || essence == "image/png"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn audio_urls_require_https_public_dns_names_without_credentials() {
        for url in [
            "http://audio.example.com/song",
            "https://localhost/song",
            "https://a.localhost/song",
            "https://127.0.0.1/song",
            "https://10.0.0.1/song",
            "https://172.16.0.1/song",
            "https://192.168.1.2/song",
            "https://169.254.169.254/song",
            "https://[::1]/song",
            "https://[fc00::1]/song",
            "https://[::ffff:127.0.0.1]/song",
            "https://2130706433/song",
            "https://0x7f000001/song",
            "https://0177.0.0.1/song",
            "https://user@audio.example.com/song",
            "https://audio.example.com:80/song",
            "https://audio.example.com\\@localhost/song",
            "https://audio.local/song",
            "https://audio.example.com./song",
            "https://audio.example.com/song#fragment",
            "https://audio.example.com/song\n",
        ] {
            assert!(validate_audio_url(url).is_err(), "{url}");
        }
        assert!(validate_audio_url("https://audio.example.com/song.mp3?token=abc%20def").is_ok());
        assert!(validate_audio_url("https://audio.example.com:443/song").is_ok());
    }

    #[test]
    fn cancellation_is_checked_without_network() {
        assert!(check_cancel(&AtomicBool::new(true)).is_err());
        assert!(check_cancel(&AtomicBool::new(false)).is_ok());
    }

    #[test]
    fn parses_search_and_chart_artist_shapes() {
        let search = json!({"results":{"trackmatches":{"track":[{"name":"Song", "artist":"Artist", "url":"https://www.last.fm/music/Artist/_/Song"}]}}});
        let chart = json!({"tracks":{"track":[{"name":"Song","artist":{"name":"Artist"}}]}});
        assert_eq!(parse_tracks(&search, true).unwrap()[0].artist, "Artist");
        assert_eq!(parse_tracks(&chart, false).unwrap()[0].name, "Song");
    }

    #[test]
    fn empty_results_stay_empty_and_errors_are_not_samples() {
        assert!(
            parse_tracks(&json!({"results":{"trackmatches":{"track":[]}}}), true)
                .unwrap()
                .is_empty()
        );
        assert!(parse_tracks(&json!({"error":6}), true).is_err());
        assert!(parse_tracks(&json!({}), false).is_err());
    }

    /// Drives the app's own request code against the deployed backend with
    /// no keys of its own, as it runs for anyone who hasn't entered any. Makes a few Last.fm requests,
    /// one YouTube search and one cover art lookup, so it is opt-in:
    /// `cargo test -- --ignored live_backend`.
    #[test]
    #[ignore = "calls the deployed backend and spends service quota"]
    fn live_backend_serves_charts_search_and_art_without_any_keys() {
        let keys = ApiKeys::default();
        let trending = fetch_trending(&keys).expect("trending through the backend");
        assert!(!trending.is_empty());
        let found = search_tracks(&keys, "Numb Linkin Park").expect("search through the backend");
        assert!(found
            .iter()
            .any(|track| track.name.eq_ignore_ascii_case("numb")));
        let numb = Track {
            name: "Numb".into(),
            artist: "Linkin Park".into(),
            url: None,
        };
        let art = fetch_art_url(&numb)
            .expect("art through the backend")
            .expect("Numb has cover art");
        assert!(!fetch_art_bytes(&art).expect("the thumbnail").is_empty());
    }

    #[test]
    fn art_urls_must_be_https_on_the_known_image_hosts() {
        for url in [
            "https://lastfm.freetls.fastly.net/i/u/300x300/abc.png",
            "https://cdn-images.dzcdn.net/images/cover/abc/250x250-000000-80-0-0.jpg",
            "https://e-cdns-images.dzcdn.net/images/cover/abc/250x250.jpg",
            "https://is1-ssl.mzstatic.com/image/thumb/Music/x.jpg/200x200bb.jpg",
            "https://IS5-SSL.mzstatic.com/image/thumb/x/200x200bb.jpg",
        ] {
            assert!(safe_art_url(url), "{url}");
        }
        for url in [
            "http://lastfm.freetls.fastly.net/i/u/abc.png",
            "https://is6-ssl.mzstatic.com/x.jpg",
            "https://mzstatic.com/x.jpg",
            "https://evil.test/is1-ssl.mzstatic.com/x.jpg",
            "https://is1-ssl.mzstatic.com.evil.test/x.jpg",
            "https://user@is1-ssl.mzstatic.com/x.jpg",
            "https://is1-ssl.mzstatic.com:443/x.jpg",
            "https://is1-ssl.mzstatic.com:8443/x.jpg",
            "https://is1-ssl.mzstatic.com/x.jpg#frag",
            "file:///C:/art.jpg",
            "",
        ] {
            assert!(!safe_art_url(url), "{url}");
        }
    }

    #[test]
    fn only_itunes_thumbnails_are_asked_for_a_larger_size() {
        assert_eq!(
            large_art_url("https://is1-ssl.mzstatic.com/image/thumb/Music/x.jpg/200x200bb.jpg"),
            "https://is1-ssl.mzstatic.com/image/thumb/Music/x.jpg/600x600bb.jpg"
        );
        assert_eq!(
            large_art_url("https://IS5-SSL.mzstatic.com/image/thumb/x/200x200bb.png"),
            "https://IS5-SSL.mzstatic.com/image/thumb/x/600x600bb.png"
        );
        for url in [
            "https://lastfm.freetls.fastly.net/i/u/300x300/200x200bb.png",
            "https://cdn-images.dzcdn.net/images/cover/abc/250x250-000000-80-0-0.jpg",
            "https://is1-ssl.mzstatic.com/image/thumb/200x200bb.jpg/100x100bb.jpg",
            "https://evil.test/is1-ssl.mzstatic.com/200x200bb.jpg",
            "http://is1-ssl.mzstatic.com/x/200x200bb.jpg",
        ] {
            assert_eq!(large_art_url(url), url);
        }
        let large =
            large_art_url("https://is3-ssl.mzstatic.com/image/thumb/Music/x.jpg/200x200bb.jpg");
        assert!(safe_art_url(&large), "still on the allowlist");
    }

    #[test]
    fn art_keys_ignore_case_and_keep_artist_and_title_apart() {
        let track = |artist: &str, name: &str| Track {
            name: name.into(),
            artist: artist.into(),
            url: None,
        };
        assert_eq!(
            art_key(&track("Linkin Park", "Numb")),
            art_key(&track(" LINKIN PARK", "numb "))
        );
        assert_eq!(art_key(&track("A", "B")), "a\u{1f}b");
        assert_ne!(art_key(&track("A B", "C")), art_key(&track("A", "B C")));
    }

    #[test]
    fn parses_art_answers_and_refuses_unsafe_urls() {
        assert_eq!(parse_art(&json!({"image": null})).unwrap(), None);
        assert_eq!(
            parse_art(&json!({"image": "https://cdn-images.dzcdn.net/images/cover/a/250x250.jpg"}))
                .unwrap()
                .as_deref(),
            Some("https://cdn-images.dzcdn.net/images/cover/a/250x250.jpg")
        );
        for root in [
            json!({}),
            json!({"image": 42}),
            json!({"image": "https://evil.test/a.jpg"}),
            json!({"image": "http://cdn-images.dzcdn.net/a.jpg"}),
        ] {
            assert!(parse_art(&root).is_err(), "{root}");
        }
    }

    #[test]
    fn only_jpeg_and_png_art_is_accepted() {
        assert!(art_content_type("image/jpeg"));
        assert!(art_content_type("Image/PNG; charset=binary"));
        for value in [
            "",
            "image/webp",
            "text/html",
            "image/svg+xml",
            "image/jpegx",
        ] {
            assert!(!art_content_type(value), "{value}");
        }
    }

    #[test]
    fn only_lastfm_https_track_links_are_opened() {
        for url in [
            "file:///song",
            "javascript:alert(1)",
            "https://www.last.fm.evil.test/music/a",
            "https://www.last.fm@evil.test/music/a",
        ] {
            assert!(!safe_track_url(url));
        }
        assert!(safe_track_url("https://www.last.fm/music/Artist/_/Song"));
    }
}
