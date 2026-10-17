use velora_event::{
    DecodeError, EventLog, MemoryStore, Payload, PayloadVersion, RecordDraft, Sensitivity,
    Timestamp,
};

/// A payload whose current shape is version 2. Version 1 stored only a name.
struct Greeting {
    name: String,
    punctuation: String,
}

impl Payload for Greeting {
    const KIND: &'static str = "test.greeting";
    const VERSION: u32 = 2;

    fn encode(&self) -> Vec<u8> {
        format!("{}\u{1f}{}", self.name, self.punctuation).into_bytes()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|err| DecodeError::new(format!("greeting is not UTF-8: {err}")))?;
        match version.get() {
            // Upcast: version 1 had no punctuation field.
            1 => Ok(Greeting {
                name: text.to_owned(),
                punctuation: "!".to_owned(),
            }),
            2 => {
                let (name, punctuation) = text
                    .split_once('\u{1f}')
                    .ok_or_else(|| DecodeError::new("version 2 greeting is malformed"))?;
                Ok(Greeting {
                    name: name.to_owned(),
                    punctuation: punctuation.to_owned(),
                })
            }
            other => Err(DecodeError::new(format!(
                "unknown greeting version {other}"
            ))),
        }
    }
}

#[test]
fn current_version_round_trips() {
    let mut log = EventLog::open(MemoryStore::new()).unwrap();
    let greeting = Greeting {
        name: "Ada".to_owned(),
        punctuation: "?".to_owned(),
    };
    log.append_typed(&greeting, Timestamp::from_parts(0, 0), Sensitivity::PUBLIC)
        .unwrap();

    let records = log.replay().unwrap();
    let decoded = records[0].decode_payload::<Greeting>().unwrap();
    assert_eq!(decoded.name, "Ada");
    assert_eq!(decoded.punctuation, "?");
}

#[test]
fn old_versions_are_upcast_on_read() {
    let mut log = EventLog::open(MemoryStore::new()).unwrap();
    // A record written long ago, before the payload gained its second field.
    let draft = RecordDraft::new(
        Greeting::KIND,
        PayloadVersion::new(1),
        b"Ada".to_vec(),
        Timestamp::from_parts(0, 0),
        Sensitivity::PUBLIC,
    );
    log.append(draft).unwrap();

    let records = log.replay().unwrap();
    assert_eq!(records[0].version(), PayloadVersion::new(1));
    let decoded = records[0].decode_payload::<Greeting>().unwrap();
    assert_eq!(decoded.name, "Ada");
    assert_eq!(decoded.punctuation, "!");
}

#[test]
fn decoding_the_wrong_kind_is_an_error() {
    struct Other;

    impl Payload for Other {
        const KIND: &'static str = "test.other";
        const VERSION: u32 = 1;

        fn encode(&self) -> Vec<u8> {
            Vec::new()
        }

        fn decode(_version: PayloadVersion, _bytes: &[u8]) -> Result<Self, DecodeError> {
            Ok(Other)
        }
    }

    let mut log = EventLog::open(MemoryStore::new()).unwrap();
    let draft = RecordDraft::new(
        Greeting::KIND,
        PayloadVersion::new(1),
        b"Ada".to_vec(),
        Timestamp::from_parts(0, 0),
        Sensitivity::PUBLIC,
    );
    log.append(draft).unwrap();

    let records = log.replay().unwrap();
    assert!(records[0].decode_payload::<Other>().is_err());
}
