//! Song search through YouTube Music's web search: one HTTPS request whose
//! answer lists songs in the order YouTube Music ranks them. Nothing here
//! starts yt-dlp; it is only used later, to play or download a chosen song.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const ENDPOINT: &str = "https://music.youtube.com/youtubei/v1/search?prettyPrint=false";
/// The "Songs" filter of YouTube Music search.
const SONGS_FILTER: &str = "EgWKAQIIAWoQEAMQBBAJEAoQBRAREBAQFQ%3D%3D";
const CLIENT_VERSION: &str = "1.20241111.01.00";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const TIMEOUT: Duration = Duration::from_secs(8);
const MAX_BODY: u64 = 4 * 1024 * 1024;
const MAX_QUERY_CHARS: usize = 200;
/// How many songs a typed Discover search lists.
const MAX_SONGS: usize = 20;
/// Deeper than any real answer; keeps a hostile one from recursing far.
const MAX_DEPTH: usize = 40;

/// A song found by a Discover search, in YouTube Music's order.
#[derive(Debug, Clone, PartialEq)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub artist: String,
    #[allow(dead_code)] // parsed for completeness; rows show title and artist
    pub album: Option<String>,
    #[allow(dead_code)]
    pub duration: Option<u32>,
    pub thumbnail: Option<String>,
}

/// Ranked songs for a typed search.
pub fn search(query: &str, cancel: &AtomicBool) -> Result<Vec<Song>> {
    crate::api::check_cancel(cancel)?;
    let query: String = query.trim().chars().take(MAX_QUERY_CHARS).collect();
    if query.is_empty() {
        bail!("Nothing to search for.");
    }
    let body = json!({
        "context": {"client": {
            "clientName": "WEB_REMIX",
            "clientVersion": CLIENT_VERSION,
            "hl": "en",
            "gl": "US",
        }},
        "query": query,
        "params": SONGS_FILTER,
    })
    .to_string();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .timeout_global(Some(TIMEOUT))
        .timeout_connect(Some(Duration::from_secs(5)))
        .user_agent(USER_AGENT)
        .build()
        .into();
    let mut response = agent
        .post(ENDPOINT)
        .header("content-type", "application/json")
        .header("origin", "https://music.youtube.com")
        .header("referer", "https://music.youtube.com/")
        .send(body)
        .map_err(|_| anyhow!("Could not reach YouTube Music."))?;
    let text = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|_| anyhow!("Could not read YouTube Music's answer."))?;
    crate::api::check_cancel(cancel)?;
    let root: Value = serde_json::from_str(&text)
        .map_err(|_| anyhow!("YouTube Music sent something unexpected."))?;
    Ok(parse_songs(&root))
}

/// The songs of every shelf in a search answer, in order, without repeats.
fn parse_songs(root: &Value) -> Vec<Song> {
    let mut shelves = Vec::new();
    collect_shelves(root, 0, &mut shelves);
    let mut songs: Vec<Song> = Vec::new();
    for item in shelves
        .into_iter()
        .filter_map(|shelf| shelf.get("contents").and_then(Value::as_array))
        .flatten()
    {
        if songs.len() >= MAX_SONGS {
            break;
        }
        let Some(song) = item
            .get("musicResponsiveListItemRenderer")
            .and_then(parse_item)
        else {
            continue;
        };
        if !songs.iter().any(|s| s.id == song.id) {
            songs.push(song);
        }
    }
    songs
}

fn collect_shelves<'a>(value: &'a Value, depth: usize, out: &mut Vec<&'a Value>) {
    if depth > MAX_DEPTH {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "musicShelfRenderer" {
                    out.push(child);
                } else {
                    collect_shelves(child, depth + 1, out);
                }
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|child| collect_shelves(child, depth + 1, out)),
        _ => {}
    }
}

fn runs(item: &Value, column: usize) -> &[Value] {
    item.pointer(&format!(
        "/flexColumns/{column}/musicResponsiveListItemFlexColumnRenderer/text/runs"
    ))
    .and_then(Value::as_array)
    .map_or(&[], Vec::as_slice)
}

