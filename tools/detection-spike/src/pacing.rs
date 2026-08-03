//! Replay input loading: `input.bytes` plus its `input.timing.ndjson`
//! sidecar, re-chunked at the recorded PTY-read boundaries.
//!
//! The sidecar carries one `{"offset","monotonic_ns"}` record per read the
//! capture rig performed; `offset` is where that read's bytes start in
//! `input.bytes`. Replay feeds the pipeline those exact slices in order, so
//! any chunk-boundary sensitivity in a pipeline is exercised with the
//! boundaries the real PTY produced, not synthetic ones. The recorded
//! timestamps ride along for pipelines with time-based semantics; the
//! text-matching configuration ignores them, and nothing here sleeps —
//! replay is deterministic and as fast as the pipeline runs.
//!
//! A malformed sidecar is an error naming the file and line, never a short
//! replay: measurement over silently truncated input would be a lie.

use std::fs;
use std::path::Path;

use serde::Deserialize;

#[derive(Deserialize)]
struct TimingRecord {
    offset: u64,
    monotonic_ns: u64,
}

/// One recorded PTY read: its byte range within the stream and the
/// capture-relative instant it arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkBoundary {
    pub offset: usize,
    pub len: usize,
    pub monotonic_ns: u64,
}

/// The replayable byte stream of one fixture.
#[derive(Debug)]
pub struct PacedInput {
    pub bytes: Vec<u8>,
    pub chunks: Vec<ChunkBoundary>,
}

impl PacedInput {
    /// Load `input.bytes` + `input.timing.ndjson` from a fixture directory.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let bytes_path = dir.join("input.bytes");
        let bytes =
            fs::read(&bytes_path).map_err(|err| format!("{}: {err}", bytes_path.display()))?;
        let timing_path = dir.join("input.timing.ndjson");
        let timing = fs::read_to_string(&timing_path)
            .map_err(|err| format!("{}: {err}", timing_path.display()))?;

        let mut records: Vec<TimingRecord> = Vec::new();
        for (index, line) in timing.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let record: TimingRecord = serde_json::from_str(line)
                .map_err(|err| format!("{}:{}: {err}", timing_path.display(), index + 1))?;
            records.push(record);
        }

        Self::from_parts(bytes, &records).map_err(|err| format!("{}: {err}", timing_path.display()))
    }

    fn from_parts(bytes: Vec<u8>, records: &[TimingRecord]) -> Result<Self, String> {
        if records.is_empty() {
            if bytes.is_empty() {
                return Ok(Self {
                    bytes,
                    chunks: Vec::new(),
                });
            }
            return Err(format!("no timing records for {} bytes", bytes.len()));
        }
        if records[0].offset != 0 {
            return Err(format!(
                "first timing record starts at offset {}, not 0",
                records[0].offset
            ));
        }

        let mut chunks = Vec::with_capacity(records.len());
        for (index, record) in records.iter().enumerate() {
            let start = usize::try_from(record.offset)
                .map_err(|_| format!("offset {} does not fit usize", record.offset))?;
            let end = match records.get(index + 1) {
                Some(next) => usize::try_from(next.offset)
                    .map_err(|_| format!("offset {} does not fit usize", next.offset))?,
                None => bytes.len(),
            };
            if end <= start || end > bytes.len() {
                return Err(format!(
                    "timing record {} spans {start}..{end} outside 0..{} or is empty",
                    index + 1,
                    bytes.len()
                ));
            }
            if index > 0 && record.monotonic_ns < records[index - 1].monotonic_ns {
                return Err(format!(
                    "timing record {} goes backwards in time ({} < {})",
                    index + 1,
                    record.monotonic_ns,
                    records[index - 1].monotonic_ns
                ));
            }
            chunks.push(ChunkBoundary {
                offset: start,
                len: end - start,
                monotonic_ns: record.monotonic_ns,
            });
        }
        Ok(Self { bytes, chunks })
    }

    /// The recorded reads, in stream order, as byte slices with timestamps.
    pub fn iter_chunks(&self) -> impl Iterator<Item = (&[u8], u64)> {
        self.chunks.iter().map(|chunk| {
            (
                &self.bytes[chunk.offset..chunk.offset + chunk.len],
                chunk.monotonic_ns,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(offset: u64, monotonic_ns: u64) -> TimingRecord {
        TimingRecord {
            offset,
            monotonic_ns,
        }
    }

    #[test]
    fn replay_pacing_reproduces_chunks() {
        let bytes = b"abcdefghij".to_vec();
        let input =
            PacedInput::from_parts(bytes, &[record(0, 100), record(3, 250), record(7, 900)])
                .expect("valid parts");

        let chunks: Vec<(&[u8], u64)> = input.iter_chunks().collect();
        assert_eq!(
            chunks,
            [(&b"abc"[..], 100), (&b"defg"[..], 250), (&b"hij"[..], 900),]
        );
        let gaps: Vec<u64> = input
            .chunks
            .windows(2)
            .map(|pair| pair[1].monotonic_ns - pair[0].monotonic_ns)
            .collect();
        assert_eq!(gaps, [150, 650]);
    }

    #[test]
    fn reassembled_chunks_equal_the_original_bytes() {
        let bytes = b"the quick brown fox".to_vec();
        let input = PacedInput::from_parts(
            bytes.clone(),
            &[record(0, 1), record(4, 2), record(10, 3), record(16, 4)],
        )
        .expect("valid parts");
        let rebuilt: Vec<u8> = input
            .iter_chunks()
            .flat_map(|(slice, _)| slice.to_vec())
            .collect();
        assert_eq!(rebuilt, bytes);
    }

    #[test]
    fn empty_stream_with_no_records_is_valid() {
        let input = PacedInput::from_parts(Vec::new(), &[]).expect("empty is fine");
        assert_eq!(input.iter_chunks().count(), 0);
    }

    #[test]
    fn bytes_without_records_are_an_error_not_a_short_replay() {
        let err = PacedInput::from_parts(b"data".to_vec(), &[]).unwrap_err();
        assert!(err.contains("no timing records"), "got: {err}");
    }

    #[test]
    fn nonzero_first_offset_is_rejected() {
        let err = PacedInput::from_parts(b"data".to_vec(), &[record(2, 1)]).unwrap_err();
        assert!(err.contains("not 0"), "got: {err}");
    }

    #[test]
    fn out_of_range_and_empty_spans_are_rejected() {
        let err =
            PacedInput::from_parts(b"data".to_vec(), &[record(0, 1), record(9, 2)]).unwrap_err();
        assert!(err.contains("outside"), "got: {err}");
        let err =
            PacedInput::from_parts(b"data".to_vec(), &[record(0, 1), record(0, 2)]).unwrap_err();
        assert!(
            err.contains("outside") || err.contains("empty"),
            "got: {err}"
        );
    }

    #[test]
    fn backwards_time_is_rejected() {
        let err =
            PacedInput::from_parts(b"data".to_vec(), &[record(0, 5), record(2, 4)]).unwrap_err();
        assert!(err.contains("backwards"), "got: {err}");
    }
}
