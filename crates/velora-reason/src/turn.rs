use std::collections::VecDeque;

use velora_event::{Payload, Record, Sequence};
use velora_llm::{Message, ToolCall};

use crate::payload::{ReasonRequest, ReasonThought};
use velora_exec::{StepRefused, StepRequest, StepResult};

/// One turn of thought, as a conversation.
///
/// A turn is not a conversation held in memory and hoped for: it is rebuilt
/// from the events that belong to it, in order. That is what lets a turn
/// interrupted by a crash continue where it left off, and what lets anyone
/// afterwards read exactly what the model was told and what it said.
///
/// The tool-call ids are derived from the turn and the step's ordinal rather
/// than from anything random or anything the model chose, so a rebuilt turn has
/// the same ids as the live one did. Without that, a resumed turn would be a
/// different conversation wearing the same name.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    id: Sequence,
    task: String,
    focus: Option<Sequence>,
    messages: Vec<Message>,
    /// Calls made and not yet answered, in the order they were made.
    open_calls: VecDeque<String>,
    steps: u32,
}

impl Turn {
    /// Opens a turn with the context it was given.
    pub fn new(id: Sequence, request: &ReasonRequest) -> Self {
        Turn {
            id,
            task: request.task().to_owned(),
            focus: request.focus(),
            messages: vec![Message::user(request.context().to_owned())],
            open_calls: VecDeque::new(),
            steps: 0,
        }
    }

    /// This turn's identity: the sequence of the request that opened it.
    pub fn id(&self) -> Sequence {
        self.id
    }

    /// What it is thinking about.
    pub fn task(&self) -> &str {
        &self.task
    }

    /// The event that prompted it.
    pub fn focus(&self) -> Option<Sequence> {
        self.focus
    }

    /// How many steps it has taken.
    pub fn steps(&self) -> u32 {
        self.steps
    }

    /// How many steps it is still waiting on.
    pub fn outstanding(&self) -> usize {
        self.open_calls.len()
    }

    /// The conversation so far.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Records one response from the model: what it said, what it thought, and
    /// whatever it asked for.
    ///
    /// **One message.** That is what the model produced, and it is the only
    /// shape a provider will read:
    ///
    /// - a `tool_calls` message must be followed by the answers to *its* calls,
    ///   so two messages each asking for one thing cannot both be answered;
    /// - and the chain of thought belongs on the message that asked, because a
    ///   provider in thinking mode refuses a later request that does not carry
    ///   it back.
    ///
    /// Both of those were found the hard way, one 400 each.
    pub fn answered(
        &mut self,
        content: Option<&str>,
        reasoning: Option<&str>,
        calls: &[(String, String)],
    ) -> Vec<ToolCall> {
        let mut made = Vec::with_capacity(calls.len());

        for (procedure, arguments) in calls {
            self.steps += 1;
            let id = self.call_id(self.steps);
            self.open_calls.push_back(id.clone());
            made.push(ToolCall::new(id, procedure, arguments));
        }

        self.messages.push(Message::assistant_response(
            content.map(str::to_owned),
            reasoning.map(str::to_owned),
            made.clone(),
        ));

        made
    }

    /// Records that one call was answered without being run.
    ///
    /// By id, not by order: a refusal is settled the moment it happens, while
    /// earlier calls in the same message may still be running, and answering
    /// the wrong one would leave the conversation with two calls and one answer.
    pub fn refuse(&mut self, call_id: &str, answer: &str) {
        if let Some(at) = self.open_calls.iter().position(|id| id == call_id) {
            self.open_calls.remove(at);
            self.messages
                .push(Message::tool_result(call_id.to_owned(), answer));
        }
    }

    /// Records that a step it asked for has been answered.
    ///
    /// Paired by order: steps are run in the order they were asked for, so the
    /// Nth answer belongs to the Nth call still waiting. Calls settled without
    /// running — a refusal — are taken out of that queue when they happen.
    pub fn complete(&mut self, output: &str) {
        if let Some(id) = self.open_calls.pop_front() {
            self.messages.push(Message::tool_result(id, output));
        }
    }

    /// Answers any call that never came back.
    ///
    /// A provider refuses a conversation in which an assistant message asks for
    /// tool calls and no tool message answers them, and it refuses it with a 400
    /// that says nothing about where the hole came from. A step whose result
    /// never arrived leaves exactly that shape — the process stopped, the
    /// executor never answered — and the turn is read back from history on the
    /// next start, so the refusal lands on a resumed turn and looks like the
    /// model misbehaving.
    ///
    /// What is said is what is known: nothing was recorded. Saying so is better
    /// than dropping the call, which would leave the model's own message
    /// unexplained, and better than inventing an outcome.
    pub fn seal(&mut self) {
        while let Some(id) = self.open_calls.pop_front() {
            self.messages.push(Message::tool_result(
                id,
                "(no result was recorded for this step)",
            ));
        }
    }

