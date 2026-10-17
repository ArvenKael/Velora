use velora_event::{DecodeError, Payload, PayloadVersion, Sequence};

use crate::belief::Belief;
use crate::claim::{ClaimKey, ClaimValue};
use velora_event::codec::{Reader, Writer};
use crate::confidence::Confidence;
use crate::evidence::Evidence;

/// The durable form of "this is so, on this evidence".
///
/// This is what a decider records when it concludes something: the claim, how
/// strongly it is held, the events it was drawn from, and — since version 2 —
/// the reasoning that connected them.
///
/// # Encoding
///
/// Version 1:
///
/// ```text
/// key            length-prefixed UTF-8
/// value version  u32
/// value          length-prefixed bytes
/// confidence     u16 basis points
/// evidence       u32 count, then count × u64 sequence, ascending and unique
/// ```
///
/// Version 2 appends:
///
/// ```text
/// rationale      length-prefixed UTF-8, may be empty
/// ```
///
/// A version 1 record still decodes, upcast to the current shape with an empty
/// rationale. That is the whole point of carrying a version: the reasoning was
/// not recorded then, and history cannot be rewritten to add it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertBelief {
    key: ClaimKey,
    value: ClaimValue,
    confidence: Confidence,
    evidence: Evidence,
    rationale: String,
}

impl AssertBelief {
    /// Builds an assertion with no stated reasoning.
    pub fn new(
        key: ClaimKey,
        value: ClaimValue,
        confidence: Confidence,
        evidence: Evidence,
    ) -> Self {
        AssertBelief {
            key,
            value,
            confidence,
            evidence,
            rationale: String::new(),
        }
    }

    /// Adds the reasoning behind the assertion.
    pub fn with_rationale(mut self, rationale: impl Into<String>) -> Self {
        self.rationale = rationale.into();
        self
    }

    /// What is claimed.
    pub fn key(&self) -> &ClaimKey {
        &self.key
    }

    /// What is claimed to be so.
    pub fn value(&self) -> &ClaimValue {
        &self.value
    }

    /// How strongly it is claimed.
    pub fn confidence(&self) -> Confidence {
        self.confidence
    }

    /// The events it is drawn from.
    pub fn evidence(&self) -> &Evidence {
        &self.evidence
    }

    /// Why, in the asserter's words. Empty for anything asserted before this
    /// field existed, and for a determinist that has nothing to explain.
    pub fn rationale(&self) -> &str {
        &self.rationale
    }

    /// The belief this assertion puts forward, recorded at sequence `at`.
    pub fn into_belief(self, at: Sequence) -> Belief {
        Belief::new(self.key, self.value, self.confidence, self.evidence, at)
            .with_rationale(self.rationale)
    }
}

impl Payload for AssertBelief {
    const KIND: &'static str = "belief.assert";
    const VERSION: u32 = 2;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        write_common(&mut writer, &self.key, &self.value, self.confidence, &self.evidence);
        writer.string(&self.rationale);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        // The upcasting seam. Every historical shape is accepted here and
        // returned in the current shape, which is what keeps old history
        // readable after this type changes.
        match version.get() {
            1 => decode_v1(bytes),
            2 => decode_v2(bytes),
            other => Err(DecodeError::new(format!(
                "unsupported belief.assert version {other}"
            ))),
        }
    }
}

fn write_common(
    writer: &mut Writer,
    key: &ClaimKey,
    value: &ClaimValue,
    confidence: Confidence,
    evidence: &Evidence,
) {
    writer.string(key.as_str());
    writer.u32(value.version().get());
    writer.bytes(value.bytes());
    writer.u16(confidence.basis_points());
    write_evidence(writer, evidence);
}

fn write_evidence(writer: &mut Writer, evidence: &Evidence) {
    writer.u32(evidence.len() as u32);
    for sequence in evidence.sequences() {
        writer.u64(sequence.get());
    }
}

/// Everything a version 1 and version 2 assertion have in common.
fn read_common(reader: &mut Reader<'_>) -> Result<(ClaimKey, ClaimValue, Confidence, Evidence), DecodeError> {
    let key = ClaimKey::new(reader.string()?);
    let value_version = PayloadVersion::new(reader.u32()?);
    let value = ClaimValue::new(value_version, reader.bytes()?.to_vec());

    let basis_points = reader.u16()?;
    let confidence = Confidence::from_basis_points(basis_points)
        .map_err(|err| DecodeError::new(err.to_string()))?;

    let evidence = decode_evidence(reader)?;

    Ok((key, value, confidence, evidence))
}

/// Reads a version 1 assertion: no rationale was recorded, so there is none.
fn decode_v1(bytes: &[u8]) -> Result<AssertBelief, DecodeError> {
    let mut reader = Reader::new(bytes);
    let (key, value, confidence, evidence) = read_common(&mut reader)?;

    if !reader.is_empty() {
        return Err(DecodeError::new("belief.assert has trailing bytes"));
    }

    Ok(AssertBelief {
        key,
        value,
        confidence,
        evidence,
        rationale: String::new(),
    })
}

