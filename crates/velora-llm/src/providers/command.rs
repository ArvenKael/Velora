use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::message::{Request, Response, Role, StopReason};
use crate::provider::{Capabilities, Provider, ProviderError, truncate};
use crate::stream::{CompletionStream, StreamEvent};

/// Talks to a model reached by running a program.
///
/// This exists because a subscription is not an API. A Claude Code or Codex
/// subscription is a session behind a command, not a bearer token against an
/// endpoint — so if the engine could only speak HTTP, "add subscriptions later"
/// would mean rewriting it. Here it means adding a `CommandProvider`.
///
/// # Safety
///
/// The program and its arguments are supplied by whoever configures the engine,
/// never by a model. The prompt is written to the process's standard input,
/// which is also why no shell is involved: an argument list is passed directly
/// to the program, so nothing in a prompt is ever interpreted as a command.
/// Configuring `sh -c` is possible and is then the operator's explicit choice.
pub struct CommandProvider {
    program: String,
    args: Vec<String>,
    model: String,
    name: String,
}

impl std::fmt::Debug for CommandProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandProvider")
            .field("name", &self.name)
            .field("program", &self.program)
            .field("args", &self.args)
            .field("model", &self.model)
            .finish()
    }
}

impl CommandProvider {
    /// Runs `program` with `args`, feeding the prompt on standard input.
    pub fn new(program: impl Into<String>, args: Vec<String>, model: impl Into<String>) -> Self {
        CommandProvider {
            program: program.into(),
            args,
            model: model.into(),
            name: "command".to_owned(),
        }
    }

    /// Names the provider for logs, such as `claude-cli`.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Starts the process with piped streams.
    fn spawn(&self) -> Result<tokio::process::Child, ProviderError> {
        Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| {
                ProviderError::Configuration(format!("could not run {}: {err}", self.program))
            })
    }

    /// Offers tools? No — and saying so is the point.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            tools: false,
        }
    }
}

/// Flattens a conversation into the single block of text a command expects.
///
/// A command has no notion of roles, so the roles are spelled out in the text.
/// This is a real loss of structure and it is why [`CommandProvider`] declares
/// that it cannot do tools: flattening tool calls into prose and hoping the
/// model answers in kind would be a lie about what the provider supports.
fn as_prompt(request: &Request) -> String {
    let mut prompt = String::new();

    for message in request.messages() {
        let heading = match message.role() {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool result",
        };
        prompt.push_str(heading);
        prompt.push_str(":\n");

        if let Some(content) = message.content() {
            prompt.push_str(content);
            prompt.push('\n');
        }
        for call in message.tool_calls() {
            prompt.push_str(&format!("[calls {}({})]\n", call.name(), call.arguments()));
        }
        prompt.push('\n');
    }

    prompt
}

/// Writes the prompt and closes the stream, so a command reading to end-of-file
/// does not wait forever for input that will never come.
async fn feed(mut stdin: tokio::process::ChildStdin, prompt: String) {
    // A failure here surfaces as the process's exit status, which is checked
    // separately; a broken pipe on the way in is a symptom, not the cause.
    let _ = stdin.write_all(prompt.as_bytes()).await;
    let _ = stdin.shutdown().await;
}

#[async_trait]
impl Provider for CommandProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> Capabilities {
        CommandProvider::capabilities(self)
    }

    async fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        let mut child = self.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            ProviderError::Configuration("the command has no standard input".to_owned())
        })?;

        // Fed from its own task: a large prompt and a chatty command would
        // otherwise deadlock, each waiting for the other to drain a pipe.
        let prompt = as_prompt(request);
        tokio::spawn(async move {
            let mut stdin = stdin;
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });

        let output = child
            .wait_with_output()
            .await
            .map_err(|err| ProviderError::Transport(err.to_string()))?;

        if !output.status.success() {
            return Err(ProviderError::Status {
                status: exit_status(&output.status),
                body: truncate(&String::from_utf8_lossy(&output.stderr)),
            });
        }

        let text = String::from_utf8_lossy(&output.stdout).trim_end().to_owned();
        if text.is_empty() {
            return Err(ProviderError::Refused(
                "the command produced no output".to_owned(),
            ));
        }

        Ok(Response::new(
            Some(text),
            Vec::new(),
            self.model.clone(),
            Some(StopReason::EndTurn),
            None,
        ))
    }

    async fn stream(&self, request: &Request) -> Result<CompletionStream, ProviderError> {
        let mut child = self.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            ProviderError::Configuration("the command has no standard input".to_owned())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ProviderError::Configuration("the command has no standard output".to_owned())
        })?;

        let prompt = as_prompt(request);
        let model = self.model.clone();
        let (sender, receiver) = mpsc::channel(64);

        tokio::spawn(async move {
            let feeder = tokio::spawn(async move { feed(stdin, prompt).await });

            // A command that prints as it goes becomes a stream, one line at a
            // time. Lines keep the text valid UTF-8 across reads, and the
            // terminator is kept so the fragments rejoin into exactly what the
            // command printed.
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let fragment = format!("{line}\n");
                        if sender.send(Ok(StreamEvent::Text(fragment))).await.is_err() {
                            let _ = child.kill().await;
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        let _ = sender
                            .send(Err(ProviderError::Transport(err.to_string())))
                            .await;
                        let _ = child.kill().await;
                        return;
                    }
                }
            }

            let _ = feeder.await;
            let status = child.wait().await;
            match status {
                Ok(status) if !status.success() => {
                    let _ = sender
                        .send(Err(ProviderError::Status {
                            status: exit_status(&status),
                            body: "the command failed after producing output".to_owned(),
                        }))
                        .await;
                }
                Ok(_) => {
                    let _ = sender
                        .send(Ok(StreamEvent::Finished {
                            model: Some(model),
                            stop_reason: Some(StopReason::EndTurn),
                            usage: None,
                        }))
                        .await;
                }
                Err(err) => {
                    let _ = sender
                        .send(Err(ProviderError::Transport(err.to_string())))
                        .await;
                }
            }
        });

        Ok(CompletionStream::new(receiver))
    }
}

/// The exit code as a status, clamped because a signal death has none.
fn exit_status(status: &std::process::ExitStatus) -> u16 {
    status.code().and_then(|code| u16::try_from(code).ok()).unwrap_or(1)
}
