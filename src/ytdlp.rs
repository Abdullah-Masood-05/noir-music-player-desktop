//! Finding a song on YouTube and downloading its audio with yt-dlp, which
//! runs on this computer (see `tools`).
//!
//! yt-dlp is started directly (no shell) with `--ignore-config`, so nothing
//! on the computer changes its behaviour. JavaScript runtimes and FFmpeg
//! found on `PATH` are never used: `--no-js-runtimes` is always passed, QuickJS
//! is only enabled with an explicit path, and `--ffmpeg-location` always names
//! either the app's own FFmpeg or an empty folder.

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::tools;

const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const LOOKUP_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_CANDIDATES: usize = 4;

/// What Discover saves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    /// YouTube's own AAC audio in an M4A file, saved as downloaded.
    #[default]
    Original,
    /// Converted to MP3 (192 kbps) with the app's own FFmpeg.
    Mp3,
}

impl AudioFormat {
    /// Reads a saved choice; anything unrecognised (from a newer build, say)
    /// means the original format rather than rejecting the settings file.
    pub fn lenient<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer).as_deref() {
            Ok("mp3") => AudioFormat::Mp3,
            _ => AudioFormat::Original,
        })
    }
}

/// A YouTube video chosen for a song.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)] // title, artist and duration describe the pick for callers
pub struct Match {
    pub video_id: String,
    pub title: String,
    pub artist: Option<String>,
    pub duration: Option<f64>,
    /// Other acceptable videos, tried if this one turns out unavailable or
    /// to be by a different artist.
    pub alternatives: Vec<String>,
    /// Whether the wider search has not been run yet.
    fallback_pending: bool,
    /// The title and artist asked for.
    query: (String, String),
    key: (String, String),
}

// ---- lookup cache ----

#[derive(Default)]
struct LookupCache {
    map: HashMap<(String, String), (Instant, Match)>,
}

impl LookupCache {
    fn get(&mut self, key: &(String, String), now: Instant) -> Option<Match> {
        match self.map.get(key) {
            Some((at, found)) if now.saturating_duration_since(*at) < LOOKUP_TTL => {
                Some(found.clone())
            }
            Some(_) => {
                self.map.remove(key);
                None
            }
            None => None,
        }
    }

    fn put(&mut self, found: Match, now: Instant) {
        if self.map.len() >= 200 {
            self.map
                .retain(|_, (at, _)| now.saturating_duration_since(*at) < LOOKUP_TTL);
        }
        if self.map.len() >= 200 {
            self.map.clear();
        }
        self.map.insert(found.key.clone(), (now, found));
    }
}

static CACHE: Mutex<Option<LookupCache>> = Mutex::new(None);

fn with_cache<T>(f: impl FnOnce(&mut LookupCache) -> T) -> T {
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(LookupCache::default))
}

fn cache_key(title: &str, artist: &str) -> (String, String) {
    (words(title).join(" "), words(artist).join(" "))
}

// ---- matching ----

fn fold(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
        'ç' | 'ć' | 'č' => 'c',
        'è' | 'é' | 'ê' | 'ë' | 'ē' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
        'ñ' | 'ń' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' => 'u',
        'ý' | 'ÿ' => 'y',
        'š' | 'ś' => 's',
        'ž' | 'ź' | 'ż' => 'z',
        other => other,
    }
}

