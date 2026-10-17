//! The durable shape of belief events.
//!
//! These bytes go into history and stay there. A change to this encoding is a
//! migration with an upcast step, never an accident — which is what the pinned
//! encodings below are here to enforce.

use velora_belief::{
    AssertBelief, ClaimKey, ClaimValue, Confidence, Evidence, RetractBelief,
};
use velora_event::{DecodeError, Payload, PayloadVersion, Sequence};

fn sample_assertion() -> AssertBelief {
    AssertBelief::new(
        ClaimKey::new("k"),
        ClaimValue::new(PayloadVersion::new(1), b"v".to_vec()),
        Confidence::from_basis_points(5_000).unwrap(),
        Evidence::new([Sequence::new(1), Sequence::new(2)]).unwrap(),
    )
}

fn sample_withdrawal() -> RetractBelief {
    RetractBelief::new(
        ClaimKey::new("k"),
        "",
        Evidence::new([Sequence::new(3)]).unwrap(),
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn an_assertion_round_trips() {
    let assertion = sample_assertion();
    let bytes = assertion.encode();
    // Decoded at the version the encoder currently writes, so that a version
    // bump moves this test with it instead of silently testing the old shape.
    let decoded =
        AssertBelief::decode(PayloadVersion::new(AssertBelief::VERSION), &bytes).unwrap();
    assert_eq!(decoded, assertion);
}

#[test]
fn a_withdrawal_round_trips() {
    let withdrawal = sample_withdrawal();
    let bytes = withdrawal.encode();
    let decoded =
        RetractBelief::decode(PayloadVersion::new(RetractBelief::VERSION), &bytes).unwrap();
    assert_eq!(decoded, withdrawal);
}

#[test]
fn a_withdrawal_confidence_round_trips() {
    let withdrawal =
        sample_withdrawal().with_confidence(Confidence::from_basis_points(4_200).unwrap());
    let bytes = withdrawal.encode();
    let decoded =
        RetractBelief::decode(PayloadVersion::new(RetractBelief::VERSION), &bytes).unwrap();

    assert_eq!(decoded, withdrawal);
    assert_eq!(decoded.confidence().basis_points(), 4_200);
}

/// A version 1 withdrawal carried no strength and was applied unconditionally,
/// so a certain withdrawal is what it meant.
#[test]
fn a_v1_withdrawal_is_upcast_as_certain() {
    let bytes = retract_bytes("k", "", &[3]);
    let decoded = RetractBelief::decode(PayloadVersion::new(1), &bytes).unwrap();

    assert_eq!(decoded.key().as_str(), "k");
    assert_eq!(decoded.evidence().sequences(), &[Sequence::new(3)]);
    assert_eq!(decoded.confidence(), Confidence::CERTAIN);
}

/// Pins the version 1 encoding. History written before rationales existed still
/// has to be readable, so the old shape is frozen too — not just the new one.
#[test]
fn the_v1_assertion_encoding_is_frozen() {
    assert_eq!(
        hex(&assert_bytes("k", 1, b"v", 5_000, &[1, 2])),
        concat!(
            "00000001",         // key length
            "6b",               // "k"
            "00000001",         // value version
            "00000001",         // value length
            "76",               // "v"
            "1388",             // confidence, 5000 basis points
            "00000002",         // evidence count
            "0000000000000001", // sequence 1
            "0000000000000002", // sequence 2
        )
    );
}

/// The upcast in action: a version 1 assertion decodes into the current shape,
/// with no rationale, because none was recorded and history cannot be rewritten
/// to add one.
#[test]
fn a_v1_assertion_is_upcast_to_the_current_shape() {
    let bytes = assert_bytes("k", 1, b"v", 5_000, &[1, 2]);
    let decoded = AssertBelief::decode(PayloadVersion::new(1), &bytes).unwrap();

    assert_eq!(decoded.key().as_str(), "k");
    assert_eq!(decoded.value().bytes(), b"v");
    assert_eq!(decoded.confidence().basis_points(), 5_000);
    assert_eq!(decoded.evidence().sequences(), &[Sequence::new(1), Sequence::new(2)]);
    assert_eq!(decoded.rationale(), "");

    // And it is re-encodable at the current version, which is what makes an
    // upcast useful rather than merely readable.
    let re_encoded = decoded.encode();
    assert_eq!(
        AssertBelief::decode(PayloadVersion::new(2), &re_encoded).unwrap(),
        decoded
    );
}

/// Pins the current encoding: version 2 is version 1 with the rationale
/// appended. If this changes, every assertion ever recorded needs an upcast.
#[test]
fn the_current_assertion_encoding_is_frozen() {
    assert_eq!(
        hex(&sample_assertion().encode()),
        concat!(
            "00000001",         // key length
            "6b",               // "k"
            "00000001",         // value version
            "00000001",         // value length
            "76",               // "v"
            "1388",             // confidence, 5000 basis points
            "00000002",         // evidence count
            "0000000000000001", // sequence 1
            "0000000000000002", // sequence 2
            "00000000",         // rationale length, empty
        )
    );
}

#[test]
fn a_rationale_round_trips() {
    let assertion = sample_assertion().with_rationale("the event says so");
    let bytes = assertion.encode();
    let decoded = AssertBelief::decode(PayloadVersion::new(2), &bytes).unwrap();

    assert_eq!(decoded, assertion);
    assert_eq!(decoded.rationale(), "the event says so");
}


/// Pins the version 1 encoding, which history written before withdrawals had a
/// strength still contains.
#[test]
fn the_v1_withdrawal_encoding_is_frozen() {
    assert_eq!(
        hex(&retract_bytes("k", "", &[3])),
        concat!(
            "00000001",         // key length
            "6b",               // "k"
            "00000000",         // reason length, empty
            "00000001",         // evidence count
            "0000000000000003", // sequence 3
        )
    );
}

/// Pins the current encoding: version 2 inserts the confidence before the
/// evidence.
#[test]
fn the_current_withdrawal_encoding_is_frozen() {
    assert_eq!(
        hex(&sample_withdrawal().encode()),
        concat!(
            "00000001",         // key length
            "6b",               // "k"
            "00000000",         // reason length, empty
            "2710",             // confidence, 10000 basis points
            "00000001",         // evidence count
            "0000000000000003", // sequence 3
        )
    );
}

/// Builds an assertion payload by hand, so malformed shapes can be tested
/// directly rather than through a constructor that refuses to produce them.
fn assert_bytes(
    key: &str,
    value_version: u32,
    value: &[u8],
    confidence: u16,
    evidence: &[u64],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(key.len() as u32).to_be_bytes());
    out.extend_from_slice(key.as_bytes());
    out.extend_from_slice(&value_version.to_be_bytes());
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
    out.extend_from_slice(&confidence.to_be_bytes());
    out.extend_from_slice(&(evidence.len() as u32).to_be_bytes());
    for sequence in evidence {
        out.extend_from_slice(&sequence.to_be_bytes());
    }
    out
}

fn decode(bytes: &[u8]) -> Result<AssertBelief, DecodeError> {
    AssertBelief::decode(PayloadVersion::new(1), bytes)
}

/// Builds a version 1 withdrawal payload by hand.
fn retract_bytes(key: &str, reason: &str, evidence: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(key.len() as u32).to_be_bytes());
    out.extend_from_slice(key.as_bytes());
    out.extend_from_slice(&(reason.len() as u32).to_be_bytes());
    out.extend_from_slice(reason.as_bytes());
    out.extend_from_slice(&(evidence.len() as u32).to_be_bytes());
    for sequence in evidence {
        out.extend_from_slice(&sequence.to_be_bytes());
    }
    out
}

