//! What every record the system writes actually says.
//!
//! The log holds canonical bytes, which is right for a log and useless to
//! everyone else: a belief is a claim, a confidence, a list of sequences and a
//! rationale, and read as text it is noise with control characters in it. This
//! is the runtime's vocabulary — the one place that knows all of its own payload
//! shapes and can turn any of them into a sentence.
//!
//! It is used three times, deliberately: by the context assembler, so the model
//! is told what happened in words rather than in bytes; by the judge, which is
//! shown events as they arrive and decides what they mean; and by the screen, so
//! the operator reads the same sentence. One function, so none of them can
//! disagree about what the system did.
//!
//! Nothing here decides what is worth showing or how much fits. A description is
//! the whole of what a record says; the assembler clips it to its budget and the
//! screen truncates it visually. Losing text here to make a list tidy would lose
//! it for the model too.

use std::time::Duration;

use velora_belief::{AssertBelief, RetractBelief};
use velora_capability::{
    Observation, ObserverDefined, ObserverRemoved, SkillDefined, SkillRemoved,
    DEFINE_OBSERVER_PROCEDURE, DEFINE_PROCEDURE,
};
use velora_config::{ConfigChange, ConfigRefused};
use velora_context::Explain;
use velora_operator::{SAY_PROCEDURE, Said};
use velora_event::{Payload, PayloadKind, PayloadVersion, Record};
use velora_exec::{StepRefused, StepRequest, StepResult};
use velora_mind::{Considered, Wake};
use velora_plan::{PlanAbandoned, PlanCreated, PlanRevised, StepDone, StepReady};
use velora_pulse::Idle;
use velora_reason::{ReasonFailed, ReasonFinished, ReasonRequest, ReasonThought};

/// Reads every kind the runtime writes.
#[derive(Debug, Clone, Copy, Default)]
pub struct Vocabulary;

/// Decodes a payload, saying so rather than guessing when it cannot.
fn read<P: Payload>(kind: &PayloadKind, version: PayloadVersion, payload: &[u8]) -> Result<P, String> {
    P::decode(version, payload).map_err(|err| format!("(this {kind} could not be read: {err})"))
}

/// How long something is, in the shortest notation that is still exact.
fn spelled(every: Duration) -> String {
    let milliseconds = every.as_millis();
    if milliseconds.is_multiple_of(1_000) {
        format!("{}s", milliseconds / 1_000)
    } else {
        format!("{milliseconds}ms")
    }
}

/// How long a while was, in the unit a person would use.
fn a_while(seconds: u64) -> String {
    match seconds {
        0..=90 => format!("{seconds} seconds"),
        91..=5_400 => format!("{} minutes", seconds / 60),
        _ => format!("{} hours", seconds / 3_600),
    }
}

/// A payload that is itself text, read as text.
fn kind_payload(payload: &[u8]) -> String {
    String::from_utf8_lossy(payload).trim().to_owned()
}

/// As much of a multi-line value as belongs on one line.
fn first_line(text: &str) -> String {
    let trimmed = text.trim();

    match trimmed.split_once('\n') {
        Some((first, _)) => format!("{} …", first.trim_end()),
        None => trimmed.to_owned(),
    }
}

