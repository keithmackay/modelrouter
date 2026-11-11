//! A log file that rotates by period and size and keeps a bounded number of
//! rotated files, so the router bounds its own log volume.
//!
//! `tracing-appender`'s rolling appender rotates by time only, so this writer
//! does the rotation and pruning and `tracing-appender` supplies the
//! non-blocking worker thread in front of it.
//!
//! The active file keeps the configured name. A rotation renames it to
//! `<name>.<UTC timestamp>` (`modelrouter.log.20261111T183000.123456Z`), opens
//! a fresh one, and deletes the oldest rotated files beyond `max_files`. Only
//! names in exactly that form are deleted, so other files in the directory
//! are left alone.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::config::schema::LogRotation;

const STAMP_FORMAT: &str = "%Y%m%dT%H%M%S%.6fZ";

type Clock = Box<dyn Fn() -> DateTime<Utc> + Send>;

pub struct RotatingFile {
    path: PathBuf,
    dir: PathBuf,
    base: String,
    rotation: LogRotation,
    max_bytes: u64,
    max_files: usize,
    file: File,
    size: u64,
    period: Option<i64>,
    clock: Clock,
}

impl std::fmt::Debug for RotatingFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RotatingFile")
            .field("path", &self.path)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// Which rotation period `t` falls in: the UTC day or hour number, or none
/// for size-only rotation.
fn period_of(rotation: LogRotation, t: DateTime<Utc>) -> Option<i64> {
    match rotation {
        LogRotation::Daily => Some(t.timestamp().div_euclid(86_400)),
        LogRotation::Hourly => Some(t.timestamp().div_euclid(3_600)),
        LogRotation::Size => None,
    }
}

