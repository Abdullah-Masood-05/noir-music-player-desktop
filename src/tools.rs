//! The helper programs Discover runs on this computer.
//!
//! Discover finds and downloads audio with yt-dlp, which runs here rather
//! than on a server because YouTube ties each link to the connection that
//! asked for it. yt-dlp is downloaded on first use into the app's data folder
//! and kept current in the background. FFmpeg (only for the MP3 option) and
//! QuickJS (only when YouTube needs a script solved) are fetched the same
//! way. Every download is checked against a published SHA-256 before it can
//! be run, and written beside its final name then renamed, so a half
//! finished file is never executed.
//!
//! `NOIR_TOOLS_DIR` points the tools folder somewhere else (used by tests and
//! for trying a clean install); it is read on every call.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const USER_AGENT: &str = "NoirPlayer-Desktop";
/// Pinned: the minimal LGPL FFmpeg build this app was tested with.
const FFMPEG_TAG: &str = "v1.1.0";
/// Pinned: QuickJS-ng, verified against the digests below.
const QUICKJS_TAG: &str = "v0.18.0";
const MAX_TOOL_BYTES: u64 = 80 * 1024 * 1024;
const MAX_TEXT_BYTES: u64 = 512 * 1024;
const UPDATE_INTERVAL_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    YtDlp,
    Ffmpeg,
    QuickJs,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::YtDlp => "the audio downloader (yt-dlp)",
            Tool::Ffmpeg => "FFmpeg",
            Tool::QuickJs => "QuickJS",
        }
    }

    fn file_name(self) -> &'static str {
        match (self, cfg!(windows)) {
            (Tool::YtDlp, false) => "yt-dlp",
            (Tool::YtDlp, true) => "yt-dlp.exe",
            (Tool::Ffmpeg, false) => "ffmpeg",
            (Tool::Ffmpeg, true) => "ffmpeg.exe",
            (Tool::QuickJs, false) => "qjs",
            (Tool::QuickJs, true) => "qjs.exe",
        }
    }
}

/// One downloadable file of a tool for one platform.
#[derive(Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: &'static str,
    /// Digest built into the app, for a tool whose release has no checksum
    /// file. Otherwise the checksum file of the release is used.
    pub sha256: Option<&'static str>,
}

/// The release file to use for `tool` on `os`/`arch` (the values of
/// `std::env::consts::OS` and `ARCH`).
pub fn asset_for(tool: Tool, os: &str, arch: &str) -> Result<Asset> {
    let unsupported = || match (tool, os, arch) {
        (Tool::Ffmpeg, "macos", "x86_64") => {
            anyhow!("MP3 isn't available on Intel Macs. Choose the original audio format instead.")
        }
        _ => anyhow!(
            "{} isn't available for this computer ({os} {arch}).",
            capitalized(tool.label())
        ),
    };
    let (name, sha256) = match (tool, os, arch) {
        (Tool::YtDlp, "windows", "x86_64") => ("yt-dlp.exe", None),
        (Tool::YtDlp, "windows", "aarch64") => ("yt-dlp_arm64.exe", None),
        (Tool::YtDlp, "macos", _) => ("yt-dlp_macos", None),
        (Tool::YtDlp, "linux", "x86_64") => ("yt-dlp_linux", None),
        (Tool::YtDlp, "linux", "aarch64") => ("yt-dlp_linux_aarch64", None),
        (Tool::Ffmpeg, "windows", "x86_64") => ("ffmpeg-windows-x86_64.exe", None),
        (Tool::Ffmpeg, "macos", "aarch64") => ("ffmpeg-macos-arm64", None),
        (Tool::Ffmpeg, "linux", "x86_64") => ("ffmpeg-linux-x86_64", None),
        (Tool::QuickJs, "windows", "x86_64") => (
            "qjs-windows-x86_64.exe",
            Some("ff03398d74ea6e6bd3a8e58058f7003225abcac229d7d57775b7aaae812a49c7"),
        ),
        (Tool::QuickJs, "macos", "aarch64") => (
            "qjs-darwin-arm64",
            Some("91fe89916d3b0a6c01c4dc8382a99c3c1fd91ecfc214aa81ca051902bcce590d"),
        ),
        (Tool::QuickJs, "linux", "x86_64") => (
            "qjs-linux-x86_64",
            Some("85ded23b9fabe8c1644fc40a8d2bbdd71157affcbd807359c618cc717ee24d7d"),
        ),
        (Tool::QuickJs, "linux", "aarch64") => (
            "qjs-linux-aarch64",
            Some("2c005c7171424a598d31c53c8dc4a0ae2e4958eb6fbb37908c885a307c78187c"),
        ),
        _ => return Err(unsupported()),
    };
    Ok(Asset { name, sha256 })
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn asset_here(tool: Tool) -> Result<Asset> {
    asset_for(tool, std::env::consts::OS, std::env::consts::ARCH)
}

/// Whether MP3 output can work on this computer (FFmpeg has a build for it).
pub fn mp3_supported() -> Result<()> {
    asset_here(Tool::Ffmpeg).map(|_| ())
}

/// Where the tools live.
pub fn tools_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("NOIR_TOOLS_DIR").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::data_local_dir()
        .context("Local application data directory unavailable")?
        .join("noir-player")
        .join("tools"))
}

