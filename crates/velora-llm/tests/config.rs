//! Configuration, and the promise that a credential never renders.

use std::collections::HashMap;

use velora_llm::{
    ApiKey, ConfigError, DEFAULT_BASE_URL, ENV_API_KEY, ENV_BASE_URL, ENV_COMMAND,
    ENV_COMMAND_ARGS, ENV_MAX_ATTEMPTS, ENV_MODEL, ENV_NAME, ENV_PROVIDER, ENV_TIMEOUT_SECS,
    EngineConfig, ProviderChoice,
};

fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    move |key: &str| map.get(key).cloned()
}

#[test]
fn an_api_provider_defaults_to_the_public_endpoint() {
    let config = EngineConfig::from_lookup(lookup(&[
        (ENV_MODEL, "gpt-4o-mini"),
        (ENV_API_KEY, "sk-test"),
    ]))
    .unwrap();

    let ProviderChoice::OpenAiCompatible { base_url, model, .. } = config.provider() else {
        panic!("expected an API provider");
    };
    assert_eq!(base_url, DEFAULT_BASE_URL);
    assert_eq!(model, "gpt-4o-mini");
}

#[test]
fn a_different_endpoint_can_be_configured() {
    let config = EngineConfig::from_lookup(lookup(&[
        (ENV_MODEL, "llama3"),
        (ENV_API_KEY, "local"),
        (ENV_BASE_URL, "http://localhost:11434/v1"),
        (ENV_NAME, "ollama"),
    ]))
    .unwrap();

    assert!(config.describe().contains("http://localhost:11434/v1"));
    assert!(config.describe().starts_with("ollama"), "{}", config.describe());
}

#[test]
fn a_model_is_required() {
    let err = EngineConfig::from_lookup(lookup(&[(ENV_API_KEY, "sk-test")])).unwrap_err();
    assert_eq!(err, ConfigError::Missing(ENV_MODEL));
}

#[test]
fn an_api_provider_needs_a_credential() {
    let err = EngineConfig::from_lookup(lookup(&[(ENV_MODEL, "gpt-4o-mini")])).unwrap_err();
    assert_eq!(err, ConfigError::Missing(ENV_API_KEY));
}

#[test]
fn an_unknown_provider_is_reported_rather_than_guessed() {
    let err = EngineConfig::from_lookup(lookup(&[
        (ENV_PROVIDER, "telepathy"),
        (ENV_MODEL, "m"),
    ]))
    .unwrap_err();

    assert_eq!(err, ConfigError::UnknownProvider("telepathy".to_owned()));
    assert!(err.to_string().contains("openai"), "{err}");
}