/// Lowercase words with accents and punctuation removed.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        let c = fold(c);
        if c == '&' {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            out.push("and".to_owned());
        } else if c.is_alphanumeric() {
            current.push(c);
        } else if c == '\'' || c == '’' {
            // "don't" stays one word.
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn contains_run(haystack: &[String], needle: &[String]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Words that mark a different recording or an unrelated upload.
const UNWANTED_WORDS: &[&str] = &[
    "remix",
    "remixed",
    "rmx",
    "cover",
    "covers",
    "live",
    "edit",
    "edits",
    "version",
    "reverb",
    "slowed",
    "karaoke",
    "instrumental",
    "mashup",
    "nightcore",
    "acoustic",
    "unplugged",
    "reprise",
    "tribute",
    "remake",
    "lofi",
    "demo",
    "8d",
    "ringtone",
    "reaction",
    "tutorial",
    "lesson",
    "shorts",
    "happy",
    "diwali",
    "christmas",
    "xmas",
    "holi",
    "navratri",
    "eid",
];

const UNWANTED_PHRASES: &[&str] = &[
    "sped up",
    "speed up",
    "speeded up",
    "slowed down",
    "lo fi",
    "bass boosted",
    "new year",
];

/// Whether a search result's title is the requested song and not an edit,
/// remix, cover or festival version of it (unless that was asked for).
fn title_acceptable(candidate: &str, title: &str, artist: &str) -> bool {
    let candidate = words(candidate);
    let wanted = words(title);
    let by = words(artist);
    if !contains_run(&candidate, &wanted) {
        return false;
    }
    let asked = |word: &str| wanted.iter().chain(by.iter()).any(|w| w == word);
    if candidate
        .iter()
        .any(|w| UNWANTED_WORDS.contains(&w.as_str()) && !asked(w))
    {
        return false;
    }
    UNWANTED_PHRASES.iter().all(|phrase| {
        let phrase = words(phrase);
        !contains_run(&candidate, &phrase)
            || contains_run(&wanted, &phrase)
            || contains_run(&by, &phrase)
    })
}

/// Whether any of `fields` (the video's artist, uploader, channel and title)
/// names the requested artist. No information at all counts as a match.
fn artist_matches(artist: &str, fields: &[&str]) -> bool {
    let wanted = words(artist);
    if wanted.is_empty() || fields.iter().all(|f| f.trim().is_empty()) {
        return true;
    }
    fields.iter().any(|field| {
        let have = words(field);
        contains_run(&have, &wanted) || wanted.iter().all(|w| have.contains(w))
    })
}

// ---- running yt-dlp ----

struct Run {
    success: bool,
    stdout: String,
    stderr: String,
}

#[derive(PartialEq)]
enum Line {
    Keep,
    Stop,
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn kill_tree(child: &mut std::process::Child) {
    // yt-dlp starts a second process of its own; stop both.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run(
    exe: &Path,
    args: &[String],
    timeout: Duration,
    cancel: &AtomicBool,
    on_line: &mut dyn FnMut(&str) -> Line,
) -> Result<Run> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|e| {
        anyhow!("Couldn't start the audio downloader. Security software may be blocking it. ({e})")
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no output pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("no error pipe"))?;
    let (line_tx, line_rx) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buffer).trim_end().to_owned();
                    if line_tx.send(Some(line)).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = line_tx.send(None);
    });
    let errors = Arc::new(Mutex::new(String::new()));
    let (err_done_tx, err_done_rx) = mpsc::channel::<()>();
    {
        let errors = errors.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.read_until(b'\n', &mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(mut text) = errors.lock() {
                            if text.len() < 256 * 1024 {
                                text.push_str(&String::from_utf8_lossy(&buffer));
                            }
                        }
                    }
                }
            }
            let _ = err_done_tx.send(());
        });
    }

    let deadline = Instant::now() + timeout;
    let mut out = String::new();
    let mut stopped_early = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            kill_tree(&mut child);
            return crate::api::check_cancel(cancel).map(|()| unreachable!());
        }
        if Instant::now() >= deadline {
            kill_tree(&mut child);
            bail!(
                "YouTube took too long to answer. Try again in a moment. (timed out after {} s)",
                timeout.as_secs()
            );
        }
        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(line)) => {
                if out.len() < 16 * 1024 * 1024 {
                    out.push_str(&line);
                    out.push('\n');
                }
                if on_line(&line) == Line::Stop {
                    stopped_early = true;
                    kill_tree(&mut child);
                    break;
                }
            }
            Ok(None) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
    // Output has ended; wait for the process itself.
    let status = loop {
        if stopped_early {
            break None;
        }
        if cancel.load(Ordering::Relaxed) {
            kill_tree(&mut child);
            return crate::api::check_cancel(cancel).map(|()| unreachable!());
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                kill_tree(&mut child);
                break None;
            }
        }
    };
    let _ = err_done_rx.recv_timeout(Duration::from_secs(2));
    let stderr = errors.lock().map(|t| t.clone()).unwrap_or_default();
    Ok(Run {
        success: status.is_some_and(|s| s.success()),
        stdout: out,
        stderr,
    })
}

/// Options every yt-dlp run starts with.
fn base_args(js: Option<&Path>) -> Result<Vec<String>> {
    let tools = tools::tools_dir()?;
    // Used only so yt-dlp never finds an FFmpeg of its own.
    let empty = tools.join("no-ffmpeg");
    std::fs::create_dir_all(&empty).ok();
    let mut args: Vec<String> = [
        "--ignore-config",
        "--no-js-runtimes",
        "--no-playlist",
        "--socket-timeout",
        "20",
        "--retries",
        "3",
        "--fragment-retries",
        "3",
    ]
    .map(String::from)
    .to_vec();
    args.push("--cache-dir".into());
    args.push(tools.join("ytdlp-cache").to_string_lossy().into_owned());
    args.push("--ffmpeg-location".into());
    args.push(empty.to_string_lossy().into_owned());
    if let Some(js) = js {
        args.push("--js-runtimes".into());
        args.push(format!("quickjs:{}", js.display()));
    }
    Ok(args)
}

// ---- errors ----

fn error_lines(stderr: &str) -> Vec<&str> {
    let errors: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("ERROR:"))
        .collect();
    if errors.is_empty() {
        stderr
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect()
    } else {
        errors
    }
}

fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        format!("{}...", text.chars().take(max).collect::<String>())
    }
}

