use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::device::{Device, DeviceError, Outcome};
use crate::procedure::Procedure;
use crate::shell::Shell;

/// The longest a command may be given to run, in seconds.
///
/// An hour, which is long enough for an installer, a build or another agent's
/// attempt at something, and short enough that a command which will never finish
/// does not hold a slot until the process is restarted.
pub const MAX_TIMEOUT_SECS: u64 = 60 * 60;

/// The computer Velora is running on.
///
/// One device, one name, and the whole machine behind it: a shell, the
/// filesystem, and a look at what is running. This is the "full control" the
/// system was asked for, which means what it says — there is no sandbox here.
/// What there is instead is a record: every attempt and every result is an
/// event, and an [`ExecutionPolicy`](crate::ExecutionPolicy) sits in front of
/// this device so that control can be narrowed without changing any of it.
pub struct LocalDevice {
    shell: Shell,
}

impl std::fmt::Debug for LocalDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalDevice")
            .field("workdir", &self.shell.workdir())
            .field("timeout", &self.shell.timeout())
            .field("max_output_bytes", &self.shell.max_output())
            .finish()
    }
}

impl Default for LocalDevice {
    fn default() -> Self {
        LocalDevice::new()
    }
}

impl LocalDevice {
    /// Reaches the machine this process is running on.
    pub fn new() -> Self {
        LocalDevice { shell: Shell::new() }
    }

    /// Sets where relative paths and commands start from.
    pub fn with_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.shell = self.shell.with_workdir(workdir);
        self
    }

    /// Sets how long any one procedure may take.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.shell = self.shell.with_timeout(timeout);
        self
    }

    /// Sets how much output is kept from any one procedure.
    ///
    /// Not a nicety: a command like `yes` produces output until something stops
    /// it, and the output ends up in history, which is never rewritten.
    pub fn with_max_output(mut self, bytes: usize) -> Self {
        self.shell = self.shell.with_max_output(bytes);
        self
    }

    /// Runs a shell command, bounded in time and output.
    async fn run_shell(&self, command: &str, cwd: Option<&Path>) -> Result<Outcome, DeviceError> {
        self.run_shell_for(command, cwd, None).await
    }

    /// Runs a shell command, for as long as it asked for.
    ///
    /// A command that is going to take twenty minutes — an installer, a build, a
    /// compiler, another agent working on something — cannot be run under a limit
    /// meant for looking something up, and the model has no way to know what the
    /// limit was unless it is told and can ask for more. The ceiling is the
    /// operator's, not the model's: a step holds a turn and a slot in the
    /// executor, and something that never comes back is worse than something that
    /// was stopped.
    async fn run_shell_for(
        &self,
        command: &str,
        cwd: Option<&Path>,
        timeout_secs: Option<u64>,
    ) -> Result<Outcome, DeviceError> {
        let shell = match timeout_secs {
            None => self.shell.clone(),
            Some(asked) => self.shell.clone().with_timeout(Duration::from_secs(
                asked.clamp(1, MAX_TIMEOUT_SECS),
            )),
        };

        shell.run(command, cwd, &[], None).await
    }

    async fn read_file(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let path = PathBuf::from(text(arguments, "path")?);

        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let cut = bytes.len() > self.shell.max_output();
                let shown = &bytes[..bytes.len().min(self.shell.max_output())];
                let mut output = String::from_utf8_lossy(shown).into_owned();
                if cut {
                    output.push_str(&format!(
                        "\n[file cut at {} bytes; it is {} bytes long]",
                        self.shell.max_output(),
                        bytes.len()
                    ));
                }
                Ok(Outcome::new(true, None, output).truncated(cut))
            }
            Err(err) => Ok(Outcome::failed(format!("{}: {err}", path.display()))),
        }
    }

    async fn write_file(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let path = PathBuf::from(text(arguments, "path")?);
        let contents = text(arguments, "contents")?;

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(err) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(Outcome::failed(format!(
                "could not create {}: {err}",
                parent.display()
            )));
        }

        match tokio::fs::write(&path, contents.as_bytes()).await {
            Ok(()) => Ok(Outcome::ok(format!(
                "wrote {} bytes to {}",
                contents.len(),
                path.display()
            ))),
            Err(err) => Ok(Outcome::failed(format!("{}: {err}", path.display()))),
        }
    }

    async fn list_dir(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let path = PathBuf::from(text(arguments, "path")?);

        let mut entries = match tokio::fs::read_dir(&path).await {
            Ok(entries) => entries,
            Err(err) => return Ok(Outcome::failed(format!("{}: {err}", path.display()))),
        };

        let mut lines: Vec<String> = Vec::new();
        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let described = match entry.metadata().await {
                        Ok(meta) if meta.is_dir() => format!("{name}/"),
                        Ok(meta) => format!("{name} ({} bytes)", meta.len()),
                        Err(_) => name,
                    };
                    lines.push(described);
                }
                Ok(None) => break,
                Err(err) => return Ok(Outcome::failed(format!("{}: {err}", path.display()))),
            }
        }

        lines.sort();
        let total = lines.len();
        let shown = self.shell.max_output() / 32;
        let cut = total > shown;
        lines.truncate(shown);

        let mut output = format!("{} entries in {}\n", total, path.display());
        output.push_str(&lines.join("\n"));
        if cut {
            output.push_str(&format!("\n[showing the first {shown}]"));
        }

        Ok(Outcome::new(true, None, output).truncated(cut))
    }

    async fn remove(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let path = PathBuf::from(text(arguments, "path")?);
        let recursive = arguments
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let meta = match tokio::fs::symlink_metadata(&path).await {
            Ok(meta) => meta,
            Err(err) => return Ok(Outcome::failed(format!("{}: {err}", path.display()))),
        };

        let result = if meta.is_dir() {
            if recursive {
                tokio::fs::remove_dir_all(&path).await
            } else {
                tokio::fs::remove_dir(&path).await
            }
        } else {
            tokio::fs::remove_file(&path).await
        };

        match result {
            Ok(()) => Ok(Outcome::ok(format!("removed {}", path.display()))),
            Err(err) => Ok(Outcome::failed(format!("{}: {err}", path.display()))),
        }
    }
}

