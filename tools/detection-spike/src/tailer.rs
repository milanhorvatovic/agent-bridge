//! Transcript tailer: the offset contract the runtime's content channel
//! lives by.
//!
//! The rule under test: follow the advertised transcript path, deliver each
//! complete appended line exactly once, **re-open when the file at the path
//! is no longer the file being followed** (a new inode — rotation, or a
//! config-dir move putting a different file at the same name), and
//! **re-read from zero on any size decrease** (a truncate-and-rewrite must
//! never be silently skipped past). A partial trailing line is held until
//! its newline arrives: transcript records land as whole JSONL lines, and
//! delivering half of one would hand the classifier a parse error for a
//! record the session never wrote.
//!
//! The tailer polls; nothing here watches the filesystem. Replay drains
//! committed files in one pass and the unit suite drives every contract
//! edge explicitly, so a wake-up mechanism would be untested machinery.
//!
//! Identity is (device, inode) where the platform exposes it (Unix). Where
//! it does not, replacement is still caught by the size-decrease rule
//! whenever the replacement is shorter than what was consumed; a
//! same-length in-place rewrite is outside the contract on every platform.

use std::fs::{self, File};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::PathBuf;

/// Follows one path. `/clear` advertises a *new* path, so a path switch is
/// a new `Tailer`, not a mutation of this one — which is itself the
/// re-open-on-new-path half of the contract.
pub struct Tailer {
    path: PathBuf,
    /// (device, inode) of the followed file, where the platform exposes it.
    identity: Option<(u64, u64)>,
    /// Bytes of the current file already consumed (delivered plus carried).
    offset: u64,
    /// Partial trailing line held until its newline arrives.
    carry: Vec<u8>,
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

impl Tailer {
    pub fn follow(path: PathBuf) -> Self {
        Self {
            path,
            identity: None,
            offset: 0,
            carry: Vec::new(),
        }
    }

