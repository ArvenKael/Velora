use serde::Serialize;
use velora_belief::{BeliefHandle, BeliefStore};
use velora_context::Explain;
use velora_event::Record;

/// One event, as the screen sees it.
///
/// The payload is not sent. It is canonical bytes, and bytes on a screen are
/// noise: what the operator needs is what the record *means*, which is the
/// runtime's vocabulary's business, not this server's. What is sent is that
/// description plus the metadata needed to place the record in time and order.
#[derive(Debug, Clone, Serialize)]
pub struct RecordView {
    /// Where it sits in history.
    pub sequence: u64,
    /// The kind of payload it holds.
    pub kind: String,
    /// The payload version, so a record written by an older build is visible as
    /// such rather than looking merely old.
    pub version: u32,
    /// Seconds since the epoch, as the source claimed.
    pub secs: i64,
    /// The sub-second part, for ordering records written in the same second.
    pub micros: u32,
    /// The handling constraint attached to it.
    pub sensitivity: String,
    /// The kind's family: the part before the first dot. The screen colours by
    /// this, so an unfamiliar family is still placed somewhere sensible.
    pub family: String,
    /// What the runtime says it means.
    pub says: String,
}

impl RecordView {
    /// Describes a record using the runtime's vocabulary.
    pub fn of(record: &Record, explain: &dyn Explain) -> Self {
        let kind = record.kind().as_str().to_owned();

        RecordView {
            sequence: record.sequence().get(),
            family: kind.split('.').next().unwrap_or(&kind).to_owned(),
            kind,
            version: record.version().get(),
            secs: record.time().secs(),
            micros: record.time().nanos() / 1_000,
            sensitivity: record.sensitivity().to_string(),
            says: explain.explain(record),
        }
    }
}

/// How much of a belief's evidence is sent.
///
/// A belief accumulates evidence for as long as it keeps being reinforced, which
/// is unbounded and is rather the point of one. Sending all of it on every
/// change would make the screen cost more the longer the system had been
/// running, so the newest are sent and the total is reported alongside. Nothing
/// is hidden: the count is what says how much stands behind a belief, and the
/// newest are the ones a reader would want to look at.
const EVIDENCE_SENT: usize = 12;

/// One belief, as the screen sees it.
#[derive(Debug, Clone, Serialize)]
pub struct BeliefView {
    /// What is believed about.
    pub key: String,
    /// What is believed, as text.
    pub value: String,
    /// How strongly, already formatted, so the screen does not have to know
    /// what a basis point is.
    pub confidence: String,
    /// Whether something contradicts it.
    pub contested: bool,
    /// Why it is held.
    pub rationale: String,
    /// The event that first established it.
    pub since: u64,
    /// The most recent events it was drawn from, so a reader can go and look.
    pub evidence: Vec<u64>,
    /// How many there are altogether, which is never all sent.
    pub evidence_total: usize,
}

/// Everything the screen knows about the system it is watching.
#[derive(Debug, Clone, Serialize)]
pub struct About {
    /// What this is.
    pub system: String,
    /// Which backend holds history, and where.
    pub store: String,
    /// What that backend promises about a record once it has been appended.
    pub durability: String,
    /// The model, or nothing if the system is running without one.
    pub model: Option<Model>,
    /// The stages events pass through, in order.
    pub pipeline: Vec<String>,
    /// What the system can do to this computer.
    pub procedures: Vec<String>,
    /// How it has been told to think of itself, if it has been told.
    pub persona: Option<String>,
}

/// The model being thought with.
#[derive(Debug, Clone, Serialize)]
pub struct Model {
    /// Which provider.
    pub provider: String,
    /// Which model.
    pub model: String,
    /// Whether it can stream increments.
    pub streaming: bool,
    /// Whether it can call tools.
    pub tools: bool,
}

/// What the system has been doing, in numbers.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Stats {
    /// How many records history holds.
    pub records: u64,
    /// How many beliefs are held.
    pub beliefs: usize,
    /// How many thoughts are open — greater than zero means it is thinking.
    pub turns_open: i64,
    /// Whether the timeline sent to the screen omits older records.
    pub truncated: bool,
}