fn text(run: &Value) -> &str {
    run.get("text").and_then(Value::as_str).unwrap_or("").trim()
}

fn page_type(run: &Value) -> Option<&str> {
    run.pointer(
        "/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType",
    )
    .and_then(Value::as_str)
}

/// "3:11" or "1:02:03" in seconds.
fn parse_duration(text: &str) -> Option<u32> {
    let parts: Vec<&str> = text.split(':').collect();
    if !(2..=3).contains(&parts.len())
        || parts
            .iter()
            .any(|p| p.is_empty() || p.len() > 2 || !p.bytes().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    parts
        .iter()
        .try_fold(0u32, |total, p| Some(total * 60 + p.parse::<u32>().ok()?))
}

fn parse_item(item: &Value) -> Option<Song> {
    let id = item
        .pointer("/playlistItemData/videoId")
        .or_else(|| {
            item.pointer(
                "/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId",
            )
        })
        .and_then(Value::as_str)
        .filter(|id| crate::ytdlp::valid_video_id(id))?;
    let title: String = runs(item, 0).iter().map(text).collect::<Vec<_>>().join(" ");
    let title = title.trim().to_owned();
    if title.is_empty() {
        return None;
    }
    let details = runs(item, 1);
    let artists: Vec<&str> = details
        .iter()
        .filter(|run| page_type(run) == Some("MUSIC_PAGE_TYPE_ARTIST"))
        .map(text)
        .filter(|name| !name.is_empty())
        .collect();
    let artist = if artists.is_empty() {
        details
            .iter()
            .map(text)
            .find(|t| !t.is_empty() && *t != "•" && parse_duration(t).is_none())
            .unwrap_or("")
            .to_owned()
    } else {
        artists.join(", ")
    };
    if artist.is_empty() {
        return None;
    }
    Some(Song {
        id: id.to_owned(),
        title,
        artist,
        album: details
            .iter()
            .find(|run| page_type(run) == Some("MUSIC_PAGE_TYPE_ALBUM"))
            .map(|run| text(run).to_owned())
            .filter(|t| !t.is_empty()),
        duration: details.iter().find_map(|run| parse_duration(text(run))),
        thumbnail: Some(thumbnail(item, id)),
    })
}

/// The largest cover that is on an allowed host; Google-hosted covers are
/// square and are asked for at 544 px. Falls back to the video's frame.
fn thumbnail(item: &Value, id: &str) -> String {
    item.pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let url = t.get("url")?.as_str()?;
            let width = t.get("width").and_then(Value::as_u64).unwrap_or(0);
            crate::api::safe_art_url(url).then_some((width, url))
        })
        .max_by_key(|(width, _)| *width)
        .map(|(_, url)| resize_google_cover(url))
        .unwrap_or_else(|| format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"))
}

