//! The optional first-run prompt.
//!
//! Velora runs without a model, so this is a convenience rather than a
//! requirement: it exists so that "nothing is configured" can be fixed in one
//! step instead of by reading a handoff document. It is skipped entirely when
//! there is no terminal to ask, so an unattended start is never blocked by a
//! question nobody can answer.

use std::io::{BufRead, Write};
use std::path::Path;

use velora_llm::{EngineConfig, ENV_API_KEY, ENV_BASE_URL, ENV_COMMAND, ENV_COMMAND_ARGS, ENV_MODEL, ENV_PROVIDER};

use crate::config::ask;

/// An endpoint Velora knows by name, so the prompt can be answered in one word.
///
/// The question used to be "openai or command", which is true and unhelpful: a
/// person with a DeepSeek key has no way to know that "openai" is what they
/// are. Anything shaped like the OpenAI API works, and these are the ones worth
/// naming.
struct Endpoint {
    name: &'static str,
    base_url: &'static str,
    model: &'static str,
}

const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        name: "openai",
        base_url: velora_llm::DEFAULT_BASE_URL,
        model: "gpt-4o-mini",
    },
    Endpoint {
        // The API root, not the /v1 form: the provider appends
        // `/chat/completions` to whatever is given here, and this is the base
        // URL DeepSeek documents for OpenAI-shaped clients.
        name: "deepseek",
        base_url: "https://api.deepseek.com",
        model: "deepseek-flash",
    },
];

/// The two shapes of provider, settled as soon as the answer is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// Something reached over HTTP, shaped like the OpenAI API.
    Api,
    /// A program whose standard input is the prompt.
    Command,
}

/// The endpoint a name refers to, if it is one Velora knows.
fn endpoint(name: &str) -> Option<&'static Endpoint> {
    ENDPOINTS.iter().find(|endpoint| endpoint.name == name)
}

