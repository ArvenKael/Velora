use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::message::{Message, Request, Response, StopReason, Tool, ToolCall, Usage};
use crate::provider::{Capabilities, Provider, ProviderError, truncate};
use crate::secret::ApiKey;
use crate::stream::{CompletionStream, StreamEvent};

/// Talks to any service shaped like the OpenAI chat completions API.
///
/// One implementation covers a lot of ground: OpenAI itself, xAI, Groq,
/// OpenRouter, Together, and local runtimes such as Ollama or llama.cpp, which
/// all speak this shape with a different base URL and model name.
pub struct OpenAiCompatible {
    client: reqwest::Client,
    base_url: String,
    api_key: ApiKey,
    model: String,
    name: String,
}

impl fmt::Debug for OpenAiCompatible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatible")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatible {
    /// Points the provider at a service.
    ///
    /// `base_url` is the API root, such as `https://api.openai.com/v1`; the
    /// chat completions path is appended to it.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<ApiKey>, model: impl Into<String>) -> Self {
        OpenAiCompatible {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            name: "openai-compatible".to_owned(),
        }
    }

    /// Replaces the HTTP client, for a custom timeout or proxy.
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Names the provider for logs, such as `groq`.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// The chat completions endpoint.
    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// Builds the wire request.
    fn wire_request(&self, request: &Request, stream: bool) -> WireRequest {
        let model = request.model().unwrap_or(&self.model).to_owned();
        let mut messages: Vec<WireMessage> =
            request.messages().iter().map(WireMessage::from).collect();

        // DeepSeek's thinking mode demands that the chain of thought be given
        // back on every later request that carries tools — every assistant
        // message, even one that thought nothing. Omitting the field is not
        // "nothing to pass back": it is a 400 that names the field, and the
        // whole turn is lost. A model that thought nothing gets an empty string,
        // which is the shape the integration DeepSeek ships itself uses.
        //
        // Scoped to DeepSeek because the field is not part of the OpenAI API: a
        // provider that has never heard of it should not be sent it.
        if !request.tools().is_empty() && self.is_deepseek(&model) {
            for message in &mut messages {
                if message.role == "assistant" && message.reasoning_content.is_none() {
                    message.reasoning_content = Some(String::new());
                }
            }
        }

        WireRequest {
            model,
            messages,
            max_tokens: request.max_tokens(),
            temperature: request.temperature(),
            tools: request.tools().iter().map(WireTool::from).collect(),
            stream: stream.then_some(true),
        }
    }

    /// Whether this endpoint is DeepSeek's, judged by what it calls itself.
    ///
    /// A heuristic, and deliberately a narrow one: the field it guards is
    /// DeepSeek's own, so a wrong "yes" would send a provider a field it does not
    /// know and a wrong "no" leaves the 400 in place. Both the model name and
    /// the base URL are checked, because a gateway can rename either.
    fn is_deepseek(&self, model: &str) -> bool {
        let named = |text: &str| text.to_ascii_lowercase().contains("deepseek");

        named(model) || named(&self.base_url)
    }

    /// Sends a request and fails on a non-success status.
    async fn post(&self, body: &WireRequest) -> Result<reqwest::Response, ProviderError> {
        let response = self
            .client
            .post(self.endpoint())
            .bearer_auth(self.api_key.expose())
            .json(body)
            .send()
            .await
            .map_err(|err| ProviderError::Transport(err.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Status {
                status: status.as_u16(),
                body: truncate(&body),
            });
        }

        Ok(response)
    }
}

#[async_trait]
impl Provider for OpenAiCompatible {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    async fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        let body = self.wire_request(request, false);
        let response = self.post(&body).await?;
        let text = response
            .text()
            .await
            .map_err(|err| ProviderError::Transport(err.to_string()))?;

        let wire: ChatResponse = serde_json::from_str(&text).map_err(|err| {
            ProviderError::Decode(format!("{err}: {}", truncate(text.as_str())))
        })?;

        wire.into_response()
    }

    async fn stream(&self, request: &Request) -> Result<CompletionStream, ProviderError> {
        let body = self.wire_request(request, true);
        // The request is accepted (or rejected) here, so a bad key does not
        // turn into a confusing error halfway through a thought.
        let response = self.post(&body).await?;

        let (sender, receiver) = mpsc::channel(64);
        tokio::spawn(async move {
            if let Err(err) = pump(response, &sender).await {
                let _ = sender.send(Err(err)).await;
            }
        });

        Ok(CompletionStream::new(receiver))
    }
}

