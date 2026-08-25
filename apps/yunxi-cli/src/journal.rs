//! Small durable turn journal for the standalone CLI host.
//!
//! The journal is deliberately a host concern. It records the input boundary
//! and the resulting host response without making `yunxi-core` depend on a
//! filesystem, a database, or a serialization format. Records are newline
//! delimited JSON so a host can inspect them with ordinary tools and a crash
//! can at worst leave an incomplete tail record.

use crate::HostResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use yunxi_core::ConversationId;

/// Keep the journal bounded by the same order of magnitude as Core message
/// validation while allowing JSON framing overhead.
pub const MAX_JOURNAL_INPUT_BYTES: usize = 32 * 1_024;
const MAX_JOURNAL_RECORD_BYTES: usize = 128 * 1_024;

/// A durable lifecycle record for one CLI turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalRecord {
    TurnStarted {
        sequence: u64,
        conversation_id: ConversationId,
        input: String,
        recorded_at: DateTime<Utc>,
    },
    TurnCompleted {
        sequence: u64,
        response: HostResponse,
        recorded_at: DateTime<Utc>,
    },
    TurnFailed {
        sequence: u64,
        error: String,
        recorded_at: DateTime<Utc>,
    },
}

impl JournalRecord {
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        match self {
            Self::TurnStarted { sequence, .. }
            | Self::TurnCompleted { sequence, .. }
            | Self::TurnFailed { sequence, .. } => *sequence,
        }
    }
}

/// Errors returned while opening or appending to a CLI journal.
#[derive(Debug)]
pub enum JournalError {
    Io(io::Error),
    Encode(serde_json::Error),
    Decode {
        line: usize,
        source: serde_json::Error,
    },
    InvalidRecord {
        line: usize,
        reason: &'static str,
    },
    InputTooLong {
        length: usize,
        maximum: usize,
    },
    SequenceExhausted,
}

impl fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "journal I/O error: {error}"),
            Self::Encode(error) => write!(formatter, "journal encoding error: {error}"),
            Self::Decode { line, source } => {
                write!(
                    formatter,
                    "journal record at line {line} is invalid: {source}"
                )
            }
            Self::InvalidRecord { line, reason } => {
                write!(
                    formatter,
                    "journal record at line {line} is invalid: {reason}"
                )
            }
            Self::InputTooLong { length, maximum } => write!(
                formatter,
                "journal input is {length} bytes, above maximum {maximum}"
            ),
            Self::SequenceExhausted => formatter.write_str("journal sequence exhausted"),
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Encode(error) | Self::Decode { source: error, .. } => Some(error),
            Self::InvalidRecord { .. } | Self::InputTooLong { .. } | Self::SequenceExhausted => {
                None
            }
        }
    }
}

impl From<io::Error> for JournalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// An append-only, synchronously flushed journal.
pub struct CliJournal {
    path: PathBuf,
    file: Mutex<File>,
    next_sequence: Mutex<u64>,
}

impl fmt::Debug for CliJournal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CliJournal")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl CliJournal {
    /// Opens or creates a JSONL journal and resumes after its highest sequence.
    ///
    /// A final record without a newline is treated as a crash-truncated tail;
    /// all complete records remain available. Interior malformed records are
    /// rejected so corruption is never silently hidden.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)?;
        let (records, next_sequence, valid_length) = read_records(&mut file)?;
        drop(records);
        if file.metadata()?.len() > valid_length {
            file.set_len(valid_length)?;
            file.sync_data()?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            path,
            file: Mutex::new(file),
            next_sequence: Mutex::new(next_sequence),
        })
    }

    /// The backing path, useful for host diagnostics.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads all complete records currently present in the journal.
    pub fn records(&self) -> Result<Vec<JournalRecord>, JournalError> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("journal file lock poisoned"))?;
        let (records, _, _) = read_records(&mut file)?;
        file.seek(SeekFrom::End(0))?;
        Ok(records)
    }

    /// Starts a turn before Core is allowed to produce a side effect.
    pub fn start(&self, conversation_id: ConversationId, input: &str) -> Result<u64, JournalError> {
        if input.len() > MAX_JOURNAL_INPUT_BYTES {
            return Err(JournalError::InputTooLong {
                length: input.len(),
                maximum: MAX_JOURNAL_INPUT_BYTES,
            });
        }
        let mut sequence = self
            .next_sequence
            .lock()
            .map_err(|_| io::Error::other("journal sequence lock poisoned"))?;
        let current = *sequence;
        if current == 0 {
            return Err(JournalError::SequenceExhausted);
        }
        *sequence = current
            .checked_add(1)
            .ok_or(JournalError::SequenceExhausted)?;
        drop(sequence);

        self.append(&JournalRecord::TurnStarted {
            sequence: current,
            conversation_id,
            input: input.to_owned(),
            recorded_at: Utc::now(),
        })?;
        Ok(current)
    }

    /// Commits the result of a previously started turn.
    pub fn complete(&self, sequence: u64, response: &HostResponse) -> Result<(), JournalError> {
        self.append(&JournalRecord::TurnCompleted {
            sequence,
            response: response.clone(),
            recorded_at: Utc::now(),
        })
    }

    /// Records a planner, arbiter, or port failure for a started turn.
    pub fn fail(&self, sequence: u64, error: impl Into<String>) -> Result<(), JournalError> {
        self.append(&JournalRecord::TurnFailed {
            sequence,
            error: error.into(),
            recorded_at: Utc::now(),
        })
    }

    fn append(&self, record: &JournalRecord) -> Result<(), JournalError> {
        let encoded = serde_json::to_vec(record).map_err(JournalError::Encode)?;
        if encoded.len() > MAX_JOURNAL_RECORD_BYTES {
            return Err(JournalError::InvalidRecord {
                line: 0,
                reason: "encoded record exceeds maximum size",
            });
        }
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("journal file lock poisoned"))?;
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }
}