#[test]
fn confidence_above_certainty_is_refused() {
    let bytes = assert_bytes("k", 1, b"v", 10_001, &[1]);
    assert!(decode(&bytes).is_err());
}

#[test]
fn an_assertion_with_no_evidence_is_refused() {
    let bytes = assert_bytes("k", 1, b"v", 5_000, &[]);
    let err = decode(&bytes).unwrap_err();
    assert!(err.to_string().contains("at least one event"), "{err}");
}

#[test]
fn out_of_order_evidence_is_refused() {
    let bytes = assert_bytes("k", 1, b"v", 5_000, &[2, 1]);
    assert!(decode(&bytes).is_err(), "non-canonical evidence is not a valid shape");
}

#[test]
fn duplicate_evidence_is_refused() {
    let bytes = assert_bytes("k", 1, b"v", 5_000, &[1, 1]);
    assert!(decode(&bytes).is_err());
}

#[test]
fn trailing_bytes_are_refused() {
    let mut bytes = assert_bytes("k", 1, b"v", 5_000, &[1]);
    bytes.push(0);
    assert!(decode(&bytes).is_err());
}

#[test]
fn a_truncated_payload_is_refused() {
    let mut bytes = assert_bytes("k", 1, b"v", 5_000, &[1]);
    bytes.pop();
    assert!(decode(&bytes).is_err());
}

#[test]
fn an_invalid_key_encoding_is_refused() {
    let mut bytes = assert_bytes("k", 1, b"v", 5_000, &[1]);
    bytes[4] = 0xFF; // the single key byte is no longer UTF-8
    assert!(decode(&bytes).is_err());
}

/// The upcasting seam is real: an unknown version is refused by name rather
/// than silently guessed at.
#[test]
fn an_unknown_version_is_refused() {
    let bytes = sample_assertion().encode();
    let err = AssertBelief::decode(PayloadVersion::new(3), &bytes).unwrap_err();
    assert!(err.to_string().contains("version 3"), "{err}");
}