/// Asks for a configuration, and saves it where it belongs.
///
/// The credential is only ever written to the machine-wide file, never to the
/// project-local one, and that file is created readable only by its owner.
pub fn run(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    save_to: Option<&Path>,
) -> std::io::Result<Option<EngineConfig>> {
    writeln!(writer)?;
    writeln!(writer, "Velora has no model configured.")?;
    writeln!(
        writer,
        "Velora works without one — rules still decide what they cover — but events that"
    )?;
    writeln!(
        writer,
        "need a model are held until one is available. Answer below to set one up now;"
    )?;
    writeln!(
        writer,
        "nothing is saved without a key, and Velora runs without a model until there is one.\n"
    )?;

    let provider = ask(
        reader,
        writer,
        "Provider (openai, deepseek, or command for a subscription CLI)",
        "openai",
    )?;
    let provider = provider.trim().to_lowercase();
    let known = endpoint(&provider);

    // Which provider this is, settled before anything is asked about it. Asking
    // for a model first meant a mistyped provider was answered with "no model
    // given", which is true of every question that was not reached and says
    // nothing about the actual mistake.
    let choice = match provider.as_str() {
        "openai" | "openai-compatible" | "deepseek" => Choice::Api,
        "command" | "cli" => Choice::Command,
        other => {
            writeln!(
                writer,
                "\n{other:?} is not a provider Velora knows. Nothing was saved."
            )?;
            return Ok(None);
        }
    };

    // A named endpoint already knows its model, so this is a default rather
    // than a question with no answer in it.
    let model_default = known.map_or("", |endpoint| endpoint.model);
    let model = ask(reader, writer, "Model", model_default)?;
    if model.trim().is_empty() {
        writeln!(
            writer,
            "\nNo model given, so nothing was saved. Velora will run without one."
        )?;
        return Ok(None);
    }

    // The name that goes in the file is the one the loader knows, which is not
    // always the one that was typed. "deepseek" is an endpoint, not a provider:
    // what makes it DeepSeek is the base URL, and the provider is the shape of
    // the API, which is OpenAI's. Writing the typed name through would produce a
    // file Velora then refuses to read.
    let canonical = match choice {
        Choice::Api => "openai",
        Choice::Command => "command",
    };

    let mut answers: Vec<(&str, String)> = vec![
        (ENV_PROVIDER, canonical.to_owned()),
        (ENV_MODEL, model.trim().to_owned()),
    ];

    match choice {
        Choice::Api => {
            let base_url = ask(
                reader,
                writer,
                "API base URL",
                known.map_or(velora_llm::DEFAULT_BASE_URL, |endpoint| endpoint.base_url),
            )?;
            answers.push((ENV_BASE_URL, base_url.trim().to_owned()));

            writeln!(
                writer,
                "The key is stored in the machine-wide file with owner-only permissions. \
                 It is typed in the clear."
            )?;
            let api_key = ask(reader, writer, "API key", "")?;
            if api_key.trim().is_empty() {
                writeln!(
                    writer,
                    "\nNo key given, so nothing was saved. Export {ENV_API_KEY} when you have one."
                )?;
                return Ok(None);
            }
            answers.push((ENV_API_KEY, api_key.trim().to_owned()));
        }
        Choice::Command => {
            let program = ask(reader, writer, "Program to run", "")?;
            if program.trim().is_empty() {
                writeln!(writer, "\nNo program given, so nothing was saved.")?;
                return Ok(None);
            }
            answers.push((ENV_COMMAND, program.trim().to_owned()));

            let arguments = ask(reader, writer, "Arguments (space separated)", "")?;
            let arguments: Vec<String> = arguments
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            if !arguments.is_empty() {
                // The configuration file takes a list, so the answers have to
                // be handed over in the shape the command path expects.
                answers.push((
                    ENV_COMMAND_ARGS,
                    serde_json::to_string(&arguments).unwrap_or_else(|_| "[]".to_owned()),
                ));
            }
        }
    }

    let lookup = |key: &str| {
        answers
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.clone())
    };

    let config = match EngineConfig::from_lookup(lookup) {
        Ok(config) => config,
        Err(err) => {
            writeln!(writer, "\nThat configuration is not usable: {err}")?;
            return Ok(None);
        }
    };

    match save_to {
        Some(path) => match config
            .to_document(true)
            .map_err(std::io::Error::other)
            .and_then(|document| crate::config::write_private(path, &document))
        {
            Ok(()) => {
                writeln!(writer, "\nSaved to {}.", path.display())?;
                writeln!(writer, "It is readable only by you. Delete it to start over.")?;
            }
            Err(err) => {
                writeln!(writer, "\nCould not save to {}: {err}", path.display())?;
                writeln!(writer, "Velora will use this configuration for this run only.")?;
            }
        },
        None => {
            writeln!(
                writer,
                "\nNo home directory to save into, so this configuration is for this run only."
            )?;
        }
    }

    Ok(Some(config))
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn answer(input: &str, save_to: Option<&Path>) -> (Option<EngineConfig>, String) {
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let config = run(&mut reader, &mut writer, save_to).expect("the prompt should not fail");
        (config, String::from_utf8_lossy(&writer).into_owned())
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("velora-setup-{tag}-{}.toml", std::process::id()))
    }

    #[test]
    fn an_api_provider_is_written_where_only_its_owner_can_read_it() {
        let path = scratch("api");
        let _ = std::fs::remove_file(&path);

        let (config, output) = answer("openai
gpt-4o-mini

sk-test-key
", Some(&path));
        let config = config.expect("a configuration was built");

        assert_eq!(config.describe(), "openai-compatible model=gpt-4o-mini base_url=https://api.openai.com/v1");
        assert!(output.contains("Saved to"), "{output}");

        let document = std::fs::read_to_string(&path).expect("the file was written");
        assert!(document.contains("gpt-4o-mini"), "{document}");
        assert!(document.contains("sk-test-key"), "the key is saved, by choice: {document}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the file holds a credential");
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_command_provider_is_written_as_a_program_and_arguments() {
        let path = scratch("command");
        let _ = std::fs::remove_file(&path);

        let (config, _output) = answer("command
claude
claude
-p --verbose
", Some(&path));
        let config = config.expect("a configuration was built");

        assert!(config.describe().starts_with("command model=claude"), "{}", config.describe());
        let document = std::fs::read_to_string(&path).unwrap();
        assert!(document.contains("claude"), "{document}");
        assert!(document.contains("--verbose"), "{document}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_endpoint_velora_knows_fills_in_its_own_endpoint_and_model() {
        // Whoever has a DeepSeek key has no way to know that "openai" is what
        // they are, which is what the question used to be. One word should be
        // enough.
        let path = scratch("deepseek");
        let _ = std::fs::remove_file(&path);

        let (config, output) = answer(
            "deepseek


sk-deepseek-key
",
            Some(&path),
        );
        let config = config.expect("a configuration was built");

        assert_eq!(
            config.describe(),
            "openai-compatible model=deepseek-flash base_url=https://api.deepseek.com"
        );

        let document = std::fs::read_to_string(&path).expect("the file was written");
        assert!(document.contains("deepseek-flash"), "{document}");
        assert!(document.contains("https://api.deepseek.com"), "{document}");
        assert!(document.contains("sk-deepseek-key"), "{document}");
        assert!(output.contains("Saved to"), "{output}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn what_velora_does_not_know_is_not_pretended_to_be_known() {
        let path = scratch("notknown");
        let (config, output) = answer("some-other-vendor\n", Some(&path));

        assert!(config.is_none());
        assert!(output.contains("not a provider Velora knows"), "{output}");
        assert!(!path.exists());
    }

    #[test]
    fn an_api_provider_without_a_key_saves_nothing() {
        let path = scratch("nokey");
        let (config, output) = answer("openai
gpt-4o-mini


", Some(&path));

        assert!(config.is_none());
        assert!(output.contains("Export"), "{output}");
        assert!(!path.exists());
    }

    #[test]
    fn an_unknown_provider_saves_nothing() {
        let path = scratch("unknown");
        let (config, output) = answer("telepathy
something
", Some(&path));

        assert!(config.is_none());
        assert!(output.contains("not a provider"), "{output}");
        assert!(!path.exists());
    }

    #[test]
    fn a_configuration_with_nowhere_to_go_is_still_used() {
        let (config, output) = answer("command
claude
claude

", None);

        assert!(config.is_some(), "the run can still use it");
        assert!(output.contains("this run only"), "{output}");
    }
}