/// Everything sent when the page loads.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// What the system is.
    pub about: About,
    /// Recent events, oldest first.
    pub records: Vec<RecordView>,
    /// What has been said, oldest first, with the system's own words rather than
    /// a summary of the step that said them. Derived from two kinds of ordinary
    /// record — there is no conversation stored anywhere — and sent separately
    /// from the timeline because it is what a person reads, where the timeline
    /// is what they scan.
    pub dialogue: Vec<Line>,
    /// What is believed.
    pub beliefs: Vec<BeliefView>,
    /// What has been happening.
    pub stats: Stats,
}

/// A batch of new events, as they are pushed to the screen.
#[derive(Debug, Clone, Serialize)]
pub struct Records {
    /// The new records, oldest first.
    pub records: Vec<RecordView>,
    /// What was said, if any of it was. Read from the same batch, so the screen
    /// and the model are told the same thing by the same code.
    pub dialogue: Vec<Line>,
    /// The numbers after them.
    pub stats: Stats,
}

/// The beliefs, whole.
///
/// Sent whole rather than as a difference: beliefs are few, and a screen that
/// has to apply differences to a list it may have missed the start of is a
/// screen that will eventually disagree with the system it is describing.
#[derive(Debug, Clone, Serialize)]
pub struct Beliefs {
    /// Every belief held.
    pub beliefs: Vec<BeliefView>,
}

/// One thing that was said, as the screen shows it.
#[derive(Debug, Clone, Serialize)]
pub struct Line {
    /// The record it came from.
    pub sequence: u64,
    /// `operator` or `velora`.
    pub who: &'static str,
    /// What was said.
    pub text: String,
    /// The record's kind, so the screen can still say where it came from.
    pub kind: String,
}

/// Reads what has been said out of history.
///
/// The operator's words are an event they submitted; the system's are what a
/// step of speaking reported. Neither is stored as a conversation, and this does
/// not store one either.
pub fn dialogue(records: &[Record]) -> Vec<Line> {
    velora_operator::Transcript::read(records)
        .utterances()
        .iter()
        .map(|utterance| Line {
            sequence: utterance.sequence(),
            who: match utterance.speaker() {
                velora_operator::Speaker::Operator => "operator",
                velora_operator::Speaker::System => "velora",
            },
            text: utterance.text().to_owned(),
            kind: records
                .iter()
                .find(|record| record.sequence().get() == utterance.sequence())
                .map_or_else(String::new, |record| record.kind().as_str().to_owned()),
        })
        .collect()
}