// ---- checksums ----

/// Reads `sha256sum` style text (`<hex>  <name>` or `<hex> *<name>`).
pub fn parse_checksums(text: &str) -> HashMap<String, String> {
    let mut sums = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(digest), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_hexdigit()) {
            sums.insert(
                name.trim_start_matches('*').to_owned(),
                digest.to_ascii_lowercase(),
            );
        }
    }
    sums
}

// ---- metadata ----

#[derive(Debug, Default, Serialize, Deserialize)]
struct Meta {
    #[serde(default)]
    version: String,
    /// Unix seconds of the last update check (successful or not).
    #[serde(default)]
    checked_at: u64,
    /// Version of a downloaded, verified update waiting to replace the
    /// running copy.
    #[serde(default)]
    pending: Option<String>,
}

static META_LOCK: Mutex<()> = Mutex::new(());
static UPDATING: AtomicBool = AtomicBool::new(false);

fn meta_path(dir: &Path, tool: Tool) -> PathBuf {
    dir.join(format!("{}.json", tool.file_name()))
}

fn read_meta(dir: &Path, tool: Tool) -> Meta {
    std::fs::read(meta_path(dir, tool))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_meta(dir: &Path, tool: Tool, meta: &Meta) -> Result<()> {
    let path = meta_path(dir, tool);
    let temp = temp_name(&path);
    std::fs::write(&temp, serde_json::to_vec_pretty(meta)?)?;
    std::fs::rename(&temp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })?;
    Ok(())
}

