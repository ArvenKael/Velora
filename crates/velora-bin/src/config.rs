//! Finding, reading and repairing the configuration Velora starts from.
//!
//! Three places, in increasing precedence: the machine-wide file, the
//! project-local file, and the environment. A credential belongs in the
//! machine-wide one, because the project-local file sits inside a repository
//! and is one `git add` away from being published.
//!
//! Configuration is read once, at the start. Nothing here reloads: a running
//! system that changed its model underneath itself would be harder to reason
//! about than one that was restarted.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use velora_llm::{ConfigError, Engine, EngineConfig};
use velora_log::warn;

/// The project-local configuration file.
pub const PROJECT_FILE: &str = "velora.toml";
/// Turns the first-run prompt off entirely.
pub const ENV_NO_PROMPT: &str = "VELORA_NO_PROMPT";
/// Makes a model that is missing or unusable fatal, instead of degraded.
pub const ENV_REQUIRE_MODEL: &str = "VELORA_REQUIRE_MODEL";

/// What the runtime ended up with.
pub enum Model {
    /// A model is configured and ready, with the configuration it came from.
    Ready {
        /// The engine to think with.
        engine: Box<Engine>,
        /// What it was built from, which is the baseline settings fold over.
        config: Box<EngineConfig>,
    },
    /// No model, and why. Velora still runs; events that need one wait.
    Absent(String),
}

/// Where the machine-wide file is expected.
pub fn machine_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("XDG_CONFIG_HOME")
        && !explicit.trim().is_empty()
    {
        return Some(PathBuf::from(explicit).join("velora").join("config.toml"));
    }

    std::env::var("HOME")
        .ok()
        .filter(|home| !home.trim().is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("velora")
                .join("config.toml")
        })
}

/// Whether a model is required rather than merely wanted.
fn model_required() -> bool {
    std::env::var(ENV_REQUIRE_MODEL).is_ok_and(|value| !value.is_empty() && value != "0")
}