/// One record, in full.
#[derive(Debug, Clone, Serialize)]
pub struct Detail {
    /// The record.
    pub record: RecordView,
    /// The whole of it, when one line was not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Reads the beliefs out of a handle.
pub fn beliefs(handle: &BeliefHandle) -> Vec<BeliefView> {
    beliefs_of(&handle.snapshot())
}

/// Reads the beliefs out of a store.
///
/// Separate from the handle so that what the screen is sent about a belief can be
/// checked against a store built by hand, rather than only against one a running
/// judge happened to produce.
pub fn beliefs_of(store: &BeliefStore) -> Vec<BeliefView> {
    store
        .iter()
        .map(|belief| BeliefView {
            key: belief.key().as_str().to_owned(),
            value: String::from_utf8_lossy(belief.value().bytes()).into_owned(),
            confidence: belief.confidence().to_string(),
            contested: store.is_contested(belief.key()),
            rationale: belief.rationale().to_owned(),
            since: belief.established_at().get(),
            evidence: belief
                .evidence()
                .sequences()
                .iter()
                .skip(
                    belief
                        .evidence()
                        .sequences()
                        .len()
                        .saturating_sub(EVIDENCE_SENT),
                )
                .map(|sequence| sequence.get())
                .collect(),
            evidence_total: belief.evidence().sequences().len(),
        })
        .collect()
}

/// How many thoughts history leaves open.
///
/// Counted rather than asked, because the count is derivable from the record and
/// a screen that asks the resolver would be asking a moving thing.
pub fn open_turns(records: &[Record]) -> i64 {
    let mut open = 0i64;

    for record in records {
        match record.kind().as_str() {
            "reason.requested" => open += 1,
            "reason.finished" | "reason.failed" => open -= 1,
            _ => {}
        }
    }

    open.max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use velora_belief::{Belief, ClaimKey, ClaimValue, Confidence, Evidence};
    use velora_event::{PayloadVersion, Sequence};

    fn belief_with(evidence: &[u64]) -> Belief {
        Belief::new(
            ClaimKey::new("operator.name"),
            ClaimValue::new(PayloadVersion::new(1), b"Sam".to_vec()),
            Confidence::from_basis_points(9_000).unwrap(),
            Evidence::new(evidence.iter().copied().map(Sequence::new)).unwrap(),
            Sequence::new(evidence.iter().copied().max().unwrap_or(0) + 1),
        )
        .with_rationale("the operator said so")
    }

    #[test]
    fn a_belief_is_described_the_way_the_screen_shows_it() {
        let mut store = BeliefStore::new();
        store.assert(belief_with(&[3, 5, 9])).unwrap();

        let views = beliefs_of(&store);
        assert_eq!(views.len(), 1);

        let view = &views[0];
        assert_eq!(view.key, "operator.name");
        assert_eq!(view.value, "Sam");
        assert_eq!(view.confidence, "90.00%");
        assert_eq!(view.rationale, "the operator said so");
        assert_eq!(view.since, 10);
        assert_eq!(view.evidence, vec![3, 5, 9]);
        assert_eq!(view.evidence_total, 3);
        assert!(!view.contested);
    }

    #[test]
    fn a_belief_with_unbounded_evidence_is_still_bounded_on_the_wire() {
        // This is the shape that broke the screen: the demo reasserts the same
        // belief every second, and a belief gathers evidence for as long as it
        // keeps being reinforced. Sending all of it would make the screen cost
        // more the longer the system ran.
        let many: Vec<u64> = (1..=400).collect();
        let mut store = BeliefStore::new();
        store.assert(belief_with(&many)).unwrap();

        let view = &beliefs_of(&store)[0];

        assert_eq!(view.evidence.len(), EVIDENCE_SENT);
        assert_eq!(
            view.evidence,
            many[many.len() - EVIDENCE_SENT..].to_vec(),
            "the newest are the ones worth showing"
        );

        // And nothing is hidden: the count is what says how much stands behind
        // the belief.
        assert_eq!(view.evidence_total, 400);
    }

    #[test]
    fn a_belief_with_less_evidence_than_fits_sends_all_of_it() {
        let mut store = BeliefStore::new();
        store.assert(belief_with(&[1, 2, 3])).unwrap();

        let view = &beliefs_of(&store)[0];
        assert_eq!(view.evidence.len(), 3);
        assert_eq!(view.evidence_total, 3);
    }

    #[test]
    fn open_thoughts_are_counted_from_the_records_alone() {
        fn record(kind: &str) -> velora_event::Record {
            let mut log = velora_event::EventLog::open(velora_event::MemoryStore::new()).unwrap();
            log.append(velora_event::RecordDraft::new(
                kind,
                PayloadVersion::new(1),
                Vec::new(),
                velora_event::Timestamp::from_parts(1, 0),
                velora_event::Sensitivity::INTERNAL,
            ))
            .unwrap();
            log.replay().unwrap().remove(0)
        }

        assert_eq!(open_turns(&[]), 0);
        assert_eq!(open_turns(&[record("reason.requested")]), 1);
        assert_eq!(
            open_turns(&[record("reason.requested"), record("reason.finished")]),
            0
        );
        assert_eq!(
            open_turns(&[record("reason.requested"), record("reason.failed")]),
            0
        );
        // A finished turn with no request against it cannot make the count go
        // negative, which would show as a system that had thought a negative
        // number of thoughts.
        assert_eq!(open_turns(&[record("reason.finished")]), 0);
    }
}
