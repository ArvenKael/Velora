use std::fmt;

use serde_json::Value;

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Instructions that frame the whole conversation.
    System,
    /// Something a person or an upstream stage said.
    User,
    /// Something the model said.
    Assistant,
    /// The result of a tool the model asked for.
    Tool,
}

impl Role {
    /// The name this role goes by on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A model's request to call a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCall {
    /// Builds a tool call.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// The id used to match this call with its result.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The tool being called.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The arguments as the model produced them, still text.
    ///
    /// Kept as text rather than a parsed value because that is how they arrive
    /// — streamed in fragments — and because re-encoding a parsed value is not
    /// always the same bytes the model actually sent.
    pub fn arguments(&self) -> &str {
        &self.arguments
    }

    /// The arguments parsed as JSON.
    ///
    /// # Errors
    ///
    /// Fails if the model produced arguments that are not valid JSON, which is
    /// a thing models do.
    pub fn arguments_json(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.arguments)
    }
}

/// A tool the model is allowed to call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    name: String,
    description: String,
    parameters: Value,
}

impl Tool {
    /// Describes a tool whose parameters are given as JSON Schema.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Tool {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    /// Describes a tool that takes no arguments.
    pub fn without_parameters(name: impl Into<String>, description: impl Into<String>) -> Self {
        Tool::new(
            name,
            description,
            serde_json::json!({ "type": "object", "properties": {} }),
        )
    }

    /// The tool's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the tool does, in the model's words.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The JSON Schema for the tool's arguments.
    pub fn parameters(&self) -> &Value {
        &self.parameters
    }
}

/// One turn of a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    role: Role,
    content: Option<String>,
    /// The model's chain of thought, when it thought.
    ///
    /// Not shown to anyone and not part of the conversation as prose: DeepSeek's
    /// thinking mode returns it beside the content and **requires it back** on
    /// every later request that carries tools, or the request is refused with a
    /// 400. So it is carried rather than dropped.
    reasoning: Option<String>,
    tool_calls: Vec<ToolCall>,
    tool_call_id: Option<String>,
}

impl Message {
    /// A system message.
    pub fn system(text: impl Into<String>) -> Self {
        Message {
            role: Role::System,
            content: Some(text.into()),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// A user message.
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: Some(text.into()),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant message.
    pub fn assistant(text: impl Into<String>) -> Self {
        Message {
            role: Role::Assistant,
            content: Some(text.into()),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant message that only asks for tools.
    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Message {
            role: Role::Assistant,
            content: None,
            reasoning: None,
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Everything the model said in one response.
    ///
    /// One message, because that is what it was: the words it chose to say, the
    /// thinking it did first, and whatever it asked for. Splitting them apart
    /// loses the connection between them — and a provider that requires the
    /// thinking back will not accept it on a message that asked for nothing.
    pub fn assistant_response(
        content: Option<String>,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Message {
            role: Role::Assistant,
            content: content.filter(|text| !text.trim().is_empty()),
            reasoning: reasoning.filter(|text| !text.trim().is_empty()),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// The result of a tool the model asked for.
    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Message {
            role: Role::Tool,
            content: Some(text.into()),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }

    /// Who produced this message.
    pub fn role(&self) -> Role {
        self.role
    }

    /// The message text, absent when a model replied only with tool calls.
    pub fn content(&self) -> Option<&str> {
        self.content.as_deref()
    }

    /// The thinking behind this message, when there was any.
    pub fn reasoning(&self) -> Option<&str> {
        self.reasoning.as_deref()
    }

    /// Tools this message asks to call.
    pub fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }

    /// Which call a tool result answers.
    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }
}

/// Token counts, when a provider reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl Usage {
    /// Builds a usage report from whatever the provider supplied.
    pub fn new(input_tokens: Option<u64>, output_tokens: Option<u64>) -> Self {
        Usage {
            input_tokens,
            output_tokens,
        }
    }

    /// Tokens sent.
    pub fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }

    /// Tokens generated.
    pub fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }

    /// Both counts together, if both are known.
    pub fn total(&self) -> Option<u64> {
        Some(self.input_tokens? + self.output_tokens?)
    }
}

/// Why the model stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// It finished its thought.
    EndTurn,
    /// It ran out of room.
    MaxTokens,
    /// It wants to call a tool.
    ToolUse,
    /// It was filtered.
    ContentFilter,
    /// Something provider-specific.
    Other(String),
}

impl StopReason {
    /// Reads a provider's finish reason.
    pub fn from_wire(reason: &str) -> Self {
        match reason {
            "stop" | "end_turn" => StopReason::EndTurn,
            "length" | "max_tokens" => StopReason::MaxTokens,
            "tool_calls" | "tool_use" | "function_call" => StopReason::ToolUse,
            "content_filter" => StopReason::ContentFilter,
            other => StopReason::Other(other.to_owned()),
        }
    }

