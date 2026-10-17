//! Reading what has been said out of ordinary events.

use velora_event::{
    EventLog, MemoryStore, Payload, PayloadVersion, Record, RecordDraft, Sensitivity, Sequence,
    Timestamp,
};
use velora_exec::{Outcome, StepResult};
use velora_operator::{SAY_PROCEDURE, Said, Speaker, Transcript};

fn draft(kind: &str, version: u32, bytes: Vec<u8>) -> RecordDraft {
    RecordDraft::new(
        kind,
        PayloadVersion::new(version),
        bytes,
        Timestamp::from_parts(1_700_000_000, 0),
        Sensitivity::INTERNAL,
    )
}

fn said(text: &str) -> RecordDraft {
    draft(Said::KIND, Said::VERSION, Said::new(text).encode())
}

/// A step of the system speaking.
fn spoke(text: &str) -> RecordDraft {
    draft(
        StepResult::KIND,
        StepResult::VERSION,
        StepResult::from_outcome(Sequence::new(1), SAY_PROCEDURE, &Outcome::ok(text)).encode(),
    )
}

/// A step of something else, which is not dialogue.
fn did(procedure: &str, text: &str) -> RecordDraft {
    draft(
        StepResult::KIND,
        StepResult::VERSION,
        StepResult::from_outcome(Sequence::new(1), procedure, &Outcome::ok(text)).encode(),
    )
}

fn history(drafts: Vec<RecordDraft>) -> Vec<Record> {
    let mut log = EventLog::open(MemoryStore::new()).unwrap();
    for item in drafts {
        log.append(item).unwrap();
    }
    log.replay().unwrap()
}

#[test]
fn a_dialogue_is_two_kinds_of_ordinary_record() {
    let transcript = Transcript::read(&history(vec![
        said("is the lamp on?"),
        // Something else entirely, in between, as most of history is.
        did("local.shell.run", "Cargo.lock"),
        draft("observation.setting", 1, b"theme=dark".to_vec()),
        spoke("It is now."),
    ]));

    let lines: Vec<(Speaker, &str)> = transcript
        .utterances()
        .iter()
        .map(|utterance| (utterance.speaker(), utterance.text()))
        .collect();

    assert_eq!(
        lines,
        vec![
            (Speaker::Operator, "is the lamp on?"),
            (Speaker::System, "It is now."),
        ]
    );

    // Nothing here is a conversation record: one is an event the operator
    // submitted, the other is what a step of speaking reported.
    assert_eq!(transcript.len(), 2);
    assert_eq!(transcript.at(1).unwrap().text(), "is the lamp on?");
}

#[test]
fn work_that_was_not_speaking_is_not_part_of_the_dialogue() {
    let transcript = Transcript::read(&history(vec![
        said("what is in here?"),
        did("local.shell.run", "Cargo.lock"),
        did("local.fs.read", "the contents"),
    ]));

    assert_eq!(transcript.len(), 1);
    assert_eq!(transcript.last().unwrap().speaker(), Speaker::Operator);
}

#[test]
fn a_step_of_speaking_that_failed_said_nothing() {
    let failed = draft(
        StepResult::KIND,
        StepResult::VERSION,
        StepResult::from_outcome(
            Sequence::new(1),
            SAY_PROCEDURE,
            &Outcome::failed("there was nothing to say"),
        )
        .encode(),
    );

    let transcript = Transcript::read(&history(vec![said("hello"), failed]));

    assert_eq!(transcript.len(), 1, "a refusal is not something said");
}

#[test]
fn something_that_cannot_be_read_back_is_kept_and_reported() {
    let transcript = Transcript::read(&history(vec![
        said("this one is fine"),
        // Something written at a version nothing knows how to read.
        draft(Said::KIND, 9, b"future".to_vec()),
        spoke("and so is this"),
    ]));

    // Kept, so the dialogue does not silently lose a turn...
    assert_eq!(transcript.len(), 3);
    assert_eq!(transcript.unreadable(), 1);
    assert!(transcript.utterances()[1].is_unreadable());

    // ...and marked, so nobody reads the placeholder as something that was said.
    assert!(
        transcript.utterances()[1].text().contains("could not be read"),
        "{}",
        transcript.utterances()[1].text()
    );
}

#[test]
fn the_recent_part_of_a_long_dialogue_is_the_end_of_it() {
    let mut drafts = Vec::new();
    for index in 0..50 {
        drafts.push(said(&format!("turn {index}")));
    }

    let transcript = Transcript::read(&history(drafts));

    assert_eq!(transcript.recent(3).len(), 3);
    assert_eq!(transcript.recent(3)[0].text(), "turn 47");
    assert_eq!(transcript.last().unwrap().text(), "turn 49");

    // Asking for more than there is gives everything, not a panic.
    assert_eq!(transcript.recent(500).len(), 50);
    assert!(transcript.recent(0).is_empty());
}

#[test]
fn the_speakers_are_named_from_the_models_side_of_the_dialogue() {
    // The model reading this *is* the system: its own earlier words have to read
    // as its own, or it treats the dialogue as somebody else's.
    assert_eq!(Speaker::Operator.label(), "the operator");
    assert_eq!(Speaker::System.label(), "you");
    assert_eq!(Speaker::Operator.line_label(), "operator");
    assert_eq!(Speaker::System.line_label(), "you");
}

#[test]
fn an_empty_history_has_nothing_said() {
    let transcript = Transcript::read(&[]);
    assert!(transcript.is_empty());
    assert_eq!(transcript.len(), 0);
    assert_eq!(transcript.last(), None);
    assert_eq!(transcript.unreadable(), 0);
}