/// Plain words for what yt-dlp reported, with a short technical reason on
/// the end for anyone reporting a problem.
pub fn friendly_error(stderr: &str) -> String {
    let lines = error_lines(stderr);
    let text = lines.join("\n").to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    let message = if has(&["confirm your age", "age-restricted", "age restricted"]) {
        "This song's video is age-restricted on YouTube, so it can't be played without signing in. Try another song."
    } else if has(&["not a bot", "recaptcha"]) || (has(&["sign in to confirm"]) && has(&["bot"])) {
        "YouTube asked this connection to prove it isn't a bot. Try again in a while, or switch between Wi-Fi and mobile data."
    } else if has(&[
        "in your country",
        "geo restriction",
        "geo-restrict",
        "blocked it in your country",
    ]) {
        "This song's video isn't available in your country. Try another song."
    } else if has(&["http error 429", "too many requests"]) {
        "YouTube is limiting requests from this connection right now. Try again in a few minutes."
    } else if has(&["http error 403"]) {
        "YouTube refused the request. Try again in a few minutes; if it keeps happening, YouTube may have changed and an app update will fix it."
    } else if has(&[
        "video unavailable",
        "private video",
        "is private",
        "been removed",
        "members-only",
        "members only",
        "premium",
        "this video is not available",
        "no longer available",
        "account associated",
        "copyright",
    ]) {
        "This song's video isn't available on YouTube here. Try another song."
    } else if has(&[
        "getaddrinfo",
        "timed out",
        "name or service not known",
        "name resolution",
        "network is unreachable",
        "connection reset",
        "connection refused",
        "connection aborted",
        "winerror 10060",
        "winerror 10061",
        "winerror 11001",
        "urlopen error",
        "remote end closed",
        "unable to download webpage",
        "sslerror",
        "ssl:",
    ]) {
        "Couldn't reach YouTube. Check your connection and try again."
    } else if has(&["larger than max-filesize", "file is larger than"]) {
        "This song's audio is too large to download (over 100 MiB)."
    } else if has(&[
        "requested format is not available",
        "no video formats found",
        "only images are available",
        "no playable audio",
        "no audio",
    ]) {
        "No playable audio was found for this song."
    } else {
        "Couldn't get this song from YouTube."
    };
    let reason = lines
        .iter()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .or(lines.last())
        .map(|l| {
            let l = l.trim_start_matches("ERROR:").trim();
            // "[youtube] abc123: Video unavailable" -> "Video unavailable"
            let l = l
                .strip_prefix('[')
                .and_then(|rest| rest.split_once("] "))
                .map_or(l, |(_, rest)| rest);
            let l = l
                .split_once(": ")
                .filter(|(id, _)| id.len() == 11)
                .map_or(l, |(_, r)| r);
            short(l, 90)
        })
        .unwrap_or_default();
    if reason.is_empty() || message.starts_with("No ") {
        message.to_owned()
    } else {
        format!("{message} ({reason})")
    }
}

/// Whether a JavaScript runtime could fix this failure.
fn needs_js(stderr: &str) -> bool {
    let text = stderr.to_lowercase();
    [
        "requested format is not available",
        "n challenge",
        "nsig",
        "signature",
        "sign in to confirm",
        "only images are available",
        "no video formats found",
        "http error 403",
    ]
    .iter()
    .any(|n| text.contains(n))
}

fn format_missing(stderr: &str) -> bool {
    let text = stderr.to_lowercase();
    text.contains("requested format is not available") || text.contains("no video formats found")
}

/// Errors about this one video, where another video of the song may work.
fn video_specific(message: &str) -> bool {
    ["isn't available", "No playable audio", "age-restricted"]
        .iter()
        .any(|n| message.contains(n))
}

// ---- search ----

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    id: String,
    title: String,
    artist: Option<String>,
    duration: Option<f64>,
}

fn valid_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn parse_entries(json: &str) -> Result<Vec<Entry>> {
    let start = json.find('{').ok_or_else(|| anyhow!("no search results"))?;
    let root: Value = serde_json::from_str(&json[start..])
        .or_else(|_| serde_json::from_str(json.trim()))
        .map_err(|_| anyhow!("unreadable search results"))?;
    let entries = root
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("no search results"))?;
    Ok(entries
        .iter()
        .filter_map(|e| {
            let id = e.get("id")?.as_str()?;
            let title = e.get("title")?.as_str()?;
            let text = |key: &str| {
                e.get(key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            };
            valid_id(id).then(|| Entry {
                id: id.to_owned(),
                title: title.to_owned(),
                artist: text("artist")
                    .or_else(|| text("uploader"))
                    .or_else(|| text("channel")),
                duration: e.get("duration").and_then(Value::as_f64),
            })
        })
        .collect())
}