/// `...=w60-h60-l90-rj` names the size; ask for a larger square.
fn resize_google_cover(url: &str) -> String {
    let google = url.contains("googleusercontent.com/");
    match url.rsplit_once('=') {
        Some((base, size))
            if google
                && size.starts_with('w')
                && size.contains("-h")
                && size.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') =>
        {
            format!("{base}=w544-h544-l90-rj")
        }
        _ => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from a live answer for "enemy": three songs, the first of
    // which has several artists and an album.
    const ANSWER: &str = r###"{"contents":{"tabbedSearchResultsRenderer":{"tabs":[{"tabRenderer":{"content":{"sectionListRenderer":{"contents":[{"musicShelfRenderer":{"title":{"runs":[{"text":"Songs"}]},"contents":[{"musicResponsiveListItemRenderer":{"thumbnail":{"musicThumbnailRenderer":{"thumbnail":{"thumbnails":[{"url":"https://yt3.googleusercontent.com/P_0EuEIJCgW_t7qWL1FFDgNhRgkvDJa4GHvwH1PZvjFUyUPtUrd4RbvmGHMtBsgMvSghJGwpCQlGtcw3=w60-h60-l90-rj","width":60,"height":60},{"url":"https://yt3.googleusercontent.com/P_0EuEIJCgW_t7qWL1FFDgNhRgkvDJa4GHvwH1PZvjFUyUPtUrd4RbvmGHMtBsgMvSghJGwpCQlGtcw3=w120-h120-l90-rj","width":120,"height":120}]}}},"flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Enemy (From the series Arcane League of Legends)","navigationEndpoint":{"watchEndpoint":{"videoId":"vxa8ShIm9yw"}}}]}}},{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Imagine Dragons","navigationEndpoint":{"browseEndpoint":{"browseId":"UC0aXrjVxG5pZr99v77wZdPQ","browseEndpointContextSupportedConfigs":{"browseEndpointContextMusicConfig":{"pageType":"MUSIC_PAGE_TYPE_ARTIST"}}}}},{"text":", "},{"text":"League Of Legends","navigationEndpoint":{"browseEndpoint":{"browseId":"UCNQmUMv2OIDAfgIa1-ih0hA","browseEndpointContextSupportedConfigs":{"browseEndpointContextMusicConfig":{"pageType":"MUSIC_PAGE_TYPE_ARTIST"}}}}},{"text":" & "},{"text":"JID","navigationEndpoint":{"browseEndpoint":{"browseId":"UCRlGNubLJBgW9VRCuiUnuYw","browseEndpointContextSupportedConfigs":{"browseEndpointContextMusicConfig":{"pageType":"MUSIC_PAGE_TYPE_ARTIST"}}}}},{"text":" • "},{"text":"Enemy (from the series Arcane League of Legends)","navigationEndpoint":{"browseEndpoint":{"browseId":"MPREb_x","browseEndpointContextSupportedConfigs":{"browseEndpointContextMusicConfig":{"pageType":"MUSIC_PAGE_TYPE_ALBUM"}}}}},{"text":" • "},{"text":"2:54"}]}}}],"playlistItemData":{"videoId":"vxa8ShIm9yw"}}},{"musicResponsiveListItemRenderer":{"thumbnail":{"musicThumbnailRenderer":{"thumbnail":{"thumbnails":[{"url":"https://lh3.googleusercontent.com/abc=w60-h60-l90-rj","width":60,"height":60}]}}},"flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Enemy"}]}}},{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Tommee Profitt","navigationEndpoint":{"browseEndpoint":{"browseId":"UCx","browseEndpointContextSupportedConfigs":{"browseEndpointContextMusicConfig":{"pageType":"MUSIC_PAGE_TYPE_ARTIST"}}}}},{"text":" • "},{"text":"1:02:03"}]}}}],"overlay":{"musicItemThumbnailOverlayRenderer":{"content":{"musicPlayButtonRenderer":{"playNavigationEndpoint":{"watchEndpoint":{"videoId":"NXCK85F_7Gk"}}}}}}}},{"musicResponsiveListItemRenderer":{"flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Enemies"}]}}},{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Jokhay"},{"text":" • "},{"text":"4:19"}]}}}],"playlistItemData":{"videoId":"Z0pmfGEkvHg"}}}]}}]}}}}]}}}"###;

    #[test]
    fn songs_are_read_in_order_with_artists_album_and_duration() {
        let songs = parse_songs(&serde_json::from_str(ANSWER).unwrap());
        assert_eq!(songs.len(), 3);
        let first = &songs[0];
        assert_eq!(first.id, "vxa8ShIm9yw");
        assert_eq!(
            first.title,
            "Enemy (From the series Arcane League of Legends)"
        );
        assert_eq!(first.artist, "Imagine Dragons, League Of Legends, JID");
        assert_eq!(
            first.album.as_deref(),
            Some("Enemy (from the series Arcane League of Legends)")
        );
        assert_eq!(first.duration, Some(174));
        assert_eq!(
            first.thumbnail.as_deref(),
            Some("https://yt3.googleusercontent.com/P_0EuEIJCgW_t7qWL1FFDgNhRgkvDJa4GHvwH1PZvjFUyUPtUrd4RbvmGHMtBsgMvSghJGwpCQlGtcw3=w544-h544-l90-rj")
        );
        // The id comes from the play button when playlistItemData is absent.
        assert_eq!(songs[1].id, "NXCK85F_7Gk");
        assert_eq!(songs[1].duration, Some(3723));
        assert_eq!(
            songs[1].thumbnail.as_deref(),
            Some("https://lh3.googleusercontent.com/abc=w544-h544-l90-rj")
        );
        // No artist link: the first plain run is the artist; no cover: the frame.
        assert_eq!(songs[2].artist, "Jokhay");
        assert_eq!(
            songs[2].thumbnail.as_deref(),
            Some("https://i.ytimg.com/vi/Z0pmfGEkvHg/mqdefault.jpg")
        );
        assert!(songs
            .iter()
            .all(|s| crate::api::safe_art_url(s.thumbnail.as_deref().unwrap())));
    }

    #[test]
    fn malformed_answers_give_no_songs_and_do_not_panic() {
        for bad in [
            "null",
            "[]",
            "{}",
            r#"{"contents":"x"}"#,
            r#"{"musicShelfRenderer":{"contents":[1,null,{},{"musicResponsiveListItemRenderer":5}]}}"#,
            r#"{"musicShelfRenderer":{"contents":[{"musicResponsiveListItemRenderer":{"flexColumns":[]}}]}}"#,
            r#"{"musicShelfRenderer":{"contents":[{"musicResponsiveListItemRenderer":{"playlistItemData":{"videoId":"--exec=calc"},"flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"T"}]}}},{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"A"}]}}}]}}]}}"#,
            r#"{"musicShelfRenderer":{"contents":[{"musicResponsiveListItemRenderer":{"playlistItemData":{"videoId":"vxa8ShIm9yw"},"flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"  "}]}}}]}}]}}"#,
        ] {
            let root: Value = serde_json::from_str(bad).unwrap();
            assert!(parse_songs(&root).is_empty(), "{bad}");
        }
        let mut deep = Value::Null;
        for _ in 0..100 {
            deep = json!([deep]);
        }
        assert!(parse_songs(&deep).is_empty());
    }

    #[test]
    fn results_are_capped_and_deduplicated() {
        let item = |id: &str| {
            json!({"musicResponsiveListItemRenderer":{
                "playlistItemData":{"videoId":id},
                "flexColumns":[
                    {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"T"}]}}},
                    {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"A"}]}}}]}})
        };
        let mut items = vec![item("aaaaaaaaaaa"), item("aaaaaaaaaaa")];
        items.extend((0..30).map(|i| item(&format!("id{i:09}"))));
        let root = json!({"musicShelfRenderer": {"contents": items}});
        let songs = parse_songs(&root);
        assert_eq!(songs.len(), MAX_SONGS);
        assert_eq!(songs[0].id, "aaaaaaaaaaa");
        assert_eq!(songs[1].id, "id000000000");
    }

    #[test]
    fn durations_and_cover_sizes_are_read_strictly() {
        assert_eq!(parse_duration("2:54"), Some(174));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        for bad in ["", "254", "a:bc", "1:2:3:4", "1:", ":30", "Enemy"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
        assert_eq!(
            resize_google_cover("https://yt3.googleusercontent.com/x=w120-h120-l90-rj"),
            "https://yt3.googleusercontent.com/x=w544-h544-l90-rj"
        );
        assert_eq!(
            resize_google_cover("https://i.ytimg.com/vi/abc/hq720.jpg?a=b"),
            "https://i.ytimg.com/vi/abc/hq720.jpg?a=b"
        );
    }

    #[test]
    #[ignore = "uses the network and YouTube Music"]
    fn live_search_ranks_the_popular_song_first() {
        let started = std::time::Instant::now();
        let songs = search("enemy", &AtomicBool::new(false)).unwrap();
        let elapsed = started.elapsed();
        for (i, song) in songs.iter().enumerate() {
            println!(
                "{}. {} | {} | {} | {:?} | {:?}",
                i + 1,
                song.title,
                song.artist,
                song.id,
                song.duration,
                song.thumbnail
            );
        }
        println!("took {elapsed:?}");
        assert!(songs[0].artist.contains("Imagine Dragons"));
        let cover = crate::api::fetch_art_bytes(songs[0].thumbnail.as_deref().unwrap()).unwrap();
        println!("cover {} bytes", cover.len());
    }
}
