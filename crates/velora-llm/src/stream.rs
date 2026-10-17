use std::fmt;

use tokio::sync::mpsc;

use crate::message::{Response, StopReason, ToolCall, Usage};
use crate::provider::ProviderError;

/// One increment of a streamed answer.
///
/// Text arrives in fragments, and so do tool calls: a model may start a call in
/// one chunk and finish spelling out its arguments several chunks later, which
/// is why a call is identified by `index` until it has an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// A fragment of the reply text.
    Text(String),
    /// The model began calling a tool.
    ToolCallStarted {
        /// Position of this call in the response.
        index: usize,
        /// The call id, possibly empty until the provider sends it.
        id: String,
        /// The tool name, possibly empty until the provider sends it.
        name: String,
    },
    /// A fragment of a tool call's JSON arguments.
    ToolCallArguments {
        /// Position of this call in the response.
        index: usize,
        /// The raw fragment, exactly as it arrived.
        fragment: String,
    },
    /// The answer is complete.
    Finished {
        /// The model that answered, when the provider said.
        model: Option<String>,
        /// Why it stopped.
        stop_reason: Option<StopReason>,
        /// Token counts, if the provider reported them.
        usage: Option<Usage>,
    },
}

/// A streamed answer in progress.
///
/// Dropping the stream abandons the request: the provider task notices its
/// receiver is gone and stops. That is the intended way to cancel.
pub struct CompletionStream {
    events: mpsc::Receiver<Result<StreamEvent, ProviderError>>,
}

impl fmt::Debug for CompletionStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CompletionStream")
    }
}

impl CompletionStream {
    /// Wraps the provider's event channel.
    pub(crate) fn new(events: mpsc::Receiver<Result<StreamEvent, ProviderError>>) -> Self {
        CompletionStream { events }
    }

    /// The next increment, or `None` once the answer is complete.
    ///
    /// An error here is the end of the stream: a half-read answer cannot be
    /// resumed, and pretending otherwise would hand the caller a silently
    /// truncated thought.
    pub async fn next(&mut self) -> Option<Result<StreamEvent, ProviderError>> {
        self.events.recv().await
    }

    /// Reads the whole answer into one response.
    ///
    /// This is how streaming and non-streaming stay the same thing: a caller
    /// that does not care about increments collects, and a caller that does
    /// watches them arrive.
    pub async fn collect(mut self) -> Result<Response, ProviderError> {
        let mut text = String::new();
        let mut calls: Vec<PartialCall> = Vec::new();
        let mut model: Option<String> = None;
        let mut stop_reason: Option<StopReason> = None;
        let mut usage: Option<Usage> = None;

        while let Some(event) = self.next().await {
            match event? {
                StreamEvent::Text(fragment) => text.push_str(&fragment),
                StreamEvent::ToolCallStarted { index, id, name } => {
                    let call = slot(&mut calls, index);
                    if !id.is_empty() {
                        call.id = id;
                    }
                    if !name.is_empty() {
                        call.name = name;
                    }
                }
                StreamEvent::ToolCallArguments { index, fragment } => {
                    slot(&mut calls, index).arguments.push_str(&fragment);
                }
                StreamEvent::Finished {
                    model: answered_by,
                    stop_reason: reason,
                    usage: tokens,
                } => {
                    model = answered_by;
                    stop_reason = reason;
                    usage = tokens;
                }
            }
        }

        Ok(Response::new(
            (!text.is_empty()).then_some(text),
            calls
                .into_iter()
                .map(|call| ToolCall::new(call.id, call.name, call.arguments))
                .collect(),
            model.unwrap_or_default(),
            stop_reason,
            usage,
        ))
    }
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// The slot for a tool call, growing the list as indices arrive out of order.
fn slot(calls: &mut Vec<PartialCall>, index: usize) -> &mut PartialCall {
    while calls.len() <= index {
        calls.push(PartialCall::default());
    }
    &mut calls[index]
}
