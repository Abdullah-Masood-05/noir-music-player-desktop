//! Panic reports for `crash.log`, since release builds have no console.

use std::backtrace::Backtrace;
use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};

const MAX_LOG_BYTES: u64 = 256 * 1024;

thread_local! {
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

pub fn install() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if QUIET.try_with(Cell::get).unwrap_or(false) {
            return;
        }
        if let Some(path) = log_path() {
            let _ = append_report(&path, &report(info), MAX_LOG_BYTES);
        }
        previous(info);
    }));
}

/// Runs `f`, turning a panic into `None` without logging it as a crash.
pub fn catch_quietly<R>(f: impl FnOnce() -> R) -> Option<R> {
    let was_quiet = QUIET.try_with(|quiet| quiet.replace(true)).unwrap_or(false);
    let result = panic::catch_unwind(AssertUnwindSafe(f)).ok();
    let _ = QUIET.try_with(|quiet| quiet.set(was_quiet));
    result
}

fn log_path() -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join("noir-player")
            .join("crash.log"),
    )
}

fn report(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(non-text panic payload)");
    let location = info.location().map_or_else(
        || "unknown location".to_owned(),
        |location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        },
    );
    format_report(
        &chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string(),
        message,
        &location,
        &Backtrace::force_capture().to_string(),
    )
}

fn format_report(timestamp: &str, message: &str, location: &str, backtrace: &str) -> String {
    format!(
        "=== {timestamp} Noir Player {} panicked at {location}\n{message}\n{backtrace}\n",
        env!("CARGO_PKG_VERSION"),
    )
}

/// Drops the oldest reports first when the log would outgrow `max` bytes.
fn append_report(path: &Path, report: &str, max: u64) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let report = report.as_bytes();
    let report = &report[..report.len().min(max as usize)];
    let existing = fs::metadata(path).map_or(0, |metadata| metadata.len());
    if existing + report.len() as u64 > max {
        let old = fs::read(path).unwrap_or_default();
        let keep = (max as usize / 2).saturating_sub(report.len());
        let tail = &old[old.len().saturating_sub(keep)..];
        let tail = match tail.iter().position(|&byte| byte == b'\n') {
            Some(newline) if tail.len() < old.len() => &tail[newline + 1..],
            _ => tail,
        };
        fs::write(path, tail)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("noir-crash-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn report_names_message_location_and_version() {
        let text = format_report("2026-01-01", "boom", "src/media.rs:1:2", "frames");
        assert!(text.contains("boom"));
        assert!(text.contains("src/media.rs:1:2"));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
        assert!(text.contains("frames"));
    }

    #[test]
    fn reports_append_into_a_new_directory() {
        let dir = temp_dir();
        let path = dir.join("nested").join("crash.log");
        append_report(&path, "first\n", 1024).unwrap();
        append_report(&path, "second\n", 1024).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first\nsecond\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn log_stays_bounded_and_keeps_the_newest_report() {
        let dir = temp_dir();
        let path = dir.join("crash.log");
        for index in 0..200 {
            append_report(&path, &format!("report {index:03}\n"), 256).unwrap();
            assert!(fs::metadata(&path).unwrap().len() <= 256);
        }
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("report 199\n"));
        assert!(text.lines().all(|line| line.starts_with("report ")));
        append_report(&path, &"x".repeat(1000), 256).unwrap();
        assert!(fs::metadata(&path).unwrap().len() <= 256);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn quiet_catch_returns_none_on_panic_and_restores_state() {
        assert_eq!(catch_quietly(|| 7), Some(7));
        assert_eq!(catch_quietly(|| -> u8 { panic!("expected") }), None);
        assert!(!QUIET.with(Cell::get));
    }
}