    /// Deliver the complete lines that appeared since the last poll. A path
    /// with no file yet is "not yet" — the follow continues — while any
    /// other I/O failure or a non-UTF-8 line is an error naming the path.
    pub fn poll(&mut self) -> Result<Vec<String>, String> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                // A vanished file ends the current follow: whatever later
                // reappears at the path is a different file, and on
                // platforms without inode identity a longer reappearance
                // would otherwise resume mid-file and silently skip lines.
                self.identity = None;
                self.offset = 0;
                self.carry.clear();
                return Ok(Vec::new());
            }
            Err(err) => return Err(format!("{}: {err}", self.path.display())),
        };

        let identity = file_identity(&metadata);
        let replaced = matches!(
            (self.identity, identity),
            (Some(old), Some(new)) if old != new
        );
        if replaced || metadata.len() < self.offset {
            self.offset = 0;
            self.carry.clear();
        }
        self.identity = identity;

        let mut file =
            File::open(&self.path).map_err(|err| format!("{}: {err}", self.path.display()))?;
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|err| format!("{}: {err}", self.path.display()))?;
        let mut fresh = Vec::new();
        file.read_to_end(&mut fresh)
            .map_err(|err| format!("{}: {err}", self.path.display()))?;
        self.offset += fresh.len() as u64;
        self.carry.extend_from_slice(&fresh);

        // One scan, one drain: everything up to the last newline is the
        // completed share, split in place — re-scanning the carry per line
        // would go quadratic on a whole transcript arriving in one poll.
        let mut lines = Vec::new();
        if let Some(last_newline) = self.carry.iter().rposition(|&byte| byte == b'\n') {
            let completed: Vec<u8> = self.carry.drain(..=last_newline).collect();
            // The buffer ends with the newline just drained, so the final
            // split segment is always the empty tail behind it — dropped,
            // while interior empties are genuine blank lines and stay.
            let mut segments = completed.split(|&byte| byte == b'\n');
            let tail = segments.next_back();
            debug_assert_eq!(tail, Some(&[][..]), "completed share ends with a newline");
            for raw in segments {
                let line = raw.strip_suffix(b"\r").unwrap_or(raw);
                let text = std::str::from_utf8(line)
                    .map_err(|err| format!("{}: non-UTF-8 line: {err}", self.path.display()))?;
                lines.push(text.to_string());
            }
        }
        Ok(lines)
    }

    /// Bytes of a partial trailing line still held. Non-zero after a full
    /// drain means the file ends mid-record.
    pub fn pending(&self) -> usize {
        self.carry.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "detection-spike-tailer-{name}-{}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn append(path: &PathBuf, bytes: &[u8]) {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .expect("open for append");
        file.write_all(bytes).expect("append");
    }

    #[test]
    fn tailer_follows_append() {
        // The `/compact` shape: same path, same inode, the file only grows.
        let dir = TempDir::new("append");
        let path = dir.path("transcript.jsonl");
        append(&path, b"one\ntwo");

        let mut tailer = Tailer::follow(path.clone());
        assert_eq!(tailer.poll().expect("poll"), ["one"]);
        assert_eq!(tailer.pending(), 3, "partial line held, not delivered");

        append(&path, b"\nthree\n");
        assert_eq!(
            tailer.poll().expect("poll"),
            ["two", "three"],
            "append delivers the completed carry and the new line, once each"
        );
        assert_eq!(tailer.pending(), 0);
        assert_eq!(tailer.poll().expect("poll"), Vec::<String>::new());
    }

    #[test]
    fn tailer_rereads_on_size_decrease() {
        let dir = TempDir::new("shrink");
        let path = dir.path("transcript.jsonl");
        append(&path, b"first\nsecond\n");

        let mut tailer = Tailer::follow(path.clone());
        assert_eq!(tailer.poll().expect("poll"), ["first", "second"]);

        fs::write(&path, b"rewritten\n").expect("truncate and rewrite");
        assert_eq!(
            tailer.poll().expect("poll"),
            ["rewritten"],
            "a shorter file re-reads from zero"
        );
    }

    #[test]
    fn tailer_reopens_on_inode_change() {
        // Rotation: a different file replaces the followed one at the same
        // path. The replacement is shorter than what was consumed, so the
        // size-decrease rule catches it even where inodes are not exposed.
        let dir = TempDir::new("rotate");
        let path = dir.path("transcript.jsonl");
        append(&path, b"old-one\nold-two\n");

        let mut tailer = Tailer::follow(path.clone());
        assert_eq!(tailer.poll().expect("poll"), ["old-one", "old-two"]);

        let replacement = dir.path("replacement.jsonl");
        append(&replacement, b"new-one\n");
        fs::rename(&replacement, &path).expect("replace the followed file");
        assert_eq!(
            tailer.poll().expect("poll"),
            ["new-one"],
            "the replacement file is read from zero"
        );
    }

    #[test]
    fn a_recreated_file_is_a_fresh_follow_even_when_longer() {
        // Unlink-then-recreate with a *longer* file: without resetting on
        // the vanish, a platform with no inode identity would resume at
        // the old offset and silently skip the head of the new file.
        let dir = TempDir::new("recreate");
        let path = dir.path("transcript.jsonl");
        append(&path, b"one\ntwo\n");

        let mut tailer = Tailer::follow(path.clone());
        assert_eq!(tailer.poll().expect("poll"), ["one", "two"]);

        fs::remove_file(&path).expect("unlink the followed file");
        assert_eq!(tailer.poll().expect("poll"), Vec::<String>::new());

        append(&path, b"a much longer replacement, first line\nsecond\n");
        assert_eq!(
            tailer.poll().expect("poll"),
            ["a much longer replacement, first line", "second"],
            "the recreated file is read from zero"
        );
    }

    #[test]
    fn a_missing_file_is_not_yet_not_an_error() {
        let dir = TempDir::new("missing");
        let path = dir.path("transcript.jsonl");

        let mut tailer = Tailer::follow(path.clone());
        assert_eq!(tailer.poll().expect("poll"), Vec::<String>::new());

        append(&path, b"arrived\n");
        assert_eq!(tailer.poll().expect("poll"), ["arrived"]);
    }

    #[test]
    fn a_crlf_line_ending_is_stripped() {
        let dir = TempDir::new("crlf");
        let path = dir.path("transcript.jsonl");
        append(&path, b"windows\r\n");

        let mut tailer = Tailer::follow(path);
        assert_eq!(tailer.poll().expect("poll"), ["windows"]);
    }

    #[test]
    fn a_non_utf8_line_is_an_error_naming_the_path() {
        let dir = TempDir::new("utf8");
        let path = dir.path("transcript.jsonl");
        append(&path, b"ok\n\xFF\xFE\n");

        let mut tailer = Tailer::follow(path.clone());
        let err = tailer.poll().expect_err("invalid UTF-8 must error");
        assert!(
            err.contains("transcript.jsonl") && err.contains("UTF-8"),
            "got: {err}"
        );
    }
}