    /// The id a step of this turn is known by.
    fn call_id(&self, ordinal: u32) -> String {
        format!("call-{}-{ordinal}", self.id.get())
    }

    /// Rebuilds a turn from the events that belong to it.
    ///
    /// The same fold produces the same conversation as the live one had, which
    /// is the property that makes resuming a turn honest rather than
    /// approximate.
    pub fn replay(id: Sequence, request: &ReasonRequest, records: &[Record]) -> Turn {
        let mut turn = Turn::new(id, request);
        // One response's worth, waiting to be written as one message.
        let mut batch = Response::default();

        for record in records {
            match record.kind().as_str() {
                // A response begins here: what the model said and thought. The
                // calls it asked for are the records that follow.
                ReasonThought::KIND => {
                    if let Ok(thought) = ReasonThought::decode(record.version(), record.payload())
                        && thought.turn() == id
                    {
                        flush(&mut turn, &mut batch);
                        batch.content = Some(thought.text().to_owned());
                        batch.reasoning = Some(thought.reasoning().to_owned());
                    }
                }
                // A run of calls with no answer between them was one response:
                // the model asked for all of them at once, and the loop only
                // asks the model again once every outstanding step has come
                // back. They have to be rebuilt as one message, because that is
                // what was sent.
                StepRequest::KIND => {
                    if let Ok(step) = StepRequest::decode(record.version(), record.payload())
                        && step.turn() == id
                    {
                        // The record holds the procedure's own dotted name, which
                        // is what a person reads; the conversation holds the name
                        // the model was offered it under.
                        batch.calls.push((
                            crate::protocol::wire_name(step.procedure()),
                            step.arguments().to_owned(),
                            None,
                        ));
                    }
                }
                // A refusal is an answer too, and it belongs to the same
                // response as the calls around it: the model asked for it and
                // was told no, in one message.
                StepRefused::KIND => {
                    if let Ok(refused) = StepRefused::decode(record.version(), record.payload())
                        && refused.turn() == id
                    {
                        batch.calls.push((
                            crate::protocol::wire_name(refused.procedure()),
                            String::new(),
                            Some(format!("refused: {}", refused.reason())),
                        ));
                    }
                }
                StepResult::KIND => {
                    if let Ok(result) = StepResult::decode(record.version(), record.payload())
                        && result.turn() == id
                    {
                        flush(&mut turn, &mut batch);
                        turn.complete(result.output());
                    }
                }
                _ => {}
            }
        }

        flush(&mut turn, &mut batch);

        // History can end mid-step. What is rebuilt still has to be something a
        // provider will accept.
        turn.seal();
        turn
    }
}

/// One response, as it is put back together from history.
#[derive(Default)]
struct Response {
    content: Option<String>,
    reasoning: Option<String>,
    calls: Vec<(String, String, Option<String>)>,
}

impl Response {
    fn is_empty(&self) -> bool {
        self.calls.is_empty() && self.content.is_none() && self.reasoning.is_none()
    }
}

/// Writes a response into the conversation.
///
/// One assistant message holding what it said, what it thought, and every call
/// it made — then an answer for each call that was settled without being run.
/// Calls still open are left for their results, which arrive in the order they
/// were asked for.
fn flush(turn: &mut Turn, batch: &mut Response) {
    if batch.is_empty() {
        return;
    }

    let calls: Vec<(String, String)> = batch
        .calls
        .iter()
        .map(|(name, arguments, _)| (name.clone(), arguments.clone()))
        .collect();

    let made = turn.answered(batch.content.as_deref(), batch.reasoning.as_deref(), &calls);

    for ((_, _, refused), call) in batch.calls.drain(..).zip(&made) {
        if let Some(answer) = refused {
            turn.refuse(call.id(), &answer);
        }
    }

    batch.content = None;
    batch.reasoning = None;
}


#[cfg(test)]
mod tests {
    use super::*;
    use velora_event::{
        EventLog, MemoryStore, PayloadVersion, RecordDraft, Sensitivity, Timestamp,
    };
    use crate::payload::ReasonThought;
    use velora_exec::{StepRequest, StepResult};

    fn record<P: Payload>(kind: &str, payload: &P) -> RecordDraft {
        RecordDraft::new(
            kind,
            PayloadVersion::new(P::VERSION),
            payload.encode(),
            Timestamp::from_parts(1, 0),
            Sensitivity::INTERNAL,
        )
    }

    /// Rebuilds a turn from drafts, in order.
    fn replay_of(drafts: Vec<RecordDraft>) -> Turn {
        let mut log = EventLog::open(MemoryStore::new()).unwrap();
        for draft in drafts {
            log.append(draft).unwrap();
        }
        let records = log.replay().unwrap();

        let request = ReasonRequest::new("answer them", Sequence::FIRST, "the context");
        let opened = Sequence::new(2);
        Turn::replay(opened, &request, &records)
    }