/// Whether `name` is a file this writer rotated out of `base`.
fn is_rotated_name(base: &str, name: &str) -> bool {
    let Some(rest) = name.strip_prefix(base).and_then(|r| r.strip_prefix('.')) else {
        return false;
    };
    // An optional `-N` follows when two rotations share a timestamp.
    let stamp = match rest.split_once('-') {
        Some((stamp, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => stamp,
        Some(_) => return false,
        None => rest,
    };
    chrono::NaiveDateTime::parse_from_str(stamp, STAMP_FORMAT).is_ok()
}

impl RotatingFile {
    /// Open (or create) the log file at `path`, creating its directory.
    /// A file left over from an earlier period is rotated straight away.
    pub fn open(
        path: &Path,
        rotation: LogRotation,
        max_bytes: u64,
        max_files: usize,
    ) -> io::Result<Self> {
        Self::open_with_clock(path, rotation, max_bytes, max_files, Box::new(Utc::now))
    }

    fn open_with_clock(
        path: &Path,
        rotation: LogRotation,
        max_bytes: u64,
        max_files: usize,
        clock: Clock,
    ) -> io::Result<Self> {
        let base = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "log path names no file"))?
            .to_string();
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        fs::create_dir_all(&dir)?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let meta = file.metadata()?;
        let size = meta.len();
        let now = clock();
        // The period of what is already in the file is the period of its last write.
        let written: DateTime<Utc> = meta.modified().map(DateTime::from).unwrap_or(now);
        let mut this = Self {
            path: path.to_path_buf(),
            dir,
            base,
            rotation,
            max_bytes: max_bytes.max(1),
            max_files: max_files.max(1),
            file,
            size,
            period: period_of(rotation, written),
            clock,
        };
        if size > 0 && this.period != period_of(rotation, now) {
            this.rotate(now)?;
        } else {
            this.period = period_of(rotation, now);
            this.prune()?;
        }
        Ok(this)
    }

    fn rotate(&mut self, now: DateTime<Utc>) -> io::Result<()> {
        self.file.flush()?;
        let stamp = now.format(STAMP_FORMAT).to_string();
        let mut target = self.dir.join(format!("{}.{stamp}", self.base));
        let mut n = 1;
        while target.exists() {
            target = self.dir.join(format!("{}.{stamp}-{n}", self.base));
            n += 1;
        }
        fs::rename(&self.path, &target)?;
        self.file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.size = 0;
        self.period = period_of(self.rotation, now);
        self.prune()
    }

    /// Delete the oldest rotated files beyond `max_files`.
    fn prune(&self) -> io::Result<()> {
        let mut rotated: Vec<String> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| is_rotated_name(&self.base, name))
            .collect();
        if rotated.len() <= self.max_files {
            return Ok(());
        }
        // Timestamps sort by name; a `-N` suffix sorts after the bare stamp.
        rotated.sort();
        let excess = rotated.len() - self.max_files;
        for name in &rotated[..excess] {
            fs::remove_file(self.dir.join(name))?;
        }
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let now = (self.clock)();
        let over_size = self.size + buf.len() as u64 > self.max_bytes;
        let new_period = self.period != period_of(self.rotation, now);
        if self.size > 0 && (over_size || new_period) {
            // A failed rotation must not lose the line: keep writing to the
            // current file and say why on stderr, which is still the console.
            if let Err(e) = self.rotate(now) {
                eprintln!("modelrouter: could not rotate {}: {e}", self.path.display());
            }
        }
        self.file.write_all(buf)?;
        self.size += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::{Arc, Mutex};

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    fn rotated(dir: &Path) -> Vec<String> {
        names(dir).into_iter().filter(|n| is_rotated_name("app.log", n)).collect()
    }

    /// A clock the test moves by hand.
    fn manual_clock(start: DateTime<Utc>) -> (Arc<Mutex<DateTime<Utc>>>, Clock) {
        let t = Arc::new(Mutex::new(start));
        let c = t.clone();
        (t, Box::new(move || *c.lock().unwrap()))
    }

    #[test]
    fn rotated_names_are_recognised_and_nothing_else() {
        assert!(is_rotated_name("app.log", "app.log.20261111T183000.123456Z"));
        assert!(is_rotated_name("app.log", "app.log.20261111T183000.123456Z-2"));
        assert!(!is_rotated_name("app.log", "app.log"));
        assert!(!is_rotated_name("app.log", "app.log.bak"));
        assert!(!is_rotated_name("app.log", "app.log.20261111T183000.123456Z-"));
        assert!(!is_rotated_name("app.log", "app.log.20261111T183000.123456Z-x"));
        assert!(!is_rotated_name("app.log", "other.log.20261111T183000.123456Z"));
        assert!(!is_rotated_name("app.log", "app.logx20261111T183000.123456Z"));
    }

    #[test]
    fn size_rotation_starts_the_next_file_and_prunes_beyond_max_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let start = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        let (t, clock) = manual_clock(start);
        let mut w = RotatingFile::open_with_clock(&path, LogRotation::Size, 10, 2, clock).unwrap();
        // Keep fs::read_dir from seeing two writes share a stamp: advance per line.
        for i in 0..5 {
            *t.lock().unwrap() = start + chrono::Duration::seconds(i);
            w.write_all(b"0123456789").unwrap();
        }
        // Five 10-byte lines at a 10-byte cap: four rotations, two kept.
        assert_eq!(rotated(dir.path()).len(), 2, "{:?}", names(dir.path()));
        assert_eq!(fs::read(&path).unwrap(), b"0123456789");
        // The newest two survive: the rotations at 3s and 4s.
        let kept = rotated(dir.path());
        assert!(kept[0].contains("T120003"), "{kept:?}");
        assert!(kept[1].contains("T120004"), "{kept:?}");
    }

    #[test]
    fn a_line_is_never_split_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let mut w = RotatingFile::open(&path, LogRotation::Size, 8, 5).unwrap();
        w.write_all(b"aaaaa\n").unwrap();
        w.write_all(b"bbbbb\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"bbbbb\n");
        let r = rotated(dir.path());
        assert_eq!(fs::read(dir.path().join(&r[0])).unwrap(), b"aaaaa\n");
    }

    #[test]
    fn a_line_bigger_than_the_cap_still_lands_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let mut w = RotatingFile::open(&path, LogRotation::Size, 4, 5).unwrap();
        w.write_all(b"longer than four\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"longer than four\n");
        assert!(rotated(dir.path()).is_empty());
    }

    #[test]
    fn same_stamp_rotations_get_a_counter_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let (_t, clock) = manual_clock(Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap());
        let mut w = RotatingFile::open_with_clock(&path, LogRotation::Size, 3, 10, clock).unwrap();
        for _ in 0..3 {
            w.write_all(b"abc").unwrap();
        }
        let r = rotated(dir.path());
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(r[1].ends_with("Z-1"), "{r:?}");
    }

    #[test]
    fn daily_rotation_happens_at_the_utc_day_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let start = Utc.with_ymd_and_hms(2026, 11, 11, 23, 59, 0).unwrap();
        let (t, clock) = manual_clock(start);
        let mut w =
            RotatingFile::open_with_clock(&path, LogRotation::Daily, 1 << 20, 7, clock).unwrap();
        w.write_all(b"day one\n").unwrap();
        *t.lock().unwrap() = start + chrono::Duration::seconds(30);
        w.write_all(b"still day one\n").unwrap();
        assert!(rotated(dir.path()).is_empty());
        *t.lock().unwrap() = start + chrono::Duration::seconds(61);
        w.write_all(b"day two\n").unwrap();
        let r = rotated(dir.path());
        assert_eq!(r.len(), 1);
        assert!(r[0].starts_with("app.log.20261112T000001"), "{r:?}");
        assert_eq!(fs::read(&path).unwrap(), b"day two\n");
    }

    #[test]
    fn hourly_rotation_happens_at_the_hour() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let start = Utc.with_ymd_and_hms(2026, 11, 11, 10, 59, 59).unwrap();
        let (t, clock) = manual_clock(start);
        let mut w =
            RotatingFile::open_with_clock(&path, LogRotation::Hourly, 1 << 20, 7, clock).unwrap();
        w.write_all(b"x\n").unwrap();
        *t.lock().unwrap() = start + chrono::Duration::seconds(1);
        w.write_all(b"y\n").unwrap();
        assert_eq!(rotated(dir.path()).len(), 1);
    }

    #[test]
    fn time_modes_also_rotate_at_max_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let mut w = RotatingFile::open(&path, LogRotation::Daily, 4, 7).unwrap();
        w.write_all(b"abcd").unwrap();
        w.write_all(b"efgh").unwrap();
        assert_eq!(rotated(dir.path()).len(), 1);
    }

    #[test]
    fn a_stale_file_from_an_earlier_period_is_rotated_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        fs::write(&path, b"yesterday\n").unwrap();
        // The file's mtime is now; open as if it were two days later.
        let later = Utc::now() + chrono::Duration::days(2);
        let (_t, clock) = manual_clock(later);
        let _w = RotatingFile::open_with_clock(&path, LogRotation::Daily, 1 << 20, 7, clock).unwrap();
        assert_eq!(rotated(dir.path()).len(), 1);
        assert_eq!(fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn reopening_in_the_same_period_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        RotatingFile::open(&path, LogRotation::Daily, 1 << 20, 7).unwrap().write_all(b"a\n").unwrap();
        RotatingFile::open(&path, LogRotation::Daily, 1 << 20, 7).unwrap().write_all(b"b\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a\nb\n");
        assert!(rotated(dir.path()).is_empty());
    }

    #[test]
    fn open_prunes_files_left_beyond_the_cap_and_spares_unrelated_ones() {
        let dir = tempfile::tempdir().unwrap();
        for s in ["20260101T000000.000000Z", "20260102T000000.000000Z", "20260103T000000.000000Z"] {
            fs::write(dir.path().join(format!("app.log.{s}")), b"old").unwrap();
        }
        fs::write(dir.path().join("app.log.bak"), b"keep").unwrap();
        fs::write(dir.path().join("notes.txt"), b"keep").unwrap();
        let _w = RotatingFile::open(&dir.path().join("app.log"), LogRotation::Size, 1 << 20, 1).unwrap();
        assert_eq!(
            names(dir.path()),
            vec!["app.log", "app.log.20260103T000000.000000Z", "app.log.bak", "notes.txt"]
        );
    }

    #[test]
    fn open_creates_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/app.log");
        RotatingFile::open(&path, LogRotation::Daily, 1 << 20, 7).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn a_bare_file_name_uses_the_current_directory() {
        let w = RotatingFile::open_with_clock(
            Path::new("modelrouter-rotating-test-does-not-write.log"),
            LogRotation::Size,
            1 << 20,
            1,
            Box::new(Utc::now),
        );
        let w = w.unwrap();
        assert_eq!(w.dir, PathBuf::from("."));
        assert!(format!("{w:?}").contains("RotatingFile"));
        drop(w);
        fs::remove_file("modelrouter-rotating-test-does-not-write.log").unwrap();
    }

    #[test]
    fn a_path_without_a_file_name_is_refused() {
        let err = RotatingFile::open(Path::new("/"), LogRotation::Daily, 1, 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
