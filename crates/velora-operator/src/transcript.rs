use velora_event::{Payload, Record};
use velora_exec::StepResult;

use crate::device::SAY_PROCEDURE;
use crate::payload::Said;

/// Who said something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    /// The person.
    Operator,
    /// The system.
    System,
}

impl Speaker {
    /// What to call them when showing the dialogue to the model.
    ///
    /// "you" for the system on purpose. The model reading this is the system;
    /// calling its own earlier words "the assistant" would make it read its own
    /// speech as somebody else's, which is exactly the confusion that turns a
    /// dialogue into a series of unrelated requests.
    pub const fn label(self) -> &'static str {
        match self {
            Speaker::Operator => "the operator",
            Speaker::System => "you",
        }
    }

    /// A short name for a transcript line.
    pub const fn line_label(self) -> &'static str {
        match self {
            Speaker::Operator => "operator",
            Speaker::System => "you",
        }
    }
}

/// One thing that was said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utterance {
    sequence: u64,
    speaker: Speaker,
    text: String,
    /// Set when the record could not be read back, so that a reader can tell a
    /// real line from a placeholder standing in for a broken one.
    unreadable: bool,
}

impl Utterance {
    /// Where in history it was said.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Who said it.
    pub fn speaker(&self) -> Speaker {
        self.speaker
    }

    /// What was said.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether this stands in for a record that could not be decoded.
    pub fn is_unreadable(&self) -> bool {
        self.unreadable
    }
}

/// What has been said, in the order it was said.
///
/// Derived from history, never stored as its own kind of thing. There is no
/// conversation record: the operator's words are an event, and the system's
/// words are what a step of speaking reported. Both are ordinary records that
/// the rest of the system reads for its own reasons, and this is one reading of
/// them — the one a person would recognise as a dialogue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transcript {
    utterances: Vec<Utterance>,
    unreadable: usize,
}

impl Transcript {
    /// Reads what has been said out of history.
    ///
    /// Records that are neither are skipped, which is most of them. A record
    /// that cannot be decoded is *kept* and marked: dropping it would silently
    /// shorten the dialogue the model is shown, and a model reasoning from a
    /// dialogue with a hole in it has no way to know.
    pub fn read(history: &[Record]) -> Transcript {
        let mut utterances = Vec::new();
        let mut unreadable = 0;

        for record in history {
            let read = match record.kind().as_str() {
                Said::KIND => Said::decode(record.version(), record.payload())
                    .map(|said| (Speaker::Operator, said.text().to_owned())),
                StepResult::KIND => {
                    // The system speaks by running the channel, so what it said
                    // is what that step reported. A step that failed said
                    // nothing, and is not a line of dialogue.
                    match StepResult::decode(record.version(), record.payload()) {
                        Ok(result) if result.procedure() == SAY_PROCEDURE && result.succeeded() => {
                            Ok((Speaker::System, result.output().to_owned()))
                        }
                        _ => continue,
                    }
                }
                _ => continue,
            };

            let (speaker, text, broken) = match read {
                Ok((speaker, text)) => (speaker, text, false),
                Err(err) => {
                    unreadable += 1;
                    (
                        Speaker::Operator,
                        format!("(this record could not be read: {err})"),
                        true,
                    )
                }
            };

            utterances.push(Utterance {
                sequence: record.sequence().get(),
                speaker,
                text,
                unreadable: broken,
            });
        }

        Transcript {
            utterances,
            unreadable,
        }
    }

    /// Everything that was said.
    pub fn utterances(&self) -> &[Utterance] {
        &self.utterances
    }

    /// How many records that were said could not be read back.
    ///
    /// Reported rather than absorbed: a non-zero count means history holds
    /// something that cannot be shown, which is a defect in an encoding, not a
    /// gap in the record.
    pub fn unreadable(&self) -> usize {
        self.unreadable
    }

    /// Whether anything was said at all.
    pub fn is_empty(&self) -> bool {
        self.utterances.is_empty()
    }

    /// How much was said.
    pub fn len(&self) -> usize {
        self.utterances.len()
    }

    /// The last thing said, if anything was.
    pub fn last(&self) -> Option<&Utterance> {
        self.utterances.last()
    }

    /// The most recent `limit` utterances, oldest first.
    pub fn recent(&self, limit: usize) -> &[Utterance] {
        let start = self.utterances.len().saturating_sub(limit);
        &self.utterances[start..]
    }

    /// The utterance at a sequence, if it was one.
    pub fn at(&self, sequence: u64) -> Option<&Utterance> {
        self.utterances
            .iter()
            .find(|utterance| utterance.sequence == sequence)
    }
}
