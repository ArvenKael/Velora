//! Running a bounded shell command.
//!
//! One implementation of "start a command, keep its output within a cap, and do
//! not let it outlive its time" — because there are now two things that run
//! commands: the computer [`Device`](crate::Device) and the script a capability
//! is made of. Two copies of this would drift, and the drift would be in the
//! part that decides how much output reaches history and how long a stray
//! process may hold a pipe.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::device::{DeviceError, Outcome};

/// Where and how commands run.
#[derive(Debug, Clone)]
pub struct Shell {
    workdir: PathBuf,
    timeout: Duration,
    max_output_bytes: usize,
}

impl Default for Shell {
    fn default() -> Self {
        Shell::new()
    }
}

impl Shell {
    /// A shell in the current directory, with the default bounds.
    pub fn new() -> Self {
        Shell {
            workdir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            timeout: Duration::from_secs(60),
            max_output_bytes: 64 * 1024,
        }
    }

    /// Sets where relative paths and commands start from.
    pub fn with_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.workdir = workdir.into();
        self
    }

    /// Sets how long any one command may take.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets how much output is kept.
    ///
    /// Not a nicety: a command like `yes` produces output until something stops
    /// it, and the output ends up in history, which is never rewritten.
    pub fn with_max_output(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes.max(1);
        self
    }

    /// Where commands run unless told otherwise.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// How long a command may take.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// How much output is kept.
    pub fn max_output(&self) -> usize {
        self.max_output_bytes
    }

    /// Runs a command line, bounded in time and output.
    ///
    /// `env` is added to the environment rather than replacing it, and `input`
    /// is written to the command's standard input — a command given no input
    /// gets none, so it cannot wait on this process's own.
    pub async fn run(
        &self,
        command: &str,
        cwd: Option<&Path>,
        env: &[(String, String)],
        input: Option<&str>,
    ) -> Result<Outcome, DeviceError> {
        let cwd = cwd.map_or_else(|| self.workdir.clone(), Path::to_path_buf);

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&cwd)
            .envs(env.iter().map(|(name, value)| (name, value)))
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Dropping the child kills it, so a timeout cannot leave an orphan
            // behind holding a pipe.
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| DeviceError::Failed(format!("could not start a shell: {err}")))?;

        if let Some(text) = input {
            let mut stdin = child.stdin.take().expect("stdin was piped");
            // A command that never reads its input would otherwise leave this
            // write waiting forever, which is why the write is bounded too.
            let _ = tokio::time::timeout(self.timeout, async {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(text.as_bytes()).await;
                let _ = stdin.shutdown().await;
            })
            .await;
        }

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // Both pipes at once: reading them in turn deadlocks as soon as the
        // command fills the one nobody is reading.
        let cap = self.max_output_bytes;
        let reading = async { tokio::join!(read_capped(stdout, cap), read_capped(stderr, cap)) };

        let ((out, out_cut), (err, err_cut)) = tokio::time::timeout(self.timeout, reading)
            .await
            .map_err(|_| {
                DeviceError::Failed(format!(
                    "it did not finish within {}s and was stopped",
                    self.timeout.as_secs()
                ))
            })?;

        let truncated = out_cut || err_cut;
        if truncated {
            // Nothing more is wanted from it, so stopping it now beats waiting
            // for the clock to run out.
            let _ = child.kill().await;
        }

        let status = child
            .wait()
            .await
            .map_err(|err| DeviceError::Failed(format!("could not collect its exit: {err}")))?;

        let mut output = String::from_utf8_lossy(&out).into_owned();
        if !err.is_empty() {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str("--- stderr ---\n");
            output.push_str(&String::from_utf8_lossy(&err));
        }
        if truncated {
            output.push_str(&format!("\n[output cut at {} bytes]", self.max_output_bytes));
        }
        if output.trim().is_empty() {
            output.push_str("(no output)");
        }

        Ok(Outcome::new(status.success(), status.code(), output).truncated(truncated))
    }
}

/// Reads at most `cap` bytes, reporting whether there was more.
pub(crate) async fn read_capped<R: AsyncRead + Unpin>(mut reader: R, cap: usize) -> (Vec<u8>, bool) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => return (buffer, false),
            Ok(read) => {
                let room = cap.saturating_sub(buffer.len());
                if read > room {
                    buffer.extend_from_slice(&chunk[..room]);
                    return (buffer, true);
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            Err(_) => return (buffer, false),
        }
    }
}
