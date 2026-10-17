//! What the model is shown of what has been said, and why it has to be there.
//!
//! Every thought is a fresh request. Nothing carries over between turns except
//! what this crate puts in the context, so these tests are the difference
//! between a system that can follow what is being said and one answering
//! unrelated questions in the same window.

use std::sync::Arc;

use velora_belief::{Belief, BeliefStore, ClaimKey, ClaimValue, Confidence, Evidence};
use velora_context::{Assembler, Budget, Explain, Situation};
use velora_event::{
    EventLog, MemoryStore, Payload, PayloadVersion, Record, RecordDraft, Sensitivity, Sequence,
    Timestamp,
};
use velora_exec::{Outcome, StepResult};
use velora_operator::{SAY_PROCEDURE, Said};

/// A history of things said, plus anything else, in order.
enum Line {
    Operator(String),
    System(String),
    FailedSystem(String),
    Other(&'static str, Vec<u8>),
}

fn operator(text: &str) -> Line {
    Line::Operator(text.to_owned())
}

fn system(text: &str) -> Line {
    Line::System(text.to_owned())
}

fn other(kind: &'static str, text: &str) -> Line {
    Line::Other(kind, text.as_bytes().to_vec())
}

fn history(said: &[Line]) -> Vec<Record> {
    let mut log = EventLog::open(MemoryStore::new()).unwrap();

    for item in said {
        let draft = match item {
            Line::Operator(text) => typed(Said::new(text)),
            // The system speaks by running the channel, so what it said is what
            // that step reported.
            Line::System(text) => typed(StepResult::from_outcome(
                Sequence::new(1),
                SAY_PROCEDURE,
                &Outcome::ok(text),
            )),
            Line::FailedSystem(text) => typed(StepResult::from_outcome(
                Sequence::new(1),
                SAY_PROCEDURE,
                &Outcome::failed(text),
            )),
            // Raw bytes, as a source that is not a typed payload would write.
            Line::Other(kind, bytes) => RecordDraft::new(
                *kind,
                PayloadVersion::new(1),
                bytes.clone(),
                Timestamp::from_parts(1, 0),
                Sensitivity::INTERNAL,
            ),
        };
        log.append(draft).unwrap();
    }

    log.replay().unwrap()
}

fn typed<P: Payload>(payload: P) -> RecordDraft {
    RecordDraft::new(
        P::KIND,
        PayloadVersion::new(P::VERSION),
        payload.encode(),
        Timestamp::from_parts(1, 0),
        Sensitivity::INTERNAL,
    )
}

fn beliefs(entries: &[(&str, &str, u16, &[u64])]) -> BeliefStore {
    let mut store = BeliefStore::new();
    for (key, value, confidence, evidence) in entries {
        // The citing event sits one past its newest piece of evidence: a belief
        // cannot be drawn from an event that had not happened yet.
        let at = evidence.iter().copied().max().unwrap_or(0) + 1;
        store
            .assert(Belief::new(
                ClaimKey::new(*key),
                ClaimValue::new(PayloadVersion::new(1), value.as_bytes().to_vec()),
                Confidence::from_basis_points(*confidence).unwrap(),
                Evidence::new(evidence.iter().copied().map(Sequence::new)).unwrap(),
                Sequence::new(at),
            ))
            .unwrap();
    }
    store
}

fn assemble(records: &[Record], beliefs: &BeliefStore) -> String {
    Assembler::new()
        .assemble(&Situation {
            task: "reply to the operator",
            beliefs,
            history: records,
            focus: records.last(),
        })
        .render()
}

#[test]
fn the_dialogue_is_shown_with_who_said_what() {
    let records = history(&[
        operator("are you there?"),
        system("I am. What do you need?"),
    ]);

    let rendered = assemble(&records, &BeliefStore::new());

    assert!(rendered.contains("What has been said"), "{rendered}");
    assert!(rendered.contains("operator: are you there?"), "{rendered}");
    assert!(
        rendered.contains("you: I am. What do you need?"),
        "{rendered}"
    );
}

#[test]
fn the_conversation_comes_before_what_is_believed() {
    // The section that cannot be reconstructed from anything else goes first:
    // beliefs summarise the world, and no belief records who was talking.
    let records = history(&[
        other("observation.setting", "theme=dark"),
        operator("what theme am I using?"),
        system("dark"),
    ]);
    let beliefs = beliefs(&[("theme", "dark", 9_000, &[1])]);

    let rendered = assemble(&records, &beliefs);

    let said = rendered.find("What has been said").unwrap();
    let believed = rendered.find("What is believed").unwrap();
    let lately = rendered.find("What has happened lately").unwrap();

    assert!(said < believed, "{rendered}");
    assert!(believed < lately, "{rendered}");
}

#[test]
fn the_order_of_the_dialogue_is_the_order_it_happened() {
    let records = history(&[
        operator("first"),
        system("second"),
        operator("third"),
        system("fourth"),
    ]);

    let rendered = assemble(&records, &BeliefStore::new());
    let transcript = rendered.split("## What is believed").next().unwrap();

    // Searched for with their labels: a bare word also appears in the prompt
    // line above the transcript, which is not the order being asserted.
    let first = transcript.find("operator: first").unwrap();
    let second = transcript.find("you: second").unwrap();
    let third = transcript.find("operator: third").unwrap();
    let fourth = transcript.find("you: fourth").unwrap();

    assert!(
        first < second && second < third && third < fourth,
        "{transcript}"
    );
}

#[test]
fn a_long_conversation_shows_its_end_and_says_what_it_dropped() {
    let mut said: Vec<Line> = Vec::new();
    for index in 0..20 {
        said.push(operator(&format!("older question {index}")));
        said.push(system(&format!("older answer {index}")));
    }
    said.push(operator("the thing I just asked"));
    let records = history(&said);

    let rendered = Assembler::new()
        .with_budget(Budget::default().with_max_transcript(4))
        .assemble(&Situation {
            task: "reply",
            beliefs: &BeliefStore::new(),
            history: &records,
            focus: records.last(),
        })
        .render();

    assert!(rendered.contains("the thing I just asked"), "{rendered}");
    assert!(
        rendered.contains("37 earlier things said not shown"),
        "the count of what was dropped is part of the context: {rendered}"
    );
    assert!(
        !rendered.contains("older question 0"),
        "the oldest things are the ones that go: {rendered}"
    );
    assert!(
        rendered.contains("older question 19"),
        "the newest things are the ones that stay: {rendered}"
    );
}

#[test]
fn a_conversation_that_cannot_be_read_is_reported_rather_than_shortened() {
    let mut log = EventLog::open(MemoryStore::new()).unwrap();
    log.append(typed(Said::new("this one is fine")))
        .unwrap();
    log.append(RecordDraft::new(
        Said::KIND,
        PayloadVersion::new(9),
        b"from a version nobody knows".to_vec(),
        Timestamp::from_parts(1, 0),
        Sensitivity::INTERNAL,
    ))
    .unwrap();
    let records = log.replay().unwrap();

    let rendered = assemble(&records, &BeliefStore::new());

    // The hole is in the transcript and named, so the model is not reasoning
    // from a conversation it believes was complete.
    assert!(rendered.contains("this one is fine"), "{rendered}");
    assert!(rendered.contains("could not be read"), "{rendered}");
    assert!(
        rendered.contains("1 thing said could not be read back"),
        "{rendered}"
    );
}

#[test]
fn a_step_of_speaking_that_failed_said_nothing() {
    let records = history(&[
        operator("do the thing"),
        Line::FailedSystem("there was nothing to say".to_owned()),
    ]);

    let rendered = assemble(&records, &BeliefStore::new());

    // Scoped to the section: the raw record is still listed under what has
    // happened lately, because it happened, and this is about what counts as
    // dialogue.
    let section = rendered
        .split("## What has been said")
        .nth(1)
        .and_then(|rest| rest.split("\n\n## ").next())
        .unwrap_or("");

    assert!(section.contains("operator: do the thing"), "{section}");
    assert!(
        !section.contains("nothing to say"),
        "a step that failed said nothing: {section}"
    );
}

#[test]
fn history_with_nothing_said_has_no_section_for_it() {
    let records = history(&[other("observation.setting", "theme=dark")]);

    let rendered = assemble(&records, &BeliefStore::new());

    // Not an empty heading: a section announcing that nothing was said would
    // invite the model to comment on the silence.
    assert!(!rendered.contains("What has been said"), "{rendered}");
}

#[test]
fn the_transcript_can_be_turned_off_and_says_so() {
    let records = history(&[operator("are you there?")]);

    let rendered = Assembler::new()
        .with_budget(Budget::default().with_max_transcript(0))
        .assemble(&Situation {
            task: "reply",
            beliefs: &BeliefStore::new(),
            history: &records,
            focus: None,
        })
        .render();

    // Turning it off is a decision someone made, and the context records that
    // the decision was made rather than looking like a quiet conversation.
    assert!(rendered.contains("What has been said"), "{rendered}");
    assert!(rendered.contains("(not shown)"), "{rendered}");
}

#[test]
fn the_runtime_decides_how_a_record_reads_not_this_crate() {
    // The payload here is not text at all. Without a vocabulary the assembler
    // can only show the bytes, which is the truth and not an interpretation.
    let records = history(&[other("belief.assert", "abc")]);

    let plain = assemble(&records, &BeliefStore::new());
    assert!(plain.contains("#1 belief.assert"), "{plain}");

    #[derive(Debug)]
    struct Vocabulary;

    impl Explain for Vocabulary {
        fn explain_payload(
            &self,
            _kind: &velora_event::PayloadKind,
            _version: velora_event::PayloadVersion,
            payload: &[u8],
        ) -> String {
            format!("a belief, in {} bytes", payload.len())
        }
    }

    let explained = Assembler::new()
        .with_explain(Arc::new(Vocabulary))
        .assemble(&Situation {
            task: "reply to the operator",
            beliefs: &BeliefStore::new(),
            history: &records,
            focus: None,
        })
        .render();

    // Same record, same position, and now something a model can use — which is
    // the whole reason the seam exists.
    assert!(
        explained.contains("#1 belief.assert a belief, in 3 bytes"),
        "{explained}"
    );
}

#[test]
fn a_description_written_over_several_lines_is_kept_to_one() {
    // A description that renders as several lines would read as several events,
    // and the model would be reading a shape history does not have.
    let records = history(&[other("observation.note", "one\ntwo\nthree")]);

    let rendered = assemble(&records, &BeliefStore::new());
    let line = rendered
        .lines()
        .find(|line| line.contains("observation.note"))
        .unwrap();

    assert!(line.contains("one two three"), "{line}");
}