fn decode_v2(bytes: &[u8]) -> Result<AssertBelief, DecodeError> {
    let mut reader = Reader::new(bytes);
    let (key, value, confidence, evidence) = read_common(&mut reader)?;
    let rationale = reader.string()?;

    if !reader.is_empty() {
        return Err(DecodeError::new("belief.assert has trailing bytes"));
    }

    Ok(AssertBelief {
        key,
        value,
        confidence,
        evidence,
        rationale,
    })
}

/// The durable form of "this is no longer so, on this evidence".
///
/// Withdrawal is grounded too: a belief is not dropped because it became
/// inconvenient, but because something happened. That something is cited — and,
/// since version 2, the withdrawal says how strongly it is made, so a weak
/// withdrawal cannot quietly overturn a strongly held belief.
///
/// # Encoding
///
/// Version 1:
///
/// ```text
/// key         length-prefixed UTF-8
/// reason      length-prefixed UTF-8, may be empty
/// evidence    u32 count, then count × u64 sequence, ascending and unique
/// ```
///
/// Version 2 inserts a confidence before the evidence:
///
/// ```text
/// key         length-prefixed UTF-8
/// reason      length-prefixed UTF-8, may be empty
/// confidence  u16 basis points
/// evidence    u32 count, then count × u64 sequence, ascending and unique
/// ```
///
/// A version 1 withdrawal decodes as a certain one, which is what it meant: it
/// was applied unconditionally, because there was nothing to weigh it against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetractBelief {
    key: ClaimKey,
    reason: String,
    confidence: Confidence,
    evidence: Evidence,
}

impl RetractBelief {
    /// Builds a certain withdrawal.
    pub fn new(key: ClaimKey, reason: impl Into<String>, evidence: Evidence) -> Self {
        RetractBelief {
            key,
            reason: reason.into(),
            confidence: Confidence::CERTAIN,
            evidence,
        }
    }

    /// Sets how strongly the claim is withdrawn.
    pub fn with_confidence(mut self, confidence: Confidence) -> Self {
        self.confidence = confidence;
        self
    }

    /// The claim being withdrawn.
    pub fn key(&self) -> &ClaimKey {
        &self.key
    }

    /// Why, in prose. The evidence is the authoritative part.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// How strongly it is withdrawn.
    pub fn confidence(&self) -> Confidence {
        self.confidence
    }

    /// The events it is drawn from.
    pub fn evidence(&self) -> &Evidence {
        &self.evidence
    }
}

impl Payload for RetractBelief {
    const KIND: &'static str = "belief.retract";
    const VERSION: u32 = 2;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.string(self.key.as_str());
        writer.string(&self.reason);
        writer.u16(self.confidence.basis_points());
        write_evidence(&mut writer, &self.evidence);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => decode_retract_v1(bytes),
            2 => decode_retract_v2(bytes),
            other => Err(DecodeError::new(format!(
                "unsupported belief.retract version {other}"
            ))),
        }
    }
}

fn decode_retract_v1(bytes: &[u8]) -> Result<RetractBelief, DecodeError> {
    let mut reader = Reader::new(bytes);
    let key = ClaimKey::new(reader.string()?);
    let reason = reader.string()?;
    let evidence = decode_evidence(&mut reader)?;

    if !reader.is_empty() {
        return Err(DecodeError::new("belief.retract has trailing bytes"));
    }

    Ok(RetractBelief {
        key,
        reason,
        // Version 1 withdrawals had no strength and were applied
        // unconditionally, so a certain withdrawal is what they meant.
        confidence: Confidence::CERTAIN,
        evidence,
    })
}

fn decode_retract_v2(bytes: &[u8]) -> Result<RetractBelief, DecodeError> {
    let mut reader = Reader::new(bytes);
    let key = ClaimKey::new(reader.string()?);
    let reason = reader.string()?;

    let basis_points = reader.u16()?;
    let confidence = Confidence::from_basis_points(basis_points)
        .map_err(|err| DecodeError::new(err.to_string()))?;

    let evidence = decode_evidence(&mut reader)?;

    if !reader.is_empty() {
        return Err(DecodeError::new("belief.retract has trailing bytes"));
    }

    Ok(RetractBelief {
        key,
        reason,
        confidence,
        evidence,
    })
}

/// Reads a canonical evidence list.
///
/// Empty is refused, and so is anything not strictly ascending: accepting an
/// out-of-order list would make `encode` and `decode` disagree about the same
/// value, and these bytes live in history forever.
fn decode_evidence(reader: &mut Reader<'_>) -> Result<Evidence, DecodeError> {
    let count = reader.u32()? as usize;
    if count == 0 {
        return Err(DecodeError::new("a belief must cite at least one event"));
    }

    // Cap the pre-allocation: `count` comes from untrusted bytes, and a
    // truncated payload should fail on the next read, not on an allocation.
    let mut sequences = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        sequences.push(Sequence::new(reader.u64()?));
    }

    Evidence::from_canonical(sequences)
        .ok_or_else(|| DecodeError::new("evidence must be ascending and free of duplicates"))
}