#[async_trait]
impl Device for LocalDevice {
    fn name(&self) -> &str {
        "local"
    }

    fn procedures(&self) -> Vec<Procedure> {
        vec![
            Procedure::write(
                "shell.run",
                "Run a shell command on this computer and return what it printed. Pipes, \
                 redirection and globbing all work — but this is `sh`, not bash: write \
                 POSIX shell, because bash-only syntax is not understood here. Process \
                 substitution (`< <(...)`) in particular is a syntax error, and so are \
                 here-strings. A command is stopped after 60 seconds unless `timeout_secs` \
                 says otherwise, so anything long — an installer, a build, another program \
                 working on something — has to ask for the time it needs.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "the command line to run" },
                        "cwd": { "type": "string", "description": "directory to run it in" },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "how long it may run before it is stopped; 60 by \
                                            default, up to an hour"
                        }
                    },
                    "required": ["command"]
                }),
            ),
            Procedure::read(
                "fs.read",
                "Read a file's contents.",
                json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            ),
            Procedure::write(
                "fs.write",
                "Write a file, creating its directory if needed and replacing it if it exists.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "contents": { "type": "string" }
                    },
                    "required": ["path", "contents"]
                }),
            ),
            Procedure::read(
                "fs.list",
                "List a directory's entries.",
                json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            ),
            Procedure::write(
                "fs.remove",
                "Delete a file, or a directory when recursive is true.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "recursive": { "type": "boolean" }
                    },
                    "required": ["path"]
                }),
            ),
            Procedure::read(
                "process.list",
                "List the processes running on this computer.",
                json!({ "type": "object", "properties": {} }),
            ),
            Procedure::read(
                "system.info",
                "Report what this computer is: operating system, architecture, \
                 working directory and process id.",
                json!({ "type": "object", "properties": {} }),
            ),
        ]
    }

    async fn invoke(&self, procedure: &str, arguments: &Value) -> Result<Outcome, DeviceError> {
        match procedure {
            "shell.run" => {
                let command = text(arguments, "command")?;
                let cwd = optional_text(arguments, "cwd")?.map(PathBuf::from);
                let asked = arguments.get("timeout_secs").and_then(Value::as_u64);

                self.run_shell_for(&command, cwd.as_deref(), asked).await
            }
            "fs.read" => self.read_file(arguments).await,
            "fs.write" => self.write_file(arguments).await,
            "fs.list" => self.list_dir(arguments).await,
            "fs.remove" => self.remove(arguments).await,
            "process.list" => {
                if cfg!(unix) {
                    self.run_shell("ps -eo pid,ppid,comm", None).await
                } else {
                    Ok(Outcome::failed(
                        "process.list is only implemented on unix-like systems",
                    ))
                }
            }
            "system.info" => {
                let workdir = std::env::current_dir()
                    .map_or_else(|_| "(unknown)".to_owned(), |path| path.display().to_string());
                Ok(Outcome::ok(format!(
                    "os: {}\narch: {}\nworkdir: {workdir}\npid: {}",
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    std::process::id()
                )))
            }
            other => Err(DeviceError::UnknownProcedure(other.to_owned())),
        }
    }
}

fn text(arguments: &Value, key: &str) -> Result<String, DeviceError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| DeviceError::InvalidArguments(format!("{key} must be a string")))
}

fn optional_text(arguments: &Value, key: &str) -> Result<Option<String>, DeviceError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(|text| Some(text.to_owned()))
            .ok_or_else(|| DeviceError::InvalidArguments(format!("{key} must be a string"))),
    }
}