    /// The wire spelling, for logging.
    pub fn as_str(&self) -> &str {
        match self {
            StopReason::EndTurn => "stop",
            StopReason::MaxTokens => "max_tokens",
            StopReason::ToolUse => "tool_use",
            StopReason::ContentFilter => "content_filter",
            StopReason::Other(other) => other,
        }
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What to ask the model.
///
/// A request is stateless: the engine holds no conversation. Whoever is
/// thinking owns the context and passes it in, which is what lets the same
/// engine serve several unrelated lines of thought.
#[derive(Debug, Clone, Default)]
pub struct Request {
    messages: Vec<Message>,
    tools: Vec<Tool>,
    model: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
}

impl Request {
    /// An empty request.
    pub fn new() -> Self {
        Request::default()
    }

    /// A request that asks one thing.
    pub fn user(text: impl Into<String>) -> Self {
        Request::new().with_message(Message::user(text))
    }

    /// Adds a turn.
    pub fn with_message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }

    /// Adds framing instructions at the front.
    pub fn with_system(mut self, text: impl Into<String>) -> Self {
        self.messages.insert(0, Message::system(text));
        self
    }

    /// Offers tools the model may call.
    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = tools;
        self
    }

    /// Overrides the provider's default model.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Caps the reply length.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets sampling temperature.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// The conversation so far.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Tools offered, if any.
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// The model override, if any.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The reply cap, if any.
    pub fn max_tokens(&self) -> Option<u32> {
        self.max_tokens
    }

    /// The sampling temperature, if any.
    pub fn temperature(&self) -> Option<f32> {
        self.temperature
    }

    /// Whether any tools are offered.
    pub fn wants_tools(&self) -> bool {
        !self.tools.is_empty()
    }
}

/// What the model answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    content: Option<String>,
    /// The chain of thought, when the model thought before answering.
    reasoning: Option<String>,
    tool_calls: Vec<ToolCall>,
    model: String,
    stop_reason: Option<StopReason>,
    usage: Option<Usage>,
}

impl Response {
    /// Assembles a response.
    pub fn new(
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
        model: impl Into<String>,
        stop_reason: Option<StopReason>,
        usage: Option<Usage>,
    ) -> Self {
        Response {
            content,
            reasoning: None,
            tool_calls,
            model: model.into(),
            stop_reason,
            usage,
        }
    }

    /// Attaches the chain of thought that came with it.
    #[must_use]
    pub fn with_reasoning(mut self, reasoning: Option<String>) -> Self {
        self.reasoning = reasoning.filter(|text| !text.trim().is_empty());
        self
    }

    /// The chain of thought, when the model thought before answering.
    ///
    /// Kept so it can be given back: a provider in thinking mode refuses a
    /// later request that does not carry it.
    pub fn reasoning(&self) -> Option<&str> {
        self.reasoning.as_deref()
    }

    /// The reply text, absent when the model only asked for tools.
    pub fn text(&self) -> Option<&str> {
        self.content.as_deref()
    }

    /// Tools the model asked to call.
    pub fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }

    /// Whether the model wants to call tools.
    pub fn wants_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// Which model answered.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Why it stopped.
    pub fn stop_reason(&self) -> Option<&StopReason> {
        self.stop_reason.as_ref()
    }

    /// Token counts, if reported.
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }
}