/// Reads the server-sent event stream and forwards it as increments.
async fn pump(
    mut response: reqwest::Response,
    sender: &mpsc::Sender<Result<StreamEvent, ProviderError>>,
) -> Result<(), ProviderError> {
    let mut buffer = String::new();
    let mut model: Option<String> = None;
    let mut stop_reason: Option<StopReason> = None;
    let mut usage: Option<Usage> = None;

    loop {
        let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| ProviderError::Transport(err.to_string()))?
        else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Frames can be split across reads, so only whole lines are consumed.
        while let Some(newline) = buffer.find('\n') {
            let line: String = buffer.drain(..=newline).collect();
            let line = line.trim_end_matches(['\r', '\n']);

            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }

            if payload == "[DONE]" {
                finish(sender, model, stop_reason, usage).await;
                return Ok(());
            }

            let chunk: StreamChunk = serde_json::from_str(payload).map_err(|err| {
                ProviderError::Decode(format!("{err}: {}", truncate(payload)))
            })?;

            if chunk.model.is_some() {
                model = chunk.model;
            }
            if chunk.usage.is_some() {
                usage = chunk.usage.map(Usage::from);
            }

            for choice in chunk.choices {
                if let Some(delta) = choice.delta {
                    if let Some(text) = delta.content
                        && !text.is_empty()
                        && sender.send(Ok(StreamEvent::Text(text))).await.is_err()
                    {
                        return Ok(());
                    }

                    for call in delta.tool_calls {
                        let index = call.index.unwrap_or(0) as usize;
                        let function = call.function.unwrap_or_default();

                        if call.id.is_some() || function.name.is_some() {
                            let started = StreamEvent::ToolCallStarted {
                                index,
                                id: call.id.unwrap_or_default(),
                                name: function.name.unwrap_or_default(),
                            };
                            if sender.send(Ok(started)).await.is_err() {
                                return Ok(());
                            }
                        }

                        if let Some(fragment) = function.arguments
                            && !fragment.is_empty()
                            && sender
                                .send(Ok(StreamEvent::ToolCallArguments { index, fragment }))
                                .await
                                .is_err()
                        {
                            return Ok(());
                        }
                    }
                }

                if let Some(reason) = choice.finish_reason {
                    stop_reason = Some(StopReason::from_wire(&reason));
                }
            }
        }
    }

    // Some servers just close the connection instead of sending `[DONE]`.
    finish(sender, model, stop_reason, usage).await;
    Ok(())
}

async fn finish(
    sender: &mpsc::Sender<Result<StreamEvent, ProviderError>>,
    model: Option<String>,
    stop_reason: Option<StopReason>,
    usage: Option<Usage>,
) {
    let _ = sender
        .send(Ok(StreamEvent::Finished {
            model,
            stop_reason,
            usage,
        }))
        .await;
}

#[derive(Serialize)]
struct WireRequest {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    /// The chain of thought, given back as it was received.
    ///
    /// DeepSeek's thinking mode requires this on every later request that
    /// carries tools: "The `reasoning_content` in the thinking mode must be
    /// passed back to the API". It is carried through rather than dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl From<&Message> for WireMessage {
    fn from(message: &Message) -> Self {
        WireMessage {
            role: message.role().as_str(),
            content: message.content().map(str::to_owned),
            reasoning_content: message.reasoning().map(str::to_owned),
            tool_calls: message
                .tool_calls()
                .iter()
                .map(|call| WireToolCall {
                    id: call.id().to_owned(),
                    kind: "function",
                    function: WireCallFunction {
                        name: call.name().to_owned(),
                        arguments: call.arguments().to_owned(),
                    },
                })
                .collect(),
            tool_call_id: message.tool_call_id().map(str::to_owned),
        }
    }
}

#[derive(Serialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireCallFunction,
}

#[derive(Serialize)]
struct WireCallFunction {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct WireTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolDefinition,
}

impl From<&Tool> for WireTool {
    fn from(tool: &Tool) -> Self {
        WireTool {
            kind: "function",
            function: WireToolDefinition {
                name: tool.name().to_owned(),
                description: tool.description().to_owned(),
                parameters: tool.parameters().clone(),
            },
        }
    }
}

#[derive(Serialize)]
struct WireToolDefinition {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

impl ChatResponse {
    fn into_response(self) -> Result<Response, ProviderError> {
        let ChatResponse {
            model,
            choices,
            usage,
        } = self;

        let choice = choices.into_iter().next().ok_or_else(|| {
            ProviderError::Refused("the provider returned no choices".to_owned())
        })?;

        let message = choice.message.unwrap_or_default();
        let tool_calls: Vec<ToolCall> = message
            .tool_calls
            .into_iter()
            .map(|call| {
                let function = call.function.unwrap_or_default();
                ToolCall::new(
                    call.id.unwrap_or_default(),
                    function.name.unwrap_or_default(),
                    function.arguments.unwrap_or_default(),
                )
            })
            .collect();

        let content = message.content.filter(|text| !text.is_empty());
        if content.is_none() && tool_calls.is_empty() {
            return Err(ProviderError::Refused(
                "the provider returned a message with neither text nor tool calls".to_owned(),
            ));
        }

        Ok(Response::new(
            content,
            tool_calls,
            model.unwrap_or_default(),
            choice.finish_reason.map(|reason| StopReason::from_wire(&reason)),
            usage.map(Usage::from),
        )
        .with_reasoning(message.reasoning_content))
    }
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<ChoiceMessage>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct ChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    /// The chain of thought, when the model is thinking.
    ///
    /// It has to be given back on every later request that carries tools, or the
    /// API refuses with a 400 that names this field.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ResponseToolCall>,
}

#[derive(Deserialize)]
struct ResponseToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ResponseFunction>,
}

#[derive(Deserialize, Default)]
struct ResponseFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

impl From<WireUsage> for Usage {
    fn from(wire: WireUsage) -> Self {
        Usage::new(wire.prompt_tokens, wire.completion_tokens)
    }
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFunctionDelta>,
}

#[derive(Deserialize, Default)]
struct StreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}
