use std::collections::BTreeSet;

use velora_event::{Payload, Record, Sequence};
use velora_operator::Said;
use velora_reason::ReasonRequest;

/// Events of a kind the system thinks about, that no thought was opened for.
///
/// Nothing to do with conversations: an event a rule says is worth acting on,
/// and that was never acted on. The process stopped between the event arriving
/// and the system noticing, or the system was busy and then stopped. The event
/// is durable, so the work is recoverable rather than lost — and leaving it
/// would be an event that was silently ignored.
///
/// An event that *has* a request against it is not here: that turn is open, and
/// the reasoner resumes it rather than opening a second one about the same thing.
pub fn never_acted_on(records: &[Record], kinds: &[String]) -> Vec<Sequence> {
    let mut waiting: Vec<Sequence> = Vec::new();
    let mut taken_up: BTreeSet<Sequence> = BTreeSet::new();

    for record in records {
        let kind = record.kind().as_str();

        if kind == ReasonRequest::KIND {
            if let Ok(request) = ReasonRequest::decode(record.version(), record.payload())
                && let Some(focus) = request.focus()
            {
                taken_up.insert(focus);
            }
            continue;
        }

        if !kinds.iter().any(|watched| watched == kind) {
            continue;
        }

        // Something said with nothing in it is not something to act on, and
        // thinking about it on every start would be a model call spent on
        // nothing, forever.
        if kind == Said::KIND
            && Said::decode(record.version(), record.payload()).is_ok_and(|said| said.is_empty())
        {
            continue;
        }

        waiting.push(record.sequence());
    }

    waiting
        .into_iter()
        .filter(|sequence| !taken_up.contains(sequence))
        .collect()
}
