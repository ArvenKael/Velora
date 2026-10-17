use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::codec::{crc32, decode_record, encode_record, frame};
use crate::record::{Record, Sequence};

/// Bytes in a frame header: `[u32 length][u32 crc32]`.
const FRAME_HEADER_LEN: u64 = 8;

/// The largest record this store will write or believe, in bytes.
///
/// This is what makes recovery decidable. A crash during an append can leave a
/// partial frame behind, and a partial frame cannot be told from a corrupted
/// one by looking at it. What separates them is knowability: this writer never
/// appends a record above this size, so a frame *declaring* a larger length
/// cannot be a torn append, and is reported as corruption rather than being
/// silently discarded along with everything after it.
pub const MAX_RECORD_BYTES: u64 = 256 * 1024 * 1024;

/// What a store had to discard while recovering its history.
///
/// Recovery is not silent: a torn final frame is dropped, and the fact that it
/// was dropped is reported here so the caller can log it rather than discover
/// the loss later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoveryReport {
    discarded_tail_bytes: u64,
}

impl RecoveryReport {
    /// Bytes dropped from the end of the file as a torn final frame.
    pub const fn discarded_tail_bytes(self) -> u64 {
        self.discarded_tail_bytes
    }

    /// Whether recovery found the history completely intact.
    pub const fn is_clean(self) -> bool {
        self.discarded_tail_bytes == 0
    }
}

impl fmt::Display for RecoveryReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_clean() {
            f.write_str("clean")
        } else {
            write!(f, "discarded {} bytes of torn tail", self.discarded_tail_bytes)
        }
    }
}

/// Failure from a [`Store`].
#[derive(Debug)]
pub enum StoreError {
    /// Underlying I/O failure.
    Io(std::io::Error),
    /// The durable history is inconsistent in a way that is not a torn tail.
    Corruption(String),
    /// The backend refused, for a reason of its own.
    ///
    /// A store that keeps history in a database has failures a file does not —
    /// a locked table, a schema from a newer build, a query that will not run.
    /// Flattening those into an I/O error would lose the only part worth
    /// reading.
    Backend(String),
}

impl StoreError {
    pub(crate) fn corruption(reason: impl Into<String>) -> Self {
        StoreError::Corruption(reason.into())
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(err) => write!(f, "store i/o error: {err}"),
            StoreError::Corruption(reason) => write!(f, "store corruption: {reason}"),
            StoreError::Backend(reason) => write!(f, "storage backend refused: {reason}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Io(err) => Some(err),
            StoreError::Corruption(_) | StoreError::Backend(_) => None,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(err: std::io::Error) -> Self {
        StoreError::Io(err)
    }
}

/// Durable, ordered storage for records.
///
/// Recovery is replay: [`Store::read`] returns the whole history in order,
/// tolerating and discarding a torn *final* write so a crash mid-append can
/// never corrupt replay. Corruption anywhere else is an error, never a
/// truncation.
///
/// [`Store::read`] takes `&self` and is safe to call concurrently: it uses
/// positioned reads and never touches the file's shared cursor.
pub trait Store {
    /// Reads every recovered record, ascending by sequence.
    fn read(&self) -> Result<Vec<Record>, StoreError>;

    /// Durably appends one record. Sequences strictly increase.
    fn append(&mut self, record: &Record) -> Result<(), StoreError>;

    /// How many records are held.
    ///
    /// The default reads everything, which is honest and expensive: a store that
    /// can answer from an index should, because this is asked on paths where
    /// materialising a long history would be the entire cost. This is the first
    /// place the choice of storage stops being invisible.
    fn len(&self) -> Result<u64, StoreError> {
        Ok(self.read()?.len() as u64)
    }

    /// Whether there is no history yet.
    fn is_empty(&self) -> Result<bool, StoreError> {
        Ok(self.len()? == 0)
    }

    /// The record at a sequence, if it is held.
    fn read_record(&self, sequence: Sequence) -> Result<Option<Record>, StoreError> {
        Ok(self
            .read()?
            .into_iter()
            .find(|record| record.sequence() == sequence))
    }

    /// Records from `from` inclusive to `to` exclusive.
    fn read_range(&self, from: Sequence, to: Sequence) -> Result<Vec<Record>, StoreError> {
        Ok(self
            .read()?
            .into_iter()
            .filter(|record| record.sequence() >= from && record.sequence() < to)
            .collect())
    }

    /// Whether the history runs unbroken from [`Sequence::FIRST`].
    ///
    /// Recovery asks this on every start, which makes it the most valuable
    /// question to be able to answer without reading everything: for a store
    /// that counts and indexes, it is a query rather than a scan.
    fn is_contiguous(&self) -> Result<bool, StoreError> {
        let mut expected = Sequence::FIRST;
        for record in self.read()? {
            if record.sequence() != expected {
                return Ok(false);
            }
            expected = expected.next();
        }
        Ok(true)
    }

    /// What opening this store had to discard, if the backend can discard
    /// anything.
    ///
    /// `None` is not "clean" — it is "this backend cannot tear". A database
    /// commits a record or it does not, so there is no tail to lose and nothing
    /// to report; a file can be interrupted mid-write, so it always answers, and
    /// answers honestly about a history that was truncated. The runtime prints
    /// whichever of those it got rather than assuming the reassuring one.
    fn recovery(&self) -> Option<RecoveryReport> {
        None
    }
}

/// A non-durable store, for tests and for rebuilt projections.
///
/// Counting and asking whether it is empty come from [`Store`], so that a store
/// has one answer to "how much history" rather than two that disagree about
/// their return type.
#[derive(Debug, Default, Clone)]
pub struct MemoryStore {
    records: Vec<Record>,
}

impl MemoryStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        MemoryStore::default()
    }

}

