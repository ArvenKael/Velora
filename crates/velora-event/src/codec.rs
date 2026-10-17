use crate::record::{
    DecodeError, PayloadKind, PayloadVersion, Record, Sensitivity, Sequence, Timestamp,
};
use crate::store::StoreError;

/// Big-endian, self-delimiting writer for canonical history encodings.
///
/// Every payload that goes into history is written through this, so that
/// `encode` and `decode` cannot drift apart in their idea of what a length
/// prefix is, and so that a change to the shared shape is a change in one
/// place. Canonical here means: same value, same bytes, on every machine and
/// every build — which is what makes a digest of history mean anything.
pub struct Writer {
    out: Vec<u8>,
}

impl Default for Writer {
    fn default() -> Self {
        Writer::new()
    }
}

impl Writer {
    /// An empty writer.
    pub fn new() -> Self {
        Writer { out: Vec::new() }
    }

    /// Writes one byte.
    pub fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    /// Writes a big-endian `u16`.
    pub fn u16(&mut self, value: u16) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes a big-endian `u32`.
    pub fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes a big-endian `u64`.
    pub fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes a big-endian `i64`.
    pub fn i64(&mut self, value: i64) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes bytes, preceded by their length.
    pub fn bytes(&mut self, value: &[u8]) {
        self.u32(value.len() as u32);
        self.out.extend_from_slice(value);
    }

    /// Writes UTF-8 text, preceded by its length in bytes.
    pub fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    /// The encoded bytes.
    pub fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// Big-endian reader that refuses to read past the end.
///
/// Every failure names what went wrong rather than returning a short read: a
/// payload that does not decode is either a bug or corruption, and neither is
/// served by guessing.
pub struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader over an encoded payload.
    pub fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, pos: 0 }
    }

    /// Reads one byte.
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    /// Reads a big-endian `u16`.
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        let raw = self.take(2)?;
        Ok(u16::from_be_bytes([raw[0], raw[1]]))
    }

    /// Reads a big-endian `u32`.
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        let raw = self.take(4)?;
        Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    /// Reads a big-endian `u64`.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(self.take(8)?);
        Ok(u64::from_be_bytes(raw))
    }

    /// Reads a big-endian `i64`.
    pub fn i64(&mut self) -> Result<i64, DecodeError> {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(self.take(8)?);
        Ok(i64::from_be_bytes(raw))
    }

    /// Reads a length-prefixed byte string.
    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    /// Reads length-prefixed UTF-8.
    pub fn string(&mut self) -> Result<String, DecodeError> {
        let raw = self.bytes()?;
        std::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| DecodeError::new("expected UTF-8 text and found none"))
    }

    /// Whether everything has been read.
    ///
    /// Worth checking before accepting a payload: trailing bytes mean the
    /// encoding and the decoder disagree, and history cannot be rewritten once
    /// the wrong shape has been written into it.
    pub fn is_empty(&self) -> bool {
        self.pos == self.bytes.len()
    }

    /// How many bytes are left.
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| DecodeError::new("length overflow"))?;
        if end > self.bytes.len() {
            return Err(DecodeError::new("payload is truncated"));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }
}

/// Canonical, self-delimiting encoding of a record.
///
/// This exact byte sequence is what both the durable frames and the
/// [`crate::Digest`] are computed over, so the digest is identical across
/// machines and builds for identical history.
///
/// Public because it is the definition of what a record *is* on disk: a store
/// that keeps history somewhere else has to keep these bytes, or reproduce them
/// exactly, or digests of the same history stop matching across backends.
pub fn encode_record(record: &Record) -> Vec<u8> {
    let kind = record.kind().as_str().as_bytes();
    let payload = record.payload();

    let mut writer = Writer::new();
    writer.u64(record.sequence().get());
    writer.i64(record.time().secs());
    writer.u32(record.time().nanos());
    writer.u32(record.version().get());
    writer.bytes(kind);
    writer.u8(record.sensitivity().level());
    writer.bytes(payload);
    writer.finish()
}

pub fn decode_record(bytes: &[u8]) -> Result<Record, StoreError> {
    let mut reader = Reader::new(bytes);

    let sequence = Sequence::new(corrupt(reader.u64())?);
    let secs = corrupt(reader.i64())?;
    let nanos = corrupt(reader.u32())?;
    let version = PayloadVersion::new(corrupt(reader.u32())?);
    let kind = corrupt(reader.string())?;
    let sensitivity = Sensitivity::from_level(corrupt(reader.u8())?);
    let payload = corrupt(reader.bytes())?.to_vec();

    if !reader.is_empty() {
        return Err(StoreError::corruption("record has trailing bytes"));
    }

    Ok(Record {
        sequence,
        time: Timestamp::from_parts(secs, nanos),
        kind: PayloadKind::new(kind),
        version,
        sensitivity,
        payload,
    })
}

/// A record that cannot be decoded is corruption, not a decode problem: the
/// bytes were written by this system's own encoder, checksum and all.
fn corrupt<T>(result: Result<T, DecodeError>) -> Result<T, StoreError> {
    result.map_err(|error| StoreError::corruption(error.to_string()))
}

/// Frames `record_bytes` as `[u32 length][u32 crc32][bytes]`.
pub(crate) fn frame(record_bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + record_bytes.len());
    out.extend_from_slice(&(record_bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32(record_bytes).to_be_bytes());
    out.extend_from_slice(record_bytes);
    out
}

/// CRC-32/ISO-HDLC, computed without a lookup table.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}