fn read_records(file: &mut File) -> Result<(Vec<JournalRecord>, u64, u64), JournalError> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file.try_clone()?);
    let mut records = Vec::new();
    let mut max_sequence = 0_u64;
    let mut line_number = 0_usize;
    let mut valid_length = 0_u64;
    loop {
        let mut line = Vec::new();
        let bytes = Read::by_ref(&mut reader)
            .take((MAX_JOURNAL_RECORD_BYTES + 2) as u64)
            .read_until(b'\n', &mut line)?;
        if bytes == 0 {
            break;
        }
        line_number = line_number.saturating_add(1);
        let complete = line.last() == Some(&b'\n');
        if !complete {
            if bytes > MAX_JOURNAL_RECORD_BYTES {
                return Err(JournalError::InvalidRecord {
                    line: line_number,
                    reason: "record exceeds maximum size",
                });
            }
            break;
        }
        if complete {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        }
        if line.is_empty() {
            valid_length = valid_length
                .checked_add(bytes as u64)
                .ok_or_else(|| io::Error::other("journal file length overflow"))?;
            continue;
        }
        if line.len() > MAX_JOURNAL_RECORD_BYTES {
            return Err(JournalError::InvalidRecord {
                line: line_number,
                reason: "record exceeds maximum size",
            });
        }
        let record = match serde_json::from_slice::<JournalRecord>(&line) {
            Ok(record) => record,
            Err(source) => {
                return Err(JournalError::Decode {
                    line: line_number,
                    source,
                });
            }
        };
        if record.sequence() == 0 {
            return Err(JournalError::InvalidRecord {
                line: line_number,
                reason: "sequence must be positive",
            });
        }
        max_sequence = max_sequence.max(record.sequence());
        records.push(record);
        valid_length = valid_length
            .checked_add(bytes as u64)
            .ok_or_else(|| io::Error::other("journal file length overflow"))?;
    }
    let next_sequence = max_sequence
        .checked_add(1)
        .ok_or(JournalError::SequenceExhausted)?;
    Ok((records, next_sequence, valid_length))
}

#[cfg(test)]
mod tests {
    use super::{CliJournal, JournalError, JournalRecord, MAX_JOURNAL_INPUT_BYTES};
    use crate::HostResponse;
    use std::fs::{OpenOptions, remove_file};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use yunxi_core::ConversationId;

    static TEMPORARY_PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temporary_path() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let sequence = TEMPORARY_PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "yunxi-cli-journal-{}-{nonce}-{sequence}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn temporary_paths_are_unique_across_parallel_tests() {
        let paths = (0..32)
            .map(|_| std::thread::spawn(temporary_path))
            .map(|thread| thread.join().expect("path thread"))
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(paths.len(), 32);
    }

    #[test]
    fn journal_round_trips_turn_lifecycle_and_resumes_sequence() {
        let path = temporary_path();
        let conversation_id = ConversationId::new();
        let journal = CliJournal::open(&path).expect("journal should open");
        let sequence = journal
            .start(conversation_id, "hello")
            .expect("turn should start");
        journal
            .complete(
                sequence,
                &HostResponse::Delivered {
                    message: "hi".to_owned(),
                    external_reference: Some("ref-1".to_owned()),
                },
            )
            .expect("turn should complete");
        drop(journal);

        let reopened = CliJournal::open(&path).expect("journal should resume");
        assert_eq!(
            reopened.start(conversation_id, "next").expect("next turn"),
            sequence + 1
        );
        let records = reopened.records().expect("records should decode");
        assert!(matches!(records[0], JournalRecord::TurnStarted { .. }));
        assert!(matches!(records[1], JournalRecord::TurnCompleted { .. }));
        assert!(matches!(records[2], JournalRecord::TurnStarted { .. }));
        let _ = remove_file(path);
    }

    #[test]
    fn journal_ignores_only_a_crash_truncated_tail() {
        let path = temporary_path();
        let journal = CliJournal::open(&path).expect("journal should open");
        let conversation_id = ConversationId::new();
        journal
            .start(conversation_id, "complete")
            .expect("turn should start");
        drop(journal);

        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("journal file should reopen");
        file.write_all(br#"{"type":"turn_started","sequence":2,"conversation_id":"#)
            .expect("tail should append");
        drop(file);

        let reopened = CliJournal::open(&path).expect("truncated tail is recoverable");
        assert_eq!(reopened.records().expect("records should decode").len(), 1);
        assert_eq!(reopened.start(conversation_id, "after crash").unwrap(), 2);
        drop(reopened);
        let repaired = CliJournal::open(&path).expect("repaired journal should reopen");
        assert_eq!(repaired.records().expect("records should decode").len(), 2);
        let _ = remove_file(path);
    }

    #[test]
    fn journal_rejects_oversized_input_and_interior_corruption() {
        let path = temporary_path();
        let journal = CliJournal::open(&path).expect("journal should open");
        let error = journal
            .start(
                ConversationId::new(),
                &"x".repeat(MAX_JOURNAL_INPUT_BYTES + 1),
            )
            .expect_err("oversized input must be rejected");
        assert!(matches!(error, JournalError::InputTooLong { .. }));
        drop(journal);

        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("journal file should reopen");
        file.write_all(b"not-json\n")
            .expect("corruption should append");
        drop(file);
        assert!(matches!(
            CliJournal::open(&path),
            Err(JournalError::Decode { line: 1, .. })
        ));
        let _ = remove_file(path);
    }
}
