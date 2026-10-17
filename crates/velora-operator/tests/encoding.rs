//! The conversation encodings, pinned.
//!
//! These bytes are written into history and never rewritten. Changing what they
//! are means a new version and an upcast, and these tests are what makes that
//! unavoidable rather than a thing someone remembers to do.

use velora_operator::Said;
use velora_event::{Payload, PayloadVersion};

#[test]
fn a_message_encodes_to_its_length_and_its_text() {
    let message = Said::new("hello");
    let bytes = message.encode();

    assert_eq!(
        bytes,
        vec![
            0, 0, 0, 5, // length
            b'h', b'e', b'l', b'l', b'o',
        ]
    );

    let read = Said::decode(PayloadVersion::new(1), &bytes).unwrap();
    assert_eq!(read, message);
}

#[test]
fn a_message_survives_text_that_is_not_ascii() {
    let message = Said::new("what does 你好 mean? 🎛");
    let bytes = message.encode();

    // The length prefix counts bytes, not characters, so text that is not ASCII
    // round-trips rather than being cut mid-character.
    assert_eq!(
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize,
        message.text().len()
    );
    assert_ne!(message.text().len(), message.text().chars().count());

    let read = Said::decode(PayloadVersion::new(1), &bytes).unwrap();
    assert_eq!(read.text(), message.text());
}

#[test]
fn an_empty_message_is_still_a_message() {
    let message = Said::new("");
    assert!(message.is_empty());
    assert_eq!(message.encode(), vec![0, 0, 0, 0]);

    let read = Said::decode(PayloadVersion::new(1), &message.encode()).unwrap();
    assert_eq!(read.text(), "");

    // Whitespace is nothing to answer either, but it is not the same bytes.
    assert!(Said::new("   \n").is_empty());
}

#[test]
fn a_truncated_message_is_refused_rather_than_guessed_at() {
    let bytes = Said::new("hello").encode();

    for cut in 0..bytes.len() {
        assert!(
            Said::decode(PayloadVersion::new(1), &bytes[..cut]).is_err(),
            "a {cut}-byte prefix should not decode"
        );
    }
}

#[test]
fn trailing_bytes_are_refused() {
    let mut bytes = Said::new("hello").encode();
    bytes.push(0);

    assert!(Said::decode(PayloadVersion::new(1), &bytes).is_err());
}