fn search(exe: &Path, target: &str, cancel: &AtomicBool, items: &str) -> Result<Vec<Entry>> {
    let mut args = base_args(None)?;
    args.extend(["--flat-playlist", "--playlist-items", items, "-J", target].map(String::from));
    let result = run(exe, &args, SEARCH_TIMEOUT, cancel, &mut |_| Line::Keep)?;
    if !result.success {
        bail!("{}", friendly_error(&result.stderr));
    }
    parse_entries(&result.stdout)
}

/// The acceptable, distinct results for the song, best first.
fn accept_entries(entries: Vec<Entry>, title: &str, artist: &str) -> Vec<Entry> {
    let mut seen: Vec<String> = Vec::new();
    entries
        .into_iter()
        .filter(|e| title_acceptable(&e.title, title, artist))
        .filter(|e| {
            let new = !seen.contains(&e.id);
            seen.push(e.id.clone());
            new
        })
        .take(MAX_CANDIDATES)
        .collect()
}

fn wide_search(exe: &Path, title: &str, artist: &str, cancel: &AtomicBool) -> Result<Vec<Entry>> {
    let query = format!("{title} {artist}");
    search(exe, &format!("ytsearch5:{}", query.trim()), cancel, "1:5")
        .map(|entries| accept_entries(entries, title, artist))
}