impl Explain for Vocabulary {
    fn explain_payload(
        &self,
        kind: &PayloadKind,
        version: PayloadVersion,
        payload: &[u8],
    ) -> String {
        match kind.as_str() {
            // What the operator said, in the words they said it in.
            Said::KIND => match read::<Said>(kind, version, payload) {
                Ok(said) => said.text().to_owned(),
                Err(problem) => problem,
            },

            AssertBelief::KIND => match read::<AssertBelief>(kind, version, payload) {
                Ok(assertion) => {
                    let value = String::from_utf8_lossy(assertion.value().bytes());
                    let mut line = format!(
                        "{} = {value} ({} confident)",
                        assertion.key(),
                        assertion.confidence()
                    );
                    if !assertion.rationale().is_empty() {
                        line.push_str(&format!(" — {}", assertion.rationale()));
                    }
                    line
                }
                Err(problem) => problem,
            },

            // A decision to record nothing. Worth showing: it is the difference
            // between "the judge has not looked at this" and "the judge looked,
            // and there was nothing to say".
            Considered::KIND => match read::<Considered>(kind, version, payload) {
                Ok(considered) => {
                    format!("considered #{} — nothing to record", considered.event())
                }
                Err(problem) => problem,
            },

            // The other half of the judge's answer: not what the event says,
            // but that it calls for something to be done about it.
            Wake::KIND => match read::<Wake>(kind, version, payload) {
                Ok(wake) => {
                    let mut line = format!("decided this needs doing: {}", first_line(wake.task()));
                    if !wake.reason().is_empty() {
                        line.push_str(&format!(" — {}", first_line(wake.reason())));
                    }
                    line
                }
                Err(problem) => problem,
            },

            RetractBelief::KIND => match read::<RetractBelief>(kind, version, payload) {
                Ok(retraction) => format!(
                    "{} withdrawn ({} confident) — {}",
                    retraction.key(),
                    retraction.confidence(),
                    retraction.reason()
                ),
                Err(problem) => problem,
            },

            // The task, not the context: the context is thousands of characters
            // of what the model was shown, and it belongs behind a click rather
            // than in a list.
            ReasonRequest::KIND => match read::<ReasonRequest>(kind, version, payload) {
                Ok(request) => format!("asked to think about: {}", first_line(request.task())),
                Err(problem) => problem,
            },

            ReasonThought::KIND => match read::<ReasonThought>(kind, version, payload) {
                Ok(thought) => format!("thinking: {}", first_line(thought.text())),
                Err(problem) => problem,
            },

            ReasonFinished::KIND => match read::<ReasonFinished>(kind, version, payload) {
                Ok(finished) => format!(
                    "concluded after {} step{}: {}",
                    finished.steps(),
                    if finished.steps() == 1 { "" } else { "s" },
                    first_line(finished.answer())
                ),
                Err(problem) => problem,
            },

            ReasonFailed::KIND => match read::<ReasonFailed>(kind, version, payload) {
                Ok(failed) => format!(
                    "gave up after {} step{}: {}",
                    failed.steps(),
                    if failed.steps() == 1 { "" } else { "s" },
                    first_line(failed.reason())
                ),
                Err(problem) => problem,
            },

            StepRequest::KIND => match read::<StepRequest>(kind, version, payload) {
                Ok(step) => format!(
                    "wants to run {} {}",
                    step.procedure(),
                    first_line(step.arguments())
                ),
                Err(problem) => problem,
            },

            // What it did, in one line. What the command printed is the part
            // worth reading, and that is the detail.
            StepResult::KIND => match read::<StepResult>(kind, version, payload) {
                Ok(step) => {
                    // Speaking is how the system reaches a person, so what it
                    // said is the interesting part of the step — not that a
                    // procedure of that name returned. Without this, the one
                    // record carrying the system's own words reads as
                    // "operator.say succeeded, 129 bytes of output".
                    if step.procedure() == SAY_PROCEDURE && step.succeeded() {
                        return format!("told the operator: {}", first_line(step.output()));
                    }

                    // Defining something is a step like any other, and the
                    // derived event says what was defined. Saying it twice, in
                    // two vocabularies, is how the two come to disagree.
                    if step.succeeded()
                        && matches!(
                            step.procedure(),
                            DEFINE_PROCEDURE | DEFINE_OBSERVER_PROCEDURE
                        )
                    {
                        return format!("{} — accepted", step.procedure());
                    }

                    let outcome = if step.succeeded() {
                        "succeeded".to_owned()
                    } else {
                        match step.exit() {
                            Some(code) => format!("failed, exit {code}"),
                            None => "failed".to_owned(),
                        }
                    };
                    let mut line = format!(
                        "{} {outcome}, {} bytes of output",
                        step.procedure(),
                        step.output().len()
                    );
                    if step.was_truncated() {
                        line.push_str(" (truncated)");
                    }
                    line
                }
                Err(problem) => problem,
            },

            // What the system made for itself, in the words it would use.
            SkillDefined::KIND => match read::<SkillDefined>(kind, version, payload) {
                Ok(defined) => format!(
                    "defined {} — {}",
                    defined.skill().qualified(),
                    first_line(defined.skill().description())
                ),
                Err(problem) => problem,
            },

            SkillRemoved::KIND => match read::<SkillRemoved>(kind, version, payload) {
                Ok(removed) => format!("forgot the procedure {}", removed.name()),
                Err(problem) => problem,
            },

            ObserverDefined::KIND => match read::<ObserverDefined>(kind, version, payload) {
                Ok(defined) => format!(
                    "set up {} to run {}.{} every {} — {}",
                    defined.observer().name(),
                    velora_capability::DEVICE,
                    defined.observer().skill(),
                    spelled(defined.observer().every()),
                    first_line(defined.observer().description())
                ),
                Err(problem) => problem,
            },

            ObserverRemoved::KIND => match read::<ObserverRemoved>(kind, version, payload) {
                Ok(removed) => format!("stopped the source {}", removed.name()),
                Err(problem) => problem,
            },

            // A report from a source the system set up itself. Which source it
            // was is the first thing worth knowing; a report that arrived as
            // text with no source would be unattributable in a log with several.
            Observation::KIND => match read::<Observation>(kind, version, payload) {
                Ok(observation) => {
                    let how = if observation.succeeded() {
                        "reported"
                    } else {
                        "could not report"
                    };
                    format!(
                        "{} {how}: {}",
                        observation.observer(),
                        first_line(observation.text())
                    )
                }
                Err(problem) => problem,
            },

            // The system noticing that nothing has happened. Worth a line of its
            // own: it is the only record in the log whose reason for existing is
            // that nothing else does.
            Idle::KIND => match read::<Idle>(kind, version, payload) {
                Ok(idle) => format!(
                    "nothing has happened for {} — thinking on its own",
                    a_while(idle.quiet_for())
                ),
                Err(problem) => problem,
            },

            // A belief the system decided to look at again. The payload is the
            // claim key, so a page of the log reads as what it is about.
            velora_mind::RECONSIDER_KIND => {
                format!("asked itself whether {} still holds", kind_payload(payload))
            }

            // A plan, and how far through it the system is. The step in hand is
            // the part that matters: it is what the system is about to be asked
            // to do, and a log that showed only "a plan was made" would leave a
            // person unable to see what it was doing next.
            PlanCreated::KIND => match read::<PlanCreated>(kind, version, payload) {
                Ok(created) => format!(
                    "made a plan, {}: {} ({} steps)",
                    created.plan().id(),
                    first_line(created.plan().goal()),
                    created.plan().steps().len()
                ),
                Err(problem) => problem,
            },

            StepReady::KIND => match read::<StepReady>(kind, version, payload) {
                Ok(ready) => format!(
                    "step {} of {} in {} is ready: {}",
                    ready.index() + 1,
                    ready.of(),
                    ready.id(),
                    first_line(ready.step())
                ),
                Err(problem) => problem,
            },

            StepDone::KIND => match read::<StepDone>(kind, version, payload) {
                Ok(done) => format!(
                    "step {} in {} is done: {}",
                    done.index() + 1,
                    done.id(),
                    first_line(done.note())
                ),
                Err(problem) => problem,
            },

            PlanRevised::KIND => match read::<PlanRevised>(kind, version, payload) {
                Ok(revised) => format!(
                    "changed what is left of {}: {} step{} remain",
                    revised.id(),
                    revised.steps().len(),
                    if revised.steps().len() == 1 { "" } else { "s" }
                ),
                Err(problem) => problem,
            },

            PlanAbandoned::KIND => match read::<PlanAbandoned>(kind, version, payload) {
                Ok(abandoned) => format!(
                    "gave up on the plan {}: {}",
                    abandoned.id(),
                    first_line(abandoned.why())
                ),
                Err(problem) => problem,
            },

            StepRefused::KIND => match read::<StepRefused>(kind, version, payload) {
                Ok(refused) => format!("{} was refused: {}", refused.procedure(), refused.reason()),
                Err(problem) => problem,
            },

            ConfigChange::KIND => match read::<ConfigChange>(kind, version, payload) {
                Ok(change) => format!("{} set to {}", change.key(), change.value()),
                Err(problem) => problem,
            },

            ConfigRefused::KIND => match read::<ConfigRefused>(kind, version, payload) {
                Ok(refused) => format!(
                    "{} was refused: {} ({})",
                    refused.key(),
                    refused.reason(),
                    refused.value()
                ),
                Err(problem) => problem,
            },

            // Anything a source submitted as its own bytes. The runtime has no
            // decoder for it and does not pretend to: this is what it literally
            // says, which is the truth and not an interpretation.
            _ => String::from_utf8_lossy(payload).into_owned(),
        }
    }

