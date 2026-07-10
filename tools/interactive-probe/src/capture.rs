//! Byte-stream capture: everything the child writes to the terminal,
//! recorded to disk with arrival timing, so a live session can be replayed
//! offline — the input for the virtual-terminal library evaluation and a
//! fixture candidate for later replay tooling.
//!
//! Format (the downstream contract):
//!
//! - `<name>.ndjson` — one line per PTY read chunk:
//!   `{"t_ns": <monotonic ns since spawn>, "data": "<base64 bytes>"}`.
//!   NDJSON over a binary encoding deliberately: captures are debugged by
//!   humans (grep, jq) far more often than they are replayed.
//! - `<name>-meta.json` — CLI version, OS, terminal dimensions, capture
//!   date, and a scenario note, so a capture file is identifiable without
//!   provenance folklore.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct CaptureLine {
    t_ns: u64,
    data: String,
}

#[derive(Debug, PartialEq)]
pub struct CaptureChunk {
    pub t_ns: u64,
    pub bytes: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct CaptureMeta {
    pub cli_version: String,
    pub os: String,
    pub cols: u16,
    pub rows: u16,
    /// UTC calendar date of the capture, `YYYY-MM-DD`.
    pub captured_on: String,
    pub scenario: String,
    pub chunks: u64,
    pub bytes: u64,
}

pub struct CaptureWriter {
    out: BufWriter<File>,
    path: PathBuf,
    t0: Instant,
    chunks: u64,
    bytes: u64,
}

impl CaptureWriter {
    /// `t0` is the child's spawn instant: capture timestamps are meaningful
    /// only relative to it.
    pub fn create(path: &Path, t0: Instant) -> std::io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            out: BufWriter::new(File::create(path)?),
            path: path.to_path_buf(),
            t0,
            chunks: 0,
            bytes: 0,
        })
    }

    pub fn record(&mut self, at: Instant, data: &[u8]) -> std::io::Result<()> {
        let line = CaptureLine {
            // 2^64 ns ≈ 584 years — the cast cannot truncate a real session.
            t_ns: at.saturating_duration_since(self.t0).as_nanos() as u64,
            data: BASE64.encode(data),
        };
        serde_json::to_writer(&mut self.out, &line)?;
        self.out.write_all(b"\n")?;
        self.chunks += 1;
        self.bytes += data.len() as u64;
        Ok(())
    }

    /// Flush the stream and write the `-meta.json` side file. Returns the
    /// capture path for the step log.
    pub fn finish(
        mut self,
        cli_version: &str,
        cols: u16,
        rows: u16,
        captured_on: String,
        scenario: &str,
    ) -> std::io::Result<(PathBuf, u64, u64)> {
        self.out.flush()?;
        let meta = CaptureMeta {
            cli_version: cli_version.to_string(),
            os: std::env::consts::OS.to_string(),
            cols,
            rows,
            captured_on,
            scenario: scenario.to_string(),
            chunks: self.chunks,
            bytes: self.bytes,
        };
        let meta_path = meta_path_for(&self.path);
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;
        Ok((self.path, self.chunks, self.bytes))
    }
}

/// `capture.ndjson` → `capture-meta.json`, next to each other.
pub fn meta_path_for(capture: &Path) -> PathBuf {
    let stem = capture
        .file_stem()
        .map_or_else(|| "capture".into(), |s| s.to_string_lossy().into_owned());
    capture.with_file_name(format!("{stem}-meta.json"))
}

/// Read a capture back into ordered chunks. Any malformed line is an error:
/// a capture that cannot round-trip must fail the tool reading it, not
/// silently shorten the replay.
pub fn read_capture(path: &Path) -> std::io::Result<Vec<CaptureChunk>> {
    let bad = |line_no: usize, why: String| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}:{line_no}: {why}", path.display()),
        )
    };
    let mut chunks = Vec::new();
    for (idx, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: CaptureLine =
            serde_json::from_str(&line).map_err(|err| bad(idx + 1, err.to_string()))?;
        let bytes = BASE64
            .decode(parsed.data.as_bytes())
            .map_err(|err| bad(idx + 1, format!("base64: {err}")))?;
        chunks.push(CaptureChunk {
            t_ns: parsed.t_ns,
            bytes,
        });
    }
    Ok(chunks)
}

/// UTC calendar date for a Unix timestamp, `YYYY-MM-DD` — enough calendar
/// for a capture label without a date dependency. Days-to-civil conversion
/// per Howard Hinnant's algorithm.
pub fn utc_date(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agent-bridge-capture-test-{}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn capture_roundtrip_preserves_bytes_and_pacing() {
        let path = temp_path("roundtrip.ndjson");
        let t0 = Instant::now();
        let mut writer = CaptureWriter::create(&path, t0).unwrap();
        let chunks: &[(u64, &[u8])] = &[
            (1_000, b"\x1b[2J\x1b[1;1H"),
            (2_500_000, "héllo 🌍".as_bytes()),
            (30_000_000_000, &[0x00, 0xFF, 0x03]), // raw bytes, not UTF-8
        ];
        for (t_ns, data) in chunks {
            writer
                .record(t0 + Duration::from_nanos(*t_ns), data)
                .unwrap();
        }
        let (out_path, count, bytes) = writer
            .finish(
                "test-cli 0.0",
                80,
                24,
                "2026-07-10".to_string(),
                "unit test",
            )
            .unwrap();
        assert_eq!(count, 3);
        assert_eq!(
            bytes,
            chunks.iter().map(|(_, d)| d.len() as u64).sum::<u64>()
        );

        let read = read_capture(&out_path).unwrap();
        assert_eq!(read.len(), 3);
        for ((t_ns, data), chunk) in chunks.iter().zip(&read) {
            assert_eq!(chunk.t_ns, *t_ns, "pacing must survive the round-trip");
            assert_eq!(chunk.bytes, *data, "bytes must survive the round-trip");
        }

        let meta: CaptureMeta =
            serde_json::from_str(&std::fs::read_to_string(meta_path_for(&out_path)).unwrap())
                .unwrap();
        assert_eq!(meta.chunks, 3);
        assert_eq!(meta.cols, 80);
        assert_eq!(meta.captured_on, "2026-07-10");

        std::fs::remove_file(&out_path).unwrap();
        std::fs::remove_file(meta_path_for(&out_path)).unwrap();
    }

    #[test]
    fn malformed_capture_lines_are_an_error_not_a_short_replay() {
        let path = temp_path("malformed.ndjson");
        std::fs::write(&path, "{\"t_ns\": 5, \"data\": \"aGk=\"}\nnot json\n").unwrap();
        let err = read_capture(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains(":2:"),
            "must name the bad line: {err}"
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn meta_path_sits_next_to_the_capture() {
        assert_eq!(
            meta_path_for(Path::new("/tmp/x/capture.ndjson")),
            Path::new("/tmp/x/capture-meta.json")
        );
    }

    #[test]
    fn utc_date_converts_known_timestamps() {
        assert_eq!(utc_date(0), "1970-01-01");
        // 2026-07-10 00:00:00 UTC.
        assert_eq!(utc_date(1_783_641_600), "2026-07-10");
        // Leap-year day: 2024-02-29 12:00:00 UTC.
        assert_eq!(utc_date(1_709_208_000), "2024-02-29");
    }
}