fn change_meta(dir: &Path, tool: Tool, change: impl FnOnce(&mut Meta)) {
    let _guard = META_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut meta = read_meta(dir, tool);
    change(&mut meta);
    let _ = write_meta(dir, tool, &meta);
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn temp_name(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

fn staged_path(dir: &Path, tool: Tool) -> PathBuf {
    let mut name = std::ffi::OsString::from(tool.file_name());
    name.push(".new");
    dir.join(name)
}

/// Removes download leftovers from a session that was killed mid-download.
fn clean_leftovers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "tmp") {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(3600));
            if old {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

// ---- network ----

const GITHUB_HOSTS: [&str; 2] = ["github.com", "api.github.com"];

fn trusted_host(url: &str) -> bool {
    let Ok(uri) = url.parse::<ureq::http::Uri>() else {
        return false;
    };
    let Some(host) = uri.host().map(str::to_ascii_lowercase) else {
        return false;
    };
    uri.scheme_str() == Some("https")
        && uri.port_u16().is_none_or(|port| port == 443)
        && uri.authority().is_some_and(|a| !a.as_str().contains('@'))
        && (GITHUB_HOSTS.contains(&host.as_str()) || host.ends_with(".githubusercontent.com"))
}

fn network_error(url: &str, error: &ureq::Error) -> anyhow::Error {
    let host = url
        .parse::<ureq::http::Uri>()
        .ok()
        .and_then(|u| u.host().map(str::to_owned))
        .unwrap_or_default();
    anyhow!("could not reach {host}: {error}")
}

/// GETs `url` (following GitHub's redirects to its own hosts only) and hands
/// the body to `sink` in pieces. `on_progress` gets (received, total).
fn fetch(
    url: &str,
    limit: u64,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(&[u8]) -> Result<()>,
    on_progress: &dyn Fn(u64, Option<u64>),
) -> Result<()> {
    use std::io::Read;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(15 * 60)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(30)))
        .build()
        .into();
    let mut url = url.to_owned();
    for _ in 0..=5 {
        crate::api::check_cancel(cancel)?;
        if !trusted_host(&url) {
            bail!("refused a download from an unexpected address");
        }
        let mut response = agent
            .get(&url)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| network_error(&url, &e))?;
        let status = response.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            url = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("invalid redirect"))?
                .to_owned();
            continue;
        }
        if status != 200 {
            bail!("HTTP {status}");
        }
        let total = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if total.is_some_and(|t| t > limit) {
            bail!("the download is larger than expected");
        }
        let mut reader = response.body_mut().as_reader();
        let mut buffer = [0_u8; 64 * 1024];
        let mut received = 0_u64;
        loop {
            crate::api::check_cancel(cancel)?;
            let count = reader
                .read(&mut buffer)
                .map_err(|e| anyhow!("the download was interrupted: {e}"))?;
            if count == 0 {
                break;
            }
            received += count as u64;
            if received > limit {
                bail!("the download is larger than expected");
            }
            sink(&buffer[..count])?;
            on_progress(received, total);
        }
        if received == 0 || total.is_some_and(|t| t != received) {
            bail!("the download was empty or incomplete");
        }
        return Ok(());
    }
    bail!("too many redirects")
}

fn fetch_text(url: &str, cancel: &AtomicBool) -> Result<String> {
    let mut bytes = Vec::new();
    fetch(
        url,
        MAX_TEXT_BYTES,
        cancel,
        &mut |chunk| {
            bytes.extend_from_slice(chunk);
            Ok(())
        },
        &|_, _| {},
    )?;
    String::from_utf8(bytes).map_err(|_| anyhow!("unreadable text"))
}

/// "Downloading <label>: 1.2 of 3.0 MB".
pub fn megabytes(label: &str, done: u64, total: Option<u64>) -> String {
    let mb = |n: u64| n as f64 / 1_048_576.0;
    match total.filter(|t| *t > 0) {
        Some(total) => format!("{label}: {:.1} of {:.1} MB", mb(done), mb(total)),
        None => format!("{label}: {:.1} MB", mb(done)),
    }
}