#[test]
fn a_command_provider_parses_its_arguments() {
    let config = EngineConfig::from_lookup(lookup(&[
        (ENV_PROVIDER, "command"),
        (ENV_MODEL, "claude"),
        (ENV_COMMAND, "claude"),
        (ENV_COMMAND_ARGS, r#"["-p", "--output-format", "text"]"#),
    ]))
    .unwrap();

    let ProviderChoice::Command {
        program, args, model, ..
    } = config.provider()
    else {
        panic!("expected a command provider");
    };
    assert_eq!(program, "claude");
    assert_eq!(args, &["-p", "--output-format", "text"]);
    assert_eq!(model, "claude");
}

#[test]
fn command_arguments_that_are_not_a_json_array_are_reported() {
    let err = EngineConfig::from_lookup(lookup(&[
        (ENV_PROVIDER, "command"),
        (ENV_MODEL, "claude"),
        (ENV_COMMAND, "claude"),
        (ENV_COMMAND_ARGS, "-p --verbose"),
    ]))
    .unwrap_err();

    assert!(
        matches!(err, ConfigError::Invalid { ref key, .. } if key == ENV_COMMAND_ARGS),
        "{err}"
    );
}

#[test]
fn a_command_provider_needs_a_program() {
    let err = EngineConfig::from_lookup(lookup(&[
        (ENV_PROVIDER, "command"),
        (ENV_MODEL, "claude"),
    ]))
    .unwrap_err();
    assert_eq!(err, ConfigError::Missing(ENV_COMMAND));
}

#[test]
fn attempts_and_timeouts_are_configurable() {
    let config = EngineConfig::from_lookup(lookup(&[
        (ENV_MODEL, "m"),
        (ENV_API_KEY, "k"),
        (ENV_MAX_ATTEMPTS, "7"),
        (ENV_TIMEOUT_SECS, "45"),
    ]))
    .unwrap();

    assert_eq!(config.retry().max_attempts(), 7);
    assert_eq!(config.timeout().as_secs(), 45);
}

#[test]
fn a_nonsense_number_is_reported_not_ignored() {
    let err = EngineConfig::from_lookup(lookup(&[
        (ENV_MODEL, "m"),
        (ENV_API_KEY, "k"),
        (ENV_MAX_ATTEMPTS, "lots"),
    ]))
    .unwrap_err();

    assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
}

/// The system logs everything it does into a history it never rewrites, so a
/// credential that rendered into a log line would be permanent.
#[test]
fn a_credential_never_renders() {
    let secret = "sk-live-do-not-log-me";

    let key = ApiKey::new(secret);
    assert!(!format!("{key:?}").contains(secret));
    assert!(!format!("{key}").contains(secret));
    assert_eq!(key.expose(), secret);

    let config = EngineConfig::from_lookup(lookup(&[(ENV_MODEL, "m"), (ENV_API_KEY, secret)]))
        .unwrap();
    assert!(
        !config.describe().contains(secret),
        "describe() is meant to be loggable: {}",
        config.describe()
    );
    assert!(
        !format!("{config:?}").contains(secret),
        "Debug is meant to be loggable"
    );
    assert!(
        !format!("{:?}", config.provider()).contains(secret),
        "the provider choice Debug must be loggable too"
    );
}

// ---------------------------------------------------------------- documents

#[test]
fn a_document_configures_a_provider() {
    let document = r#"
[llm]
provider = "openai"
model = "gpt-4o-mini"
base_url = "https://example.test/v1"
api_key = "from-file"
"#;

    let config = EngineConfig::from_documents(None, Some(document), lookup(&[])).unwrap();
    assert_eq!(
        config.describe(),
        "openai-compatible model=gpt-4o-mini base_url=https://example.test/v1"
    );
}

#[test]
fn a_project_document_overrides_the_machine_document() {
    let machine = r#"
[llm]
provider = "openai"
model = "machine-model"
base_url = "https://machine.test/v1"
api_key = "machine-key"
"#;
    let project = r#"
[llm]
model = "project-model"
"#;

    let config = EngineConfig::from_documents(Some(machine), Some(project), lookup(&[])).unwrap();

    // The project wins where it speaks, and the machine still supplies the
    // credential it alone holds.
    assert_eq!(
        config.describe(),
        "openai-compatible model=project-model base_url=https://machine.test/v1"
    );
    let ProviderChoice::OpenAiCompatible { api_key, .. } = config.provider() else {
        panic!("expected an API provider");
    };
    assert_eq!(api_key.expose(), "machine-key");
}

#[test]
fn the_environment_overrides_both_documents() {
    let machine = "[llm]\nmodel = \"machine\"\napi_key = \"k\"\n";
    let project = "[llm]\nmodel = \"project\"\n";

    let config = EngineConfig::from_documents(
        Some(machine),
        Some(project),
        lookup(&[(ENV_MODEL, "from-environment")]),
    )
    .unwrap();

    assert!(config.describe().contains("model=from-environment"));
}

/// A misspelled key that silently did nothing would be the exact failure the
/// file exists to prevent.
#[test]
fn a_misspelled_key_is_refused() {
    let document = "[llm]\nmodell = \"gpt-4o-mini\"\napi_key = \"k\"\n";
    let err = EngineConfig::from_documents(None, Some(document), lookup(&[])).unwrap_err();

    assert!(matches!(err, ConfigError::Malformed(_)), "{err}");
    assert!(err.to_string().contains("modell"), "{err}");
}

#[test]
fn a_malformed_document_is_refused() {
    let err =
        EngineConfig::from_documents(None, Some("[llm\nmodel = "), lookup(&[])).unwrap_err();
    assert!(matches!(err, ConfigError::Malformed(_)), "{err}");
}

#[test]
fn an_api_configuration_round_trips_through_a_document() {
    let config = EngineConfig::from_lookup(lookup(&[
        (ENV_MODEL, "gpt-4o-mini"),
        (ENV_API_KEY, "sk-round-trip"),
        (ENV_BASE_URL, "https://example.test/v1"),
        (ENV_NAME, "example"),
    ]))
    .unwrap();

    let document = config.to_document(true).unwrap();
    let again = EngineConfig::from_documents(None, Some(&document), lookup(&[])).unwrap();

    assert_eq!(again.describe(), config.describe());
    assert!(document.contains("sk-round-trip"));
}

#[test]
fn a_command_configuration_round_trips_through_a_document() {
    let config = EngineConfig::from_lookup(lookup(&[
        (ENV_PROVIDER, "command"),
        (ENV_MODEL, "claude"),
        (ENV_COMMAND, "claude"),
        (ENV_COMMAND_ARGS, r#"["-p","--verbose"]"#),
    ]))
    .unwrap();

    let document = config.to_document(false).unwrap();
    let again = EngineConfig::from_documents(None, Some(&document), lookup(&[])).unwrap();

    assert_eq!(again.describe(), config.describe());
    assert!(again.describe().contains("--verbose"), "{}", again.describe());
}

#[test]
fn a_document_can_be_written_without_its_secret() {
    let config = EngineConfig::from_lookup(lookup(&[
        (ENV_MODEL, "gpt-4o-mini"),
        (ENV_API_KEY, "sk-must-not-be-written"),
    ]))
    .unwrap();

    let document = config.to_document(false).unwrap();
    assert!(!document.contains("sk-must-not-be-written"), "{document}");

    // Everything else survives, so the rest of the file is still useful.
    let err = EngineConfig::from_documents(None, Some(&document), lookup(&[])).unwrap_err();
    assert_eq!(err, ConfigError::Missing(ENV_API_KEY));
}