impl Store for MemoryStore {
    fn read(&self) -> Result<Vec<Record>, StoreError> {
        Ok(self.records.clone())
    }

    fn append(&mut self, record: &Record) -> Result<(), StoreError> {
        self.records.push(record.clone());
        Ok(())
    }
}

/// An append-only file store.
///
/// Records are written as big-endian, CRC-guarded frames. Opening the store
/// recovers the history and truncates a torn final frame left by a crash, so a
/// partially written append can never corrupt replay; anything else that fails
/// validation is reported as [`StoreError::Corruption`] and left on disk
/// untouched for inspection.
///
/// The log owns the sequence and writes through `&mut self`, so the store is
/// single-writer by construction; one process must own the file.
#[derive(Debug)]
pub struct FileStore {
    path: PathBuf,
    file: File,
    recovery: RecoveryReport,
}

impl FileStore {
    /// Opens (creating if needed) the store at `path`, recovering its history.
    ///
    /// # Errors
    ///
    /// Fails on I/O errors, or if the history is inconsistent in a way that a
    /// crash mid-append cannot explain.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        let scan = scan(&file)?;
        let total_len = file.metadata()?.len();

        let mut recovery = RecoveryReport::default();
        if scan.valid_len < total_len {
            recovery.discarded_tail_bytes = total_len - scan.valid_len;
            file.set_len(scan.valid_len)?;
            file.sync_all()?;
        }

        Ok(FileStore {
            path,
            file,
            recovery,
        })
    }

    /// The path this store is backed by.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Store for FileStore {
    fn read(&self) -> Result<Vec<Record>, StoreError> {
        Ok(scan(&self.file)?.records)
    }

    fn append(&mut self, record: &Record) -> Result<(), StoreError> {
        let bytes = encode_record(record);
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(StoreError::corruption(format!(
                "record of {} bytes exceeds the {MAX_RECORD_BYTES} byte limit",
                bytes.len()
            )));
        }

        let framed = frame(&bytes);
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&framed)?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Clean for a store that was just created, and for one whose history was
    /// intact. A file is the backend that can lose a tail, so it always answers.
    fn recovery(&self) -> Option<RecoveryReport> {
        Some(self.recovery)
    }
}

/// The intact prefix of a store file.
struct Scan {
    records: Vec<Record>,
    /// Byte length of the frames that validated end to end.
    valid_len: u64,
}

/// Scans frames from the start, stopping at a torn final frame.
///
/// Only a torn *final* frame is tolerated. A frame that fails validation with
/// history after it cannot be explained by a crash mid-append, so it is
/// reported as corruption: truncating there would silently destroy every
/// record that follows.
fn scan(file: &File) -> Result<Scan, StoreError> {
    let total_len = file.metadata()?.len();
    let mut records = Vec::new();
    let mut offset: u64 = 0;

    loop {
        let remaining = total_len - offset;

        // Not even a header's worth of bytes: a torn final append.
        if remaining < FRAME_HEADER_LEN {
            break;
        }

        let mut header = [0u8; FRAME_HEADER_LEN as usize];
        read_exact_at(file, &mut header, offset)?;
        let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let expected_crc = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        let frame_len = FRAME_HEADER_LEN + len;

        // A frame longer than this store ever writes cannot be a torn append,
        // however the length field came to hold that value.
        if len > MAX_RECORD_BYTES {
            return Err(StoreError::corruption(format!(
                "frame at byte {offset} declares {len} bytes, above the \
                 {MAX_RECORD_BYTES} byte limit this store writes"
            )));
        }

        // Declared frame runs past the end of the file: a torn final append.
        if frame_len > remaining {
            break;
        }

        let mut body = vec![0u8; len as usize];
        read_exact_at(file, &mut body, offset + FRAME_HEADER_LEN)?;

        if crc32(&body) != expected_crc {
            if frame_len == remaining {
                // The final frame is complete in length but its checksum does
                // not match. A crash mid-append can leave exactly this, so it is
                // discarded. This is the one genuinely ambiguous case: rot in
                // the last record is indistinguishable from a torn write. The
                // cost of guessing wrong is bounded to a single record, and the
                // discard is reported through `FileStore::recovery`.
                break;
            }
            return Err(StoreError::corruption(format!(
                "frame at byte {offset} failed its checksum with {} bytes of history \
                 after it; only a torn final frame may be discarded",
                remaining - frame_len
            )));
        }

        // Checksum matched, so these bytes are exactly what was written; if they
        // do not decode, the history is genuinely inconsistent.
        records.push(decode_record(&body)?);
        offset += frame_len;
    }

    Ok(Scan {
        records,
        valid_len: offset,
    })
}

/// Positioned read, leaving the file's shared cursor untouched.
#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

/// Positioned read, leaving the file's shared cursor untouched.
#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

/// Reads exactly `buf.len()` bytes starting at `offset`.
///
/// Recovery must not disturb the shared file cursor: two threads replaying the
/// same history would otherwise interleave their seeks and each read the
/// other's bytes.
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match read_at(file, buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected end of file",
                ));
            }
            Ok(read) => {
                offset += read as u64;
                buf = &mut buf[read..];
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}