/// Downloads `url` to `target`'s temp name, checks its SHA-256 and returns
/// the temp path (executable on unix). The caller renames it into place.
fn download_checked(
    url: &str,
    expected: &str,
    tool: Tool,
    target: &Path,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    let temp = temp_name(target);
    let result = (|| {
        let mut file = std::fs::File::create(&temp)
            .with_context(|| format!("could not write {}", temp.display()))?;
        let mut hasher = Sha256::new();
        let label = format!("Downloading {}", tool.label());
        let last = std::cell::Cell::new(0_u64);
        fetch(
            url,
            MAX_TOOL_BYTES,
            cancel,
            &mut |chunk| {
                hasher.update(chunk);
                file.write_all(chunk)
                    .map_err(|e| anyhow!("could not save the file: {e}"))
            },
            &|done, total| {
                // Roughly every 256 KiB, not every 64 KiB read.
                if done - last.get() >= 256 * 1024 || Some(done) == total {
                    last.set(done);
                    progress(&megabytes(&label, done, total));
                }
            },
        )?;
        file.sync_all().ok();
        drop(file);
        let actual: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if !actual.eq_ignore_ascii_case(expected) {
            bail!("the file did not match its published checksum, so it was discarded");
        }
        make_executable(&temp)?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(temp),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Turns the technical reason a tool could not be set up into a short
/// sentence a person can act on, with the reason last in brackets.
pub fn friendly_tool_error(label: &str, raw: &str) -> String {
    let text = raw.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    let message = if has(&[
        "no space left",
        "os error 112",
        "disk full",
        "not enough space",
    ]) {
        format!("Noir Player couldn't set up {label} because the disk is full. Free some space and try again.")
    } else if has(&["published checksum", "did not match"]) {
        format!("Noir Player downloaded {label} but it didn't pass its safety check, so it was discarded. Try again in a while.")
    } else if has(&[
        "isn't available for this computer",
        "isn't available on intel",
    ]) {
        format!("{} isn't available for this computer.", capitalized(label))
    } else if has(&[
        "tools folder",
        "could not write",
        "could not save",
        "access is denied",
        "permission denied",
        "os error 5)",
        "os error 183",
        "os error 123",
        "read-only",
        "cannot create a file",
        "not a directory",
        "os error 267",
        "os error 3)",
    ]) {
        format!("Noir Player couldn't set up {label} because it can't write to its tools folder.")
    } else if has(&[
        "github did not name",
        "no checksum",
        "http 403",
        "http 429",
        "http 5",
    ]) {
        format!("GitHub isn't giving Noir Player {label} right now. Try again in a few minutes.")
    } else if has(&[
        "could not reach",
        "interrupted",
        "empty or incomplete",
        "larger than expected",
        "redirect",
        "http ",
        "timed out",
    ]) {
        format!("Noir Player couldn't download {label}. Check your connection and try again.")
    } else {
        format!("Noir Player couldn't set up {label}.")
    };
    let reason = raw.trim();
    if reason.is_empty() || message.ends_with("available for this computer.") {
        message
    } else {
        format!("{message} ({reason})")
    }
}

fn download_error(tool: Tool, error: anyhow::Error) -> anyhow::Error {
    if error.to_string().contains("cancelled") {
        return error;
    }
    anyhow!(friendly_tool_error(tool.label(), &format!("{error:#}")))
}

fn create_tools_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| {
        anyhow!(friendly_tool_error(
            "its YouTube downloader",
            &format!("tools folder {}: {e}", dir.display())
        ))
    })
}

// ---- yt-dlp ----

fn ytdlp_url(tag: &str, file: &str) -> String {
    format!("https://github.com/yt-dlp/yt-dlp/releases/download/{tag}/{file}")
}

fn latest_ytdlp_tag(cancel: &AtomicBool) -> Result<String> {
    let text = fetch_text(
        "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest",
        cancel,
    )?;
    let root: serde_json::Value = serde_json::from_str(&text)?;
    let tag = root
        .get("tag_name")
        .and_then(|t| t.as_str())
        .filter(|t| {
            !t.is_empty()
                && t.len() <= 40
                && t.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
        })
        .ok_or_else(|| anyhow!("GitHub did not name the latest release"))?;
    Ok(tag.to_owned())
}

/// Downloads yt-dlp `tag` next to `final_path`, verified, and returns the
/// temp path.
fn fetch_ytdlp(
    tag: &str,
    final_path: &Path,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    let asset = asset_here(Tool::YtDlp)?;
    let sums = fetch_text(&ytdlp_url(tag, "SHA2-256SUMS"), cancel)?;
    let expected = parse_checksums(&sums)
        .remove(asset.name)
        .ok_or_else(|| anyhow!("the release has no checksum for {}", asset.name))?;
    download_checked(
        &ytdlp_url(tag, asset.name),
        &expected,
        Tool::YtDlp,
        final_path,
        progress,
        cancel,
    )
}

/// Swaps in a verified update if one is waiting. Windows refuses to replace a
/// program that is running, in which case it stays staged for the next call.
fn apply_staged(dir: &Path) {
    let staged = staged_path(dir, Tool::YtDlp);
    if !staged.exists() {
        return;
    }
    let _guard = META_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut meta = read_meta(dir, Tool::YtDlp);
    let Some(version) = meta.pending.clone() else {
        let _ = std::fs::remove_file(&staged);
        return;
    };
    if std::fs::rename(&staged, dir.join(Tool::YtDlp.file_name())).is_ok() {
        meta.version = version;
        meta.pending = None;
        let _ = write_meta(dir, Tool::YtDlp, &meta);
    }
}

/// The once-a-day check. Never blocks the caller and never leaves a broken
/// copy: the new file is only moved into place after its checksum passed.
fn spawn_update_if_due(dir: PathBuf) {
    let due =
        now_secs().saturating_sub(read_meta(&dir, Tool::YtDlp).checked_at) >= UPDATE_INTERVAL_SECS;
    if !due || UPDATING.swap(true, Ordering::SeqCst) {
        return;
    }
    change_meta(&dir, Tool::YtDlp, |m| m.checked_at = now_secs());
    let spawned = std::thread::Builder::new()
        .name("ytdlp-update".into())
        .spawn(move || {
            let _ = update_ytdlp(&dir);
            UPDATING.store(false, Ordering::SeqCst);
        });
    if spawned.is_err() {
        UPDATING.store(false, Ordering::SeqCst);
    }
}

fn update_ytdlp(dir: &Path) -> Result<()> {
    let never = AtomicBool::new(false);
    let tag = latest_ytdlp_tag(&never)?;
    let meta = read_meta(dir, Tool::YtDlp);
    if meta.version == tag || meta.pending.as_deref() == Some(tag.as_str()) {
        return Ok(());
    }
    let staged = staged_path(dir, Tool::YtDlp);
    let temp = fetch_ytdlp(&tag, &staged, &|_| {}, &never)?;
    let guard = META_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(error) = std::fs::rename(&temp, &staged) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    let mut meta = read_meta(dir, Tool::YtDlp);
    meta.pending = Some(tag);
    write_meta(dir, Tool::YtDlp, &meta)?;
    drop(guard);
    apply_staged(dir);
    Ok(())
}

/// The path of yt-dlp, downloading it first when this is the first use.
pub fn ensure_ytdlp(progress: &dyn Fn(&str), cancel: &AtomicBool) -> Result<PathBuf> {
    let dir = tools_dir()?;
    create_tools_dir(&dir)?;
    clean_leftovers(&dir);
    apply_staged(&dir);
    let path = dir.join(Tool::YtDlp.file_name());
    if path.is_file() {
        spawn_update_if_due(dir);
        return Ok(path);
    }
    asset_here(Tool::YtDlp)?;
    static FIRST_INSTALL: Mutex<()> = Mutex::new(());
    let _guard = FIRST_INSTALL.lock().unwrap_or_else(|e| e.into_inner());
    if path.is_file() {
        return Ok(path);
    }
    progress("Getting the YouTube downloader (about 24 MB, first time only)...");
    let install = || -> Result<()> {
        let tag = latest_ytdlp_tag(cancel)?;
        let temp = fetch_ytdlp(&tag, &path, progress, cancel)?;
        std::fs::rename(&temp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })?;
        change_meta(&dir, Tool::YtDlp, |m| {
            *m = Meta {
                version: tag,
                checked_at: now_secs(),
                pending: None,
            }
        });
        Ok(())
    };
    install().map_err(|e| download_error(Tool::YtDlp, e))?;
    Ok(path)
}