/// Picks a YouTube video for the song: the YouTube Music "songs" results
/// first, then an ordinary search. Results are checked against the title so
/// an edit or remix is not taken for the song; `download` later checks the
/// uploader against the artist.
pub fn find(
    title: &str,
    artist: &str,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<Match> {
    crate::api::check_cancel(cancel)?;
    let key = cache_key(title, artist);
    if let Some(found) = with_cache(|c| c.get(&key, Instant::now())) {
        return Ok(found);
    }
    let exe = tools::ensure_ytdlp(progress, cancel)?;
    progress("Finding audio on YouTube...");
    let query = format!("{title} {artist}");
    let music = format!(
        "https://music.youtube.com/search?q={}#songs",
        urlencoding::encode(query.trim())
    );
    let mut first_error = None;
    let mut candidates = match search(&exe, &music, cancel, "1:5") {
        Ok(entries) => accept_entries(entries, title, artist),
        Err(error) => {
            crate::api::check_cancel(cancel)?;
            first_error = Some(error);
            Vec::new()
        }
    };
    let mut fallback_pending = true;
    if candidates.is_empty() {
        progress("Trying a wider search...");
        fallback_pending = false;
        match wide_search(&exe, title, artist, cancel) {
            Ok(entries) => candidates = entries,
            Err(error) => {
                crate::api::check_cancel(cancel)?;
                return Err(first_error.unwrap_or(error));
            }
        }
    }
    let Some(best) = candidates.first().cloned() else {
        bail!("No matching YouTube video was found.");
    };
    let found = Match {
        video_id: best.id,
        title: best.title,
        artist: best.artist,
        duration: best.duration,
        alternatives: candidates.iter().skip(1).map(|e| e.id.clone()).collect(),
        fallback_pending,
        query: (title.to_owned(), artist.to_owned()),
        key,
    };
    with_cache(|c| c.put(found.clone(), Instant::now()));
    Ok(found)
}

// ---- download ----

/// (downloaded, total) from a `PROG` line of the progress template.
fn parse_progress(line: &str) -> Option<(u64, Option<u64>)> {
    let mut parts = line.strip_prefix("PROG ")?.split_whitespace();
    let number = |s: Option<&str>| s.and_then(|s| s.parse::<f64>().ok()).filter(|n| *n >= 0.0);
    let done = number(parts.next())? as u64;
    let total = number(parts.next()).or_else(|| number(parts.next()));
    // An exact total is followed by the estimate; either may be "None".
    Some((done, total.map(|t| t as u64).filter(|t| *t > 0)))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Original,
    ConvertM4a,
    Mp3,
}

enum Outcome {
    Saved(PathBuf),
    WrongArtist,
}

/// Matches the first `META` line of a video against the requested artist.
fn parse_meta(line: &str) -> Option<Vec<String>> {
    Some(
        line.strip_prefix("META\t")?
            .split('\t')
            .map(str::to_owned)
            .collect(),
    )
}

const DISCARD_DUBS: &str = "[format_note!*=?dubbed][format_note!*=?descriptive]";

fn download_args(
    kind: Kind,
    id: &str,
    dest: &Path,
    ffmpeg: Option<&Path>,
    js: Option<&Path>,
) -> Result<Vec<String>> {
    let mut args = base_args(js)?;
    // Later --ffmpeg-location replaces the empty folder from base_args.
    if let Some(ffmpeg) = ffmpeg {
        args.push("--ffmpeg-location".into());
        args.push(ffmpeg.to_string_lossy().into_owned());
    }
    let selector = match kind {
        Kind::Original => format!("bestaudio[ext=m4a]{DISCARD_DUBS}"),
        Kind::ConvertM4a | Kind::Mp3 => format!("bestaudio{DISCARD_DUBS}/bestaudio"),
    };
    let strings = [
        "-f",
        &selector,
        "--no-part",
        "--no-mtime",
        "--max-filesize",
        "100M",
        "--progress",
        "--newline",
        "--progress-template",
        "download:PROG %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s",
        "--no-simulate",
        "--print",
        "before_dl:META\t%(artist|)s\t%(uploader|)s\t%(channel|)s\t%(title|)s",
        "--print",
        "after_move:FILE %(filepath)s",
        "-P",
        &dest.to_string_lossy(),
        "-o",
        "audio.%(ext)s",
    ];
    args.extend(strings.iter().map(|s| (*s).to_owned()));
    match kind {
        Kind::Original => {}
        Kind::ConvertM4a => args
            .extend(["-x", "--audio-format", "m4a", "--audio-quality", "128K"].map(String::from)),
        Kind::Mp3 => args
            .extend(["-x", "--audio-format", "mp3", "--audio-quality", "192K"].map(String::from)),
    }
    args.push(format!("https://www.youtube.com/watch?v={id}"));
    Ok(args)
}

fn clear_dest(dest: &Path) {
    if let Ok(entries) = std::fs::read_dir(dest) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("audio.") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

struct Failure {
    stderr: String,
    cancelled_or_fatal: Option<anyhow::Error>,
}

#[allow(clippy::too_many_arguments)]
fn run_download(
    exe: &Path,
    kind: Kind,
    id: &str,
    dest: &Path,
    ffmpeg: Option<&Path>,
    js: Option<&Path>,
    wanted_artist: Option<&str>,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> std::result::Result<Outcome, Failure> {
    let fatal = |e: anyhow::Error| Failure {
        stderr: String::new(),
        cancelled_or_fatal: Some(e),
    };
    clear_dest(dest);
    let args = download_args(kind, id, dest, ffmpeg, js).map_err(fatal)?;
    let mut file = None;
    let mut wrong_artist = false;
    let result = run(exe, &args, DOWNLOAD_TIMEOUT, cancel, &mut |line| {
        if let Some((done, total)) = parse_progress(line) {
            if kind == Kind::Original || total.is_none_or(|t| done < t) {
                progress(&tools::megabytes("Downloading audio", done, total));
            } else {
                progress("Converting audio...");
            }
        } else if let Some(path) = line.strip_prefix("FILE ") {
            file = Some(PathBuf::from(path.trim()));
        } else if let (Some(fields), Some(artist)) = (parse_meta(line), wanted_artist) {
            let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
            if !artist_matches(artist, &refs) {
                wrong_artist = true;
                return Line::Stop;
            }
        }
        Line::Keep
    })
    .map_err(fatal)?;
    if wrong_artist {
        clear_dest(dest);
        return Ok(Outcome::WrongArtist);
    }
    if !result.success {
        clear_dest(dest);
        return Err(Failure {
            stderr: result.stderr,
            cancelled_or_fatal: None,
        });
    }
    let path = file
        .filter(|p| p.is_file())
        .or_else(|| newest_audio(dest))
        .ok_or_else(|| Failure {
            stderr: "ERROR: no playable audio was produced".into(),
            cancelled_or_fatal: None,
        })?;
    Ok(Outcome::Saved(path))
}

fn newest_audio(dest: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dest)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("audio."))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

#[allow(clippy::too_many_arguments)]
fn download_candidate(
    exe: &Path,
    id: &str,
    dest: &Path,
    format: AudioFormat,
    wanted_artist: Option<&str>,
    force_js: bool,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<Outcome> {
    let mut kind = match format {
        AudioFormat::Original => Kind::Original,
        AudioFormat::Mp3 => Kind::Mp3,
    };
    let mut ffmpeg: Option<PathBuf> = None;
    let mut js: Option<PathBuf> = None;
    if force_js {
        js = Some(tools::ensure_quickjs(progress, cancel)?);
    }
    if kind == Kind::Mp3 {
        ffmpeg = Some(tools::ensure_ffmpeg(progress, cancel)?);
    }
    for _ in 0..4 {
        crate::api::check_cancel(cancel)?;
        let failure = match run_download(
            exe,
            kind,
            id,
            dest,
            ffmpeg.as_deref(),
            js.as_deref(),
            wanted_artist,
            progress,
            cancel,
        ) {
            Ok(outcome) => return Ok(outcome),
            Err(failure) => failure,
        };
        if let Some(error) = failure.cancelled_or_fatal {
            return Err(error);
        }
        crate::api::check_cancel(cancel)?;
        let stderr = failure.stderr;
        if js.is_none() && needs_js(&stderr) {
            progress("Trying again with a script solver...");
            if let Ok(path) = tools::ensure_quickjs(progress, cancel) {
                js = Some(path);
                continue;
            }
            crate::api::check_cancel(cancel)?;
        }
        if kind == Kind::Original && format_missing(&stderr) {
            if let Ok(path) = tools::ensure_ffmpeg(progress, cancel) {
                progress("Converting audio...");
                ffmpeg = Some(path);
                kind = Kind::ConvertM4a;
                continue;
            }
            crate::api::check_cancel(cancel)?;
        }
        bail!("{}", friendly_error(&stderr));
    }
    bail!("Couldn't get this song from YouTube.")
}

/// Downloads the audio of `found` into `dest` (an existing, empty folder) as
/// `audio.<ext>` and returns its path: `.m4a` for `Original`, `.mp3` for
/// `Mp3`.
pub fn download(
    found: &Match,
    dest: &Path,
    format: AudioFormat,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    download_inner(found, dest, format, false, progress, cancel)
}

fn download_inner(
    found: &Match,
    dest: &Path,
    format: AudioFormat,
    force_js: bool,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    crate::api::check_cancel(cancel)?;
    let exe = tools::ensure_ytdlp(progress, cancel)?;
    let (title, artist) = (found.query.0.as_str(), found.query.1.as_str());
    let mut queue: Vec<String> = std::iter::once(found.video_id.clone())
        .chain(found.alternatives.iter().cloned())
        .collect();
    let mut tried: Vec<String> = Vec::new();
    let mut by_someone_else: Vec<String> = Vec::new();
    let mut last_error = None;
    let mut wide_search_pending = found.fallback_pending;
    // Videos whose uploader does not match the artist are skipped while
    // anything else is left to try, so a different song with the same title
    // is not taken for this one.
    loop {
        for id in std::mem::take(&mut queue) {
            if tried.contains(&id) {
                continue;
            }
            tried.push(id.clone());
            match download_candidate(
                &exe,
                &id,
                dest,
                format,
                Some(artist),
                force_js,
                progress,
                cancel,
            ) {
                Ok(Outcome::Saved(path)) => return Ok(path),
                Ok(Outcome::WrongArtist) => by_someone_else.push(id),
                Err(error) => {
                    if cancel.load(Ordering::Relaxed) || !video_specific(&error.to_string()) {
                        forget(found);
                        return Err(error);
                    }
                    last_error = Some(error);
                }
            }
            progress("Trying another upload...");
        }
        if !wide_search_pending {
            break;
        }
        wide_search_pending = false;
        progress("Trying a wider search...");
        match wide_search(&exe, title, artist, cancel) {
            Ok(entries) => queue = entries.into_iter().map(|e| e.id).collect(),
            Err(_) => crate::api::check_cancel(cancel)?,
        }
    }
    // As a last resort, the closest title is taken whoever uploaded it.
    if let Some(id) = by_someone_else.first() {
        match download_candidate(&exe, id, dest, format, None, force_js, progress, cancel)? {
            Outcome::Saved(path) => return Ok(path),
            Outcome::WrongArtist => {}
        }
    }
    forget(found);
    Err(last_error.unwrap_or_else(|| anyhow!("No matching YouTube video was found.")))
}

fn forget(found: &Match) {
    with_cache(|c| {
        c.map.remove(&found.key);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_decorated_titles_of_the_song_are_accepted() {
        assert!(title_acceptable("Numb", "Numb", "Linkin Park"));
        assert!(title_acceptable(
            "Numb (Official Music Video) [4K UPGRADE]",
            "Numb",
            "Linkin Park"
        ));
        assert!(title_acceptable(
            "Kesariya (From \"Brahmastra\")",
            "Kesariya",
            "Arijit Singh"
        ));
        assert!(title_acceptable(
            "Don't Stop Me Now",
            "Don't Stop Me Now",
            "Queen"
        ));
        assert!(title_acceptable("Beyoncé - Halo", "Halo", "Beyonce"));
    }

    #[test]
    fn edits_remixes_and_covers_are_rejected_unless_asked_for() {
        assert!(!title_acceptable(
            "Kesariya Happy Diwali",
            "Kesariya",
            "Arijit Singh"
        ));
        assert!(!title_acceptable(
            "Kesariya (Happy Diwali Edit)",
            "Kesariya",
            "Arijit Singh"
        ));
        for bad in [
            "Numb (Remix)",
            "Numb - Piano Cover",
            "Numb (Live at Wembley)",
            "Numb sped up",
            "Numb (Slowed + Reverb)",
            "Numb Karaoke Version",
            "Numb Instrumental",
            "Numb Lo-Fi",
            "Numb (Acoustic)",
        ] {
            assert!(!title_acceptable(bad, "Numb", "Linkin Park"), "{bad}");
        }
        // Asked for, so allowed.
        assert!(title_acceptable("Numb/Encore", "Numb Encore", "Jay-Z"));
        assert!(title_acceptable("Live Forever", "Live Forever", "Oasis"));
        assert!(title_acceptable("Happy", "Happy", "Pharrell Williams"));
        assert!(title_acceptable(
            "Numb (Remix)",
            "Numb (Remix)",
            "Linkin Park"
        ));
        assert!(!title_acceptable("Different Song", "Numb", "Linkin Park"));
        assert!(!title_acceptable("Numbers", "Numb", "Linkin Park"));
    }

    #[test]
    fn artist_is_checked_against_every_name_field() {
        assert!(artist_matches(
            "Arijit Singh",
            &["Pritam, Arijit Singh", "", ""]
        ));
        assert!(artist_matches(
            "Linkin Park",
            &["", "Linkin Park - Topic", ""]
        ));
        assert!(artist_matches("Beyonce", &["Beyoncé"]));
        assert!(artist_matches("Anyone", &["", ""]));
        assert!(!artist_matches(
            "Linkin Park",
            &["Some Cover Band", "Cover Channel"]
        ));
    }

    #[test]
    fn search_results_are_read() {
        let json = r#"{"entries":[{"id":"kXYiU_JCYtU","title":"Numb","uploader":"Linkin Park","duration":187.0},{"id":"bad","title":"x"},{"id":"abcdefghijk"}]}"#;
        let entries = parse_entries(json).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].artist.as_deref(), Some("Linkin Park"));
        assert_eq!(entries[0].duration, Some(187.0));
        assert!(parse_entries("garbage").is_err());
    }

    #[test]
    fn progress_lines_become_byte_counts() {
        assert_eq!(
            parse_progress("PROG 1258291 3030000 None 3200000.5"),
            Some((1_258_291, Some(3_030_000)))
        );
        assert_eq!(
            parse_progress("PROG 100 None 2900000.0 NA"),
            Some((100, Some(2_900_000)))
        );
        assert_eq!(parse_progress("PROG 100 None None None"), Some((100, None)));
        assert_eq!(parse_progress("PROG None 5 5 1"), None);
        assert_eq!(parse_progress("FILE x"), None);
        assert_eq!(
            tools::megabytes("Downloading audio", 1_258_291, Some(3_145_728)),
            "Downloading audio: 1.2 of 3.0 MB"
        );
    }

    #[test]
    fn meta_lines_are_read() {
        let fields = parse_meta("META\tPritam\tT-Series\t\tKesariya").unwrap();
        assert_eq!(fields, ["Pritam", "T-Series", "", "Kesariya"]);
        assert!(parse_meta("FILE x").is_none());
    }

    #[test]
    fn errors_are_explained_in_plain_words() {
        let cases = [
            (
                "ERROR: [youtube] abcdefghijk: Sign in to confirm you’re not a bot. Use --cookies-from-browser",
                "prove it isn't a bot",
            ),
            (
                "ERROR: [youtube] abcdefghijk: Sign in to confirm your age. This video may be inappropriate",
                "age-restricted",
            ),
            (
                "ERROR: [youtube] abcdefghijk: The uploader has not made this video available in your country",
                "isn't available in your country",
            ),
            (
                "ERROR: [youtube] abcdefghijk: Video unavailable. This video is not available",
                "isn't available on YouTube",
            ),
            (
                "ERROR: [youtube] abcdefghijk: Private video. Sign in if you've been granted access",
                "isn't available on YouTube",
            ),
            (
                "ERROR: unable to download video data: HTTP Error 403: Forbidden",
                "refused the request",
            ),
            (
                "ERROR: unable to download video data: HTTP Error 429: Too Many Requests",
                "limiting requests",
            ),
            (
                "ERROR: [youtube] x: Unable to download webpage: <urlopen error [Errno 11001] getaddrinfo failed>",
                "Couldn't reach YouTube",
            ),
            (
                "ERROR: [youtube] abcdefghijk: Requested format is not available. Use --list-formats",
                "No playable audio",
            ),
            ("ERROR: something odd happened", "Couldn't get this song"),
        ];
        for (stderr, expected) in cases {
            let message = friendly_error(stderr);
            assert!(message.contains(expected), "{stderr} -> {message}");
        }
        // The reason is shortened and left out of "No ..." answers.
        let forbidden =
            friendly_error("ERROR: unable to download video data: HTTP Error 403: Forbidden");
        assert!(
            forbidden.ends_with("HTTP Error 403: Forbidden)"),
            "{forbidden}"
        );
        assert!(!friendly_error("ERROR: Requested format is not available").contains('('));
        // Warnings alone don't read as an error about the video.
        assert!(friendly_error("WARNING: sign in to confirm\nERROR: boom").contains("boom"));
    }

    #[test]
    fn only_failures_a_script_solver_could_fix_trigger_the_retry() {
        assert!(needs_js(
            "ERROR: [youtube] x: Requested format is not available"
        ));
        assert!(needs_js("WARNING: n challenge solving failed"));
        assert!(needs_js("ERROR: Sign in to confirm you’re not a bot"));
        assert!(needs_js(
            "ERROR: unable to download video data: HTTP Error 403: Forbidden"
        ));
        assert!(!needs_js("ERROR: [youtube] x: Video unavailable"));
        assert!(!needs_js("ERROR: getaddrinfo failed"));
        assert!(!needs_js(
            "WARNING: No supported JavaScript runtime could be found"
        ));
    }

    #[test]
    fn lookups_expire_after_thirty_minutes() {
        let key = cache_key("Numb", "Linkin Park");
        assert_eq!(key, cache_key("  numb ", "LINKIN PARK"));
        let found = Match {
            video_id: "kXYiU_JCYtU".into(),
            title: "Numb".into(),
            artist: None,
            duration: None,
            alternatives: vec![],
            fallback_pending: false,
            query: ("Numb".into(), "Linkin Park".into()),
            key: key.clone(),
        };
        let mut cache = LookupCache::default();
        let start = Instant::now();
        cache.put(found.clone(), start);
        assert_eq!(
            cache.get(&key, start + Duration::from_secs(29 * 60)),
            Some(found)
        );
        assert_eq!(cache.get(&key, start + Duration::from_secs(31 * 60)), None);
        assert!(cache.map.is_empty());
    }

    #[test]
    fn saved_format_reads_leniently() {
        #[derive(Deserialize)]
        struct S {
            #[serde(default, deserialize_with = "AudioFormat::lenient")]
            f: AudioFormat,
        }
        let parse = |json: &str| serde_json::from_str::<S>(json).unwrap().f;
        assert_eq!(parse(r#"{"f":"mp3"}"#), AudioFormat::Mp3);
        assert_eq!(parse(r#"{"f":"original"}"#), AudioFormat::Original);
        assert_eq!(parse(r#"{"f":"flac"}"#), AudioFormat::Original);
        assert_eq!(parse(r#"{"f":7}"#), AudioFormat::Original);
        assert_eq!(parse("{}"), AudioFormat::Original);
        assert_eq!(serde_json::to_string(&AudioFormat::Mp3).unwrap(), "\"mp3\"");
    }

    fn live_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("noir-ytdlp-live-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `cargo test live_ytdlp -- --ignored --nocapture`, with NOIR_TOOLS_DIR
    /// set to a scratch folder.
    fn live(title: &str, artist: &str, format: AudioFormat, force_js: bool) {
        assert!(
            std::env::var_os("NOIR_TOOLS_DIR").is_some(),
            "set NOIR_TOOLS_DIR to a scratch folder"
        );
        let cancel = AtomicBool::new(false);
        let say = |m: &str| eprintln!("  {m}");
        let started = Instant::now();
        let found = find(title, artist, &say, &cancel).unwrap();
        eprintln!(
            "{title}: found {} {:?} in {:?}",
            found.video_id,
            found.title,
            started.elapsed()
        );
        let dest = live_dir(&format!("{}-{:?}-{force_js}", found.video_id, format));
        let started = Instant::now();
        let path = download_inner(&found, &dest, format, force_js, &say, &cancel).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        eprintln!(
            "{title}: {} ({size} bytes) in {:?}",
            path.display(),
            started.elapsed()
        );
        assert!(size > 100_000);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    #[ignore = "uses the network and YouTube"]
    fn live_ytdlp_numb_original() {
        live("Numb", "Linkin Park", AudioFormat::Original, false);
    }

    #[test]
    #[ignore = "uses the network and YouTube"]
    fn live_ytdlp_kesariya_is_not_the_diwali_edit() {
        let cancel = AtomicBool::new(false);
        std::env::var_os("NOIR_TOOLS_DIR").expect("set NOIR_TOOLS_DIR");
        let found = find("Kesariya", "Arijit Singh", &|_| {}, &cancel).unwrap();
        eprintln!("picked {:?} {}", found.title, found.video_id);
        assert!(!found.title.to_lowercase().contains("diwali"));
        live("Kesariya", "Arijit Singh", AudioFormat::Original, false);
    }

    #[test]
    #[ignore = "uses the network and YouTube"]
    fn live_ytdlp_numb_mp3() {
        live("Numb", "Linkin Park", AudioFormat::Mp3, false);
    }

    #[test]
    #[ignore = "uses the network and YouTube"]
    fn live_ytdlp_quickjs_retry_path() {
        live("Numb", "Linkin Park", AudioFormat::Original, true);
    }
    #[test]
    #[ignore = "uses the network and YouTube"]
    fn live_ytdlp_cancelling_stops_yt_dlp_quickly() {
        std::env::var_os("NOIR_TOOLS_DIR").expect("set NOIR_TOOLS_DIR");
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(4));
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let result =
            find("Cancel Test Song", "Nobody At All", &|_| {}, &cancel).and_then(|found| {
                let dest = live_dir("cancel");
                download(&found, &dest, AudioFormat::Original, &|_| {}, &cancel)
            });
        stopper.join().unwrap();
        eprintln!(
            "cancelled run ended after {:?}: {result:?}",
            started.elapsed()
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(8));
    }
}