    fn request() -> RecordDraft {
        record(
            ReasonRequest::KIND,
            &ReasonRequest::new("answer them", Sequence::FIRST, "the context"),
        )
    }

    /// Every assistant message asking for calls, and the answers it got.
    ///
    /// A provider requires each such message to be followed by the tool messages
    /// answering its own calls. This returns the ids that were not.
    fn unanswered(turn: &Turn) -> Vec<String> {
        let mut open: Vec<String> = Vec::new();
        let mut orphaned = Vec::new();

        for message in turn.messages() {
            match message.role() {
                velora_llm::Role::Assistant if !message.tool_calls().is_empty() => {
                    orphaned.append(&mut open);
                    open = message
                        .tool_calls()
                        .iter()
                        .map(|call| call.id().to_owned())
                        .collect();
                }
                velora_llm::Role::Tool => {
                    let id = message.tool_call_id().unwrap_or("").to_owned();
                    open.retain(|each| each != &id);
                }
                _ => {}
            }
        }

        orphaned.extend(open);
        orphaned
    }

    #[test]
    fn the_thinking_that_came_with_a_response_stays_with_it() {
        // A provider in thinking mode refuses a later request that does not
        // carry the reasoning it produced, so it is part of the message rather
        // than something read and dropped.
        let mut turn = Turn::new(
            Sequence::new(2),
            &ReasonRequest::new("answer them", Sequence::FIRST, "the context"),
        );

        turn.answered(
            Some("Let me look."),
            Some("they asked about the lamp; I should check"),
            &[("local_fs_list".to_owned(), "{}".to_owned())],
        );

        let asking = turn
            .messages()
            .iter()
            .find(|message| !message.tool_calls().is_empty())
            .expect("a message asking for something");

        assert_eq!(asking.content(), Some("Let me look."));
        assert_eq!(
            asking.reasoning(),
            Some("they asked about the lamp; I should check")
        );
        assert_eq!(asking.tool_calls().len(), 1, "and it is one message");
    }

    #[test]
    fn thinking_survives_being_rebuilt_from_history() {
        // A turn is its events, so the thinking has to be in them: a resumed
        // turn that had lost it would be refused by the provider on its very
        // next request.
        let turn = replay_of(vec![
            request(),
            record(
                ReasonThought::KIND,
                &ReasonThought::new(
                    Sequence::new(2),
                    "Let me look.",
                    "they asked about the lamp",
                ),
            ),
            record(
                StepRequest::KIND,
                &StepRequest::new(Sequence::new(2), "local.fs.list", "{}"),
            ),
            record(
                StepResult::KIND,
                &StepResult::from_outcome(
                    Sequence::new(2),
                    "local.fs.list",
                    &velora_exec::Outcome::ok("scripts/"),
                ),
            ),
        ]);

        let asking = turn
            .messages()
            .iter()
            .find(|message| !message.tool_calls().is_empty())
            .expect("a message asking for something");

        assert_eq!(asking.reasoning(), Some("they asked about the lamp"));
        assert_eq!(asking.content(), Some("Let me look."));
        assert!(unanswered(&turn).is_empty());
    }

    #[test]
    fn a_step_that_was_answered_leaves_nothing_open() {
        let turn = replay_of(vec![
            request(),
            record(
                StepRequest::KIND,
                &StepRequest::new(Sequence::new(2), "local.shell.run", "{}"),
            ),
            record(
                StepResult::KIND,
                &StepResult::from_outcome(
                    Sequence::new(2),
                    "local.shell.run",
                    &velora_exec::Outcome::ok("done"),
                ),
            ),
        ]);

        assert!(unanswered(&turn).is_empty());
    }

    #[test]
    fn a_step_whose_result_never_came_is_sealed_rather_than_left_open() {
        // This is what a resumed turn looked like to DeepSeek: an assistant
        // message asking for a tool call, and no tool message answering it. The
        // provider refused the whole request, and the refusal arrived as "the
        // thought could not go on" with a 400 about tool_call_id.
        let turn = replay_of(vec![
            request(),
            record(
                StepRequest::KIND,
                &StepRequest::new(Sequence::new(2), "local.shell.run", "{}"),
            ),
        ]);

        assert!(
            unanswered(&turn).is_empty(),
            "a resumed turn must be a conversation a provider will accept"
        );

        let last = turn.messages().last().expect("a message");
        assert_eq!(last.role(), velora_llm::Role::Tool);
        assert!(
            last.content().unwrap_or("").contains("no result was recorded"),
            "{:?}",
            last.content()
        );
    }
}