// ---- pinned tools ----

fn ensure_pinned(
    tool: Tool,
    tag: &str,
    url: impl Fn(&str) -> String,
    sums_file: Option<&str>,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    let asset = asset_here(tool)?;
    let dir = tools_dir()?;
    create_tools_dir(&dir)?;
    let path = dir.join(tool.file_name());
    static INSTALL: Mutex<()> = Mutex::new(());
    let _guard = INSTALL.lock().unwrap_or_else(|e| e.into_inner());
    let current = path.is_file() && read_meta(&dir, tool).version == tag;
    if current {
        return Ok(path);
    }
    progress(&format!("Setting up {} (first time only)...", tool.label()));
    let install = || -> Result<()> {
        let expected = match (asset.sha256, sums_file) {
            (Some(digest), _) => digest.to_owned(),
            (None, Some(sums_file)) => parse_checksums(&fetch_text(&url(sums_file), cancel)?)
                .remove(asset.name)
                .ok_or_else(|| anyhow!("the release has no checksum for {}", asset.name))?,
            (None, None) => bail!("no checksum is known for {}", asset.name),
        };
        let temp = download_checked(&url(asset.name), &expected, tool, &path, progress, cancel)?;
        if let Err(error) = std::fs::rename(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            return Err(error.into());
        }
        change_meta(&dir, tool, |m| {
            *m = Meta {
                version: tag.to_owned(),
                checked_at: now_secs(),
                pending: None,
            }
        });
        Ok(())
    };
    match install() {
        Ok(()) => Ok(path),
        // An older copy that is still running can't be replaced on Windows;
        // it works, so keep using it.
        Err(_) if path.is_file() => Ok(path),
        Err(error) => Err(download_error(tool, error)),
    }
}