    fn detail(&self, record: &Record) -> Option<String> {
        let (kind, version, payload) = (record.kind(), record.version(), record.payload());

        match kind.as_str() {
            // The whole situation the model was given. The most valuable thing
            // the system records: it is the difference between "it decided that"
            // and "it decided that, having been told this".
            ReasonRequest::KIND => read::<ReasonRequest>(kind, version, payload)
                .ok()
                .map(|request| request.context().to_owned()),

            // What the command actually printed.
            StepResult::KIND => read::<StepResult>(kind, version, payload)
                .ok()
                .and_then(|step| {
                    let output = step.output();
                    (!output.is_empty()).then(|| output.to_owned())
                }),

            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use velora_belief::{ClaimKey, ClaimValue, Confidence, Evidence};
    use velora_operator::Said;
    use velora_event::{
        EventLog, MemoryStore, PayloadVersion, RecordDraft, Sensitivity, Sequence, Timestamp,
    };
    use velora_exec::{StepRequest, StepResult};
    use velora_reason::{ReasonFinished, ReasonRequest, ReasonThought};

    fn said<P: Payload>(payload: &P) -> String {
        let mut log = EventLog::open(MemoryStore::new()).unwrap();
        log.append(RecordDraft::new(
            P::KIND,
            PayloadVersion::new(P::VERSION),
            payload.encode(),
            Timestamp::from_parts(1, 0),
            Sensitivity::INTERNAL,
        ))
        .unwrap();

        Vocabulary.explain(&log.replay().unwrap()[0])
    }

    fn detail_of<P: Payload>(payload: &P) -> Option<String> {
        let mut log = EventLog::open(MemoryStore::new()).unwrap();
        log.append(RecordDraft::new(
            P::KIND,
            PayloadVersion::new(P::VERSION),
            payload.encode(),
            Timestamp::from_parts(1, 0),
            Sensitivity::INTERNAL,
        ))
        .unwrap();

        Vocabulary.detail(&log.replay().unwrap()[0])
    }

    #[test]
    fn what_the_operator_said_is_read_back_in_their_own_words() {
        assert_eq!(
            said(&Said::new("what is in my home directory?")),
            "what is in my home directory?"
        );
    }

    #[test]
    fn a_belief_reads_as_a_claim_rather_than_as_bytes() {
        let assertion = AssertBelief::new(
            ClaimKey::new("operator.name"),
            ClaimValue::new(PayloadVersion::new(1), b"Sam".to_vec()),
            Confidence::from_basis_points(9_000).unwrap(),
            Evidence::one(Sequence::FIRST),
        )
        .with_rationale("the operator said so");

        let line = said(&assertion);

        assert!(line.contains("operator.name = Sam"), "{line}");
        assert!(line.contains("90.00% confident"), "{line}");
        assert!(line.contains("the operator said so"), "{line}");

        // The point of the vocabulary: none of this is legible as raw bytes.
        assert!(!line.contains('\u{0}'), "{line}");
    }

    #[test]
    fn a_thought_shows_what_was_asked_and_keeps_the_context_for_the_detail() {
        let request = ReasonRequest::new(
            "decide whether anything needs doing",
            Sequence::new(7),
            "## The task\ndecide\n\n## The conversation so far\noperator: hello",
        );

        assert_eq!(
            said(&request),
            "asked to think about: decide whether anything needs doing"
        );

        // The whole situation the model was given is available, because that is
        // the difference between "it decided that" and "it decided that, having
        // been told this".
        let detail = detail_of(&request).unwrap();
        assert!(detail.contains("## The conversation so far"), "{detail}");
        assert!(detail.contains("operator: hello"), "{detail}");
    }

    #[test]
    fn a_long_task_is_one_line_in_the_list_and_whole_in_the_detail() {
        let request = ReasonRequest::new(
            "first line\nsecond line\nthird line",
            Sequence::new(7),
            "context",
        );

        let line = said(&request);
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains("first line"), "{line}");
        assert!(line.contains('…'), "{line}");
    }

    #[test]
    fn a_finished_turn_says_how_many_steps_it_took() {
        assert_eq!(
            said(&ReasonFinished::new(Sequence::new(3), 1, "done.")),
            "concluded after 1 step: done."
        );
        assert_eq!(
            said(&ReasonFinished::new(Sequence::new(3), 4, "done.")),
            "concluded after 4 steps: done."
        );
        assert_eq!(
            said(&ReasonThought::new(Sequence::new(3), "looking\nmore", "")),
            "thinking: looking …"
        );
    }

    #[test]
    fn what_a_command_printed_is_the_detail_not_the_line() {
        let step = StepResult::from_outcome(
            Sequence::new(3),
            "local.shell.run",
            &velora_exec::Outcome::ok("lots\nof\noutput\nhere"),
        );

        let line = said(&step);
        assert!(line.contains("local.shell.run succeeded"), "{line}");
        assert!(line.contains("19 bytes of output"), "{line}");
        assert!(!line.contains('\n'), "{line}");

        assert_eq!(detail_of(&step).unwrap(), "lots\nof\noutput\nhere");
    }

    #[test]
    fn a_step_that_was_asked_for_says_what_it_would_run() {
        let step = StepRequest::new(
            Sequence::new(3),
            "local.shell.run",
            "{\"command\":\"ls\"}",
        );

        assert_eq!(
            said(&step),
            "wants to run local.shell.run {\"command\":\"ls\"}"
        );
    }

    #[test]
    fn a_payload_from_a_source_is_shown_as_what_it_literally_says() {
        let mut log = EventLog::open(MemoryStore::new()).unwrap();
        log.append(RecordDraft::new(
            "observation.note",
            PayloadVersion::new(1),
            b"theme=dark".to_vec(),
            Timestamp::from_parts(1, 0),
            Sensitivity::INTERNAL,
        ))
        .unwrap();

        let records = log.replay().unwrap();
        assert_eq!(Vocabulary.explain(&records[0]), "theme=dark");
        assert_eq!(Vocabulary.detail(&records[0]), None);
    }

    #[test]
    fn a_payload_that_cannot_be_read_says_so_rather_than_showing_garbage() {
        let mut log = EventLog::open(MemoryStore::new()).unwrap();
        log.append(RecordDraft::new(
            AssertBelief::KIND,
            // A version this build does not know how to read.
            PayloadVersion::new(99),
            vec![0xff, 0xfe, 0x00],
            Timestamp::from_parts(1, 0),
            Sensitivity::INTERNAL,
        ))
        .unwrap();

        let records = log.replay().unwrap();
        let line = Vocabulary.explain(&records[0]);

        assert!(line.contains("could not be read"), "{line}");

        // And it does not quietly fall back to the bytes, which would look like
        // a record that decoded fine and happened to be nonsense.
        assert!(!line.contains('\u{0}'), "{line}");
    }
}