/// Whether the first-run prompt is allowed.
fn prompting_enabled() -> bool {
    if std::env::var(ENV_NO_PROMPT).is_ok_and(|value| !value.is_empty() && value != "0") {
        return false;
    }
    // Somewhere to ask, and someone to answer.
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Reads a file if it is there.
fn read(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(document) => Ok(Some(document)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

/// Whether something required is simply *missing*, as opposed to something
/// being wrong with what is there.
///
/// The distinction is the whole point: an absence can be filled in, so it is
/// worth asking about, and a valid absence is also a mode Velora runs in. A
/// value that is present and unusable is a mistake to complain about instead.
///
/// Every `Missing` counts, not only the model. A configuration with a model and
/// no key is the case where someone has *most* of what they need, and it was
/// the case that got no prompt at all: the one thing left to ask for, a
/// credential, was the one thing it would not ask for.
fn is_absence(error: &ConfigError) -> bool {
    matches!(error, ConfigError::Missing(_))
}

/// Reads configuration, optionally asking for it, and builds the model.
///
/// # Errors
///
/// Fails only when a model is misconfigured *and* `VELORA_REQUIRE_MODEL` says
/// the system should not run without one.
pub async fn resolve() -> Result<Model, String> {
    let project_path = PathBuf::from(PROJECT_FILE);
    let machine = machine_path();

    // Unreadable files are reported and then treated as absent: a permission
    // problem should not stop a system that still has rules to work with.
    let mut project = None;
    match read(&project_path) {
        Ok(document) => project = document,
        Err(problem) => warn!(problem = %problem, "a configuration file could not be read"),
    }

    let mut machine_document = None;
    if let Some(path) = &machine {
        match read(path) {
            Ok(document) => machine_document = document,
            Err(problem) => warn!(problem = %problem, "a configuration file could not be read"),
        }
    }

    // A credential in the project file is one `git add` from being published.
    if project.as_deref().is_some_and(|document| document.contains("api_key")) {
        warn!(
            project = %project_path.display(),
            machine = %machine
                .as_ref()
                .map_or_else(|| "~/.config/velora/config.toml".to_owned(), |path| path.display().to_string()),
            "the project configuration contains a credential; move it to the machine-wide file"
        );
    }

    let lookup = |key: &str| std::env::var(key).ok();
    let outcome = EngineConfig::from_documents(
        machine_document.as_deref(),
        project.as_deref(),
        lookup,
    );

    match outcome {
        Ok(config) => match config.clone().build() {
            Ok(engine) => Ok(Model::Ready {
                engine: Box::new(engine),
                config: Box::new(config),
            }),
            Err(err) => degraded_or_fatal(err),
        },
        Err(err) if is_absence(&err) => ask_then_degrade().await,
        Err(err) => degraded_or_fatal(err),
    }
}

/// Nothing was configured: offer to fix that, then carry on regardless.
async fn ask_then_degrade() -> Result<Model, String> {
    if prompting_enabled()
        && let Some(config) = prompt().await
    {
        return match config.clone().build() {
            Ok(engine) => Ok(Model::Ready {
                engine: Box::new(engine),
                config: Box::new(config),
            }),
            Err(err) => degraded_or_fatal(err),
        };
    }

    Ok(Model::Absent("nothing is configured".to_owned()))
}

/// A configuration exists but cannot be honoured.
///
/// Velora can still do useful work without a model, so the default is to say so
/// loudly and keep running. `VELORA_REQUIRE_MODEL` is for the opposite
/// preference: nothing half-configured.
fn degraded_or_fatal(error: ConfigError) -> Result<Model, String> {
    if model_required() {
        return Err(format!(
            "{error} — and {ENV_REQUIRE_MODEL} is set, so Velora will not run half-configured"
        ));
    }

    warn!(
        problem = %error,
        "the model configuration cannot be used; Velora will run without a model until it is fixed"
    );

    Ok(Model::Absent(error.to_string()))
}

/// Runs the configuration prompt because it was asked for.
///
/// Unlike the first-run prompt this does not wait to be needed: it runs on a
/// system that is already configured, and it runs where the automatic one would
/// have declined to. Someone who wants to give Velora a key should not have to
/// work out why it did not ask.
///
/// # Errors
///
/// Fails if the terminal cannot be read, or if the file cannot be written.
pub async fn configure() -> Result<(), String> {
    let path = machine_path();

    let outcome = tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut reader = stdin.lock();
        let mut writer = stdout.lock();

        match crate::setup::run(&mut reader, &mut writer, path.as_deref()) {
            Ok(config) => Ok(config),
            Err(err) => Err(format!("the prompt could not be run: {err}")),
        }
    })
    .await
    .map_err(|err| format!("the prompt could not be run: {err}"))??;

    match outcome {
        Some(_) => Ok(()),
        None => Err("nothing was saved".to_owned()),
    }
}

/// Runs the first-run prompt on a blocking thread, since it reads a terminal.
async fn prompt() -> Option<EngineConfig> {
    tokio::task::spawn_blocking(|| {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut reader = stdin.lock();
        let mut writer = stdout.lock();

        match crate::setup::run(&mut reader, &mut writer, machine_path().as_deref()) {
            Ok(config) => config,
            Err(err) => {
                let _ = writeln!(writer, "setup could not be completed: {err}");
                None
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// Writes a configuration somewhere only its owner can read it.
pub fn write_private(path: &Path, document: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        restrict_directory(parent);
    }

    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(document.as_bytes())?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        std::fs::write(path, document)
    }
}

#[cfg(unix)]
fn restrict_directory(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    // Best effort: a directory that could not be tightened is not a reason to
    // refuse to save the configuration.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) {}

/// A prompt that takes a default when the answer is empty.
pub fn ask(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    question: &str,
    default: &str,
) -> std::io::Result<String> {
    if default.is_empty() {
        write!(writer, "{question}: ")?;
    } else {
        write!(writer, "{question} [{default}]: ")?;
    }
    writer.flush()?;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let answer = line.trim();

    Ok(if answer.is_empty() {
        default.to_owned()
    } else {
        answer.to_owned()
    })
}