/// The path of the minimal FFmpeg, downloading it first when needed. Only
/// the MP3 option (and converting audio that isn't M4A) needs it.
pub fn ensure_ffmpeg(progress: &dyn Fn(&str), cancel: &AtomicBool) -> Result<PathBuf> {
    mp3_supported()?;
    ensure_pinned(
        Tool::Ffmpeg,
        FFMPEG_TAG,
        |file| {
            format!(
            "https://github.com/Abdullah-Masood-05/ffmpeg-ytdl-minimal-build/releases/download/{FFMPEG_TAG}/{file}"
        )
        },
        Some("SHA256SUMS.txt"),
        progress,
        cancel,
    )
}

/// The path of QuickJS, downloading it first when needed. yt-dlp uses it to
/// solve the scripts YouTube sometimes asks for.
pub fn ensure_quickjs(progress: &dyn Fn(&str), cancel: &AtomicBool) -> Result<PathBuf> {
    ensure_pinned(
        Tool::QuickJs,
        QUICKJS_TAG,
        |file| {
            format!("https://github.com/quickjs-ng/quickjs/releases/download/{QUICKJS_TAG}/{file}")
        },
        None,
        progress,
        cancel,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_errors_are_plain_language() {
        let f = |raw: &str| friendly_tool_error("the YouTube downloader", raw);
        let m = f(
            r"tools folder C:.txt	ools: Cannot create a file when that file already exists. (os error 183)",
        );
        assert!(m.starts_with("Noir Player couldn't set up the YouTube downloader because it can't write to its tools folder."));
        assert!(m.ends_with("(os error 183))"));
        assert!(f("could not write x: Access is denied. (os error 5)").contains("can't write"));
        assert!(f(
            "could not save the file: There is not enough space on the disk. (os error 112)"
        )
        .contains("disk is full"));
        assert!(
            f("the file did not match its published checksum, so it was discarded")
                .contains("safety check")
        );
        assert!(f("could not reach github.com: timed out").contains("Check your connection"));
        assert!(f("HTTP 503").contains("GitHub isn't giving"));
        assert!(f("GitHub did not name the latest release").contains("GitHub isn't giving"));
        assert!(
            f("YouTube downloader isn't available for this computer (x y).")
                .starts_with("The YouTube downloader isn't available")
        );
        assert!(f("weird").starts_with("Noir Player couldn't set up"));
    }

    #[test]
    fn assets_are_chosen_per_platform() {
        let name = |tool, os, arch| asset_for(tool, os, arch).map(|a| a.name);
        assert_eq!(
            name(Tool::YtDlp, "windows", "x86_64").unwrap(),
            "yt-dlp.exe"
        );
        assert_eq!(
            name(Tool::YtDlp, "macos", "aarch64").unwrap(),
            "yt-dlp_macos"
        );
        assert_eq!(
            name(Tool::YtDlp, "macos", "x86_64").unwrap(),
            "yt-dlp_macos"
        );
        assert_eq!(
            name(Tool::YtDlp, "linux", "x86_64").unwrap(),
            "yt-dlp_linux"
        );
        assert_eq!(
            name(Tool::YtDlp, "linux", "aarch64").unwrap(),
            "yt-dlp_linux_aarch64"
        );
        assert!(name(Tool::YtDlp, "freebsd", "x86_64").is_err());
        assert!(name(Tool::YtDlp, "linux", "riscv64").is_err());
        assert_eq!(
            name(Tool::Ffmpeg, "windows", "x86_64").unwrap(),
            "ffmpeg-windows-x86_64.exe"
        );
        assert_eq!(
            name(Tool::Ffmpeg, "macos", "aarch64").unwrap(),
            "ffmpeg-macos-arm64"
        );
        let intel = name(Tool::Ffmpeg, "macos", "x86_64").unwrap_err();
        assert!(intel.to_string().contains("Intel Macs"), "{intel}");
        assert_eq!(
            name(Tool::QuickJs, "macos", "aarch64").unwrap(),
            "qjs-darwin-arm64"
        );
    }

    #[test]
    fn quickjs_assets_carry_a_digest_and_others_do_not() {
        for (os, arch) in [
            ("windows", "x86_64"),
            ("macos", "aarch64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
        ] {
            let digest = asset_for(Tool::QuickJs, os, arch).unwrap().sha256.unwrap();
            assert_eq!(digest.len(), 64);
            assert!(digest.bytes().all(|c| c.is_ascii_hexdigit()));
        }
        assert!(asset_for(Tool::YtDlp, "linux", "x86_64")
            .unwrap()
            .sha256
            .is_none());
    }

    #[test]
    fn checksum_files_are_parsed_by_exact_name() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let text = format!("{a}  yt-dlp\n{b} *yt-dlp.exe\nnot a line\n{a}  yt-dlp_macos.zip\n");
        let sums = parse_checksums(&text);
        assert_eq!(sums["yt-dlp"], a);
        assert_eq!(sums["yt-dlp.exe"], "b".repeat(64));
        assert!(!sums.contains_key("yt-dlp_macos"));
        assert_eq!(sums.len(), 3);
    }

    #[test]
    fn downloads_only_come_from_github() {
        assert!(trusted_host("https://github.com/yt-dlp/yt-dlp/releases/x"));
        assert!(trusted_host(
            "https://release-assets.githubusercontent.com/abc?x=1"
        ));
        assert!(trusted_host("https://objects.githubusercontent.com/abc"));
        assert!(!trusted_host("http://github.com/x"));
        assert!(!trusted_host("https://evilgithub.com/x"));
        assert!(!trusted_host("https://github.com.evil.example/x"));
        assert!(!trusted_host("https://user@github.com/x"));
        assert!(!trusted_host("https://github.com:8443/x"));
    }

    #[test]
    fn progress_text_is_in_megabytes() {
        assert_eq!(
            megabytes("Downloading", 1_258_291, Some(3_145_728)),
            "Downloading: 1.2 of 3.0 MB"
        );
        assert_eq!(
            megabytes("Downloading", 524_288, None),
            "Downloading: 0.5 MB"
        );
    }

    #[test]
    #[ignore = "downloads yt-dlp, FFmpeg and QuickJS from GitHub"]
    fn live_tools_install_with_checksums() {
        let dir = std::env::temp_dir().join(format!("noir-tools-live-{}", std::process::id()));
        let before = std::env::var_os("NOIR_TOOLS_DIR");
        std::env::set_var("NOIR_TOOLS_DIR", &dir);
        let cancel = AtomicBool::new(false);
        let say = |m: &str| eprintln!("{m}");
        let started = std::time::Instant::now();
        let ytdlp = ensure_ytdlp(&say, &cancel).unwrap();
        eprintln!("yt-dlp installed in {:?}", started.elapsed());
        assert!(ytdlp.is_file());
        let started = std::time::Instant::now();
        let ffmpeg = ensure_ffmpeg(&say, &cancel).unwrap();
        let qjs = ensure_quickjs(&say, &cancel).unwrap();
        eprintln!("ffmpeg + qjs installed in {:?}", started.elapsed());
        assert!(ffmpeg.is_file() && qjs.is_file());
        // A second call is instant and downloads nothing.
        let started = std::time::Instant::now();
        ensure_ytdlp(&|m| panic!("unexpected download: {m}"), &cancel).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(&dir);
        match before {
            Some(value) => std::env::set_var("NOIR_TOOLS_DIR", value),
            None => std::env::remove_var("NOIR_TOOLS_DIR"),
        }
    }
}
