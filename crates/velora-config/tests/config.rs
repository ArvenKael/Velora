//! What the system may decide about itself, and what it may not.

use std::collections::HashMap;

use velora_config::{
    COMMAND, MAX_STEPS, MODEL, PERSONA, PROVIDER, Setting, SettingError, Settings, SettingsHandle,
    StewardStage, ConfigChange, ConfigRefused,
};
use velora_event::{
    MemoryStore, Payload, PayloadVersion, Record, Sensitivity, Store, Timestamp,
};
use velora_llm::{EngineConfig, LiveEngine};
use velora_loop::{Event, Loop, Shutdown, event_channel};

fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    move |key: &str| map.get(key).cloned()
}

fn baseline(program: &str, model: &str) -> Settings {
    let config = EngineConfig::from_lookup(lookup(&[
        (velora_llm::ENV_PROVIDER, "command"),
        (velora_llm::ENV_MODEL, model),
        (velora_llm::ENV_COMMAND, program),
    ]))
    .unwrap();
    Settings::baseline(&config)
}

fn change(key: &str, value: &str) -> Event {
    let change = ConfigChange::new(key, value).expect("a settable change");
    Event::typed(
        "test",
        &change,
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

/// The payload for a change as an outside writer would produce it, straight
/// into the log and past the constructor that makes this impossible here.
fn raw_change(key: &str, value: &str) -> Vec<u8> {
    let mut writer = velora_event::codec::Writer::new();
    writer.string(key);
    writer.string(value);
    writer.finish()
}

/// A change arriving live, as an adapter that did not go through the
/// constructor would send it.
fn raw_change_event(key: &str, value: &str) -> Event {
    Event::new(
        "test",
        ConfigChange::KIND,
        PayloadVersion::new(1),
        raw_change(key, value),
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}


async fn run(steward: StewardStage, events: Vec<Event>, store: MemoryStore) -> MemoryStore {
    let (sender, receiver) = event_channel(64);
    let mut loop_ = Loop::open(store, receiver).unwrap();
    loop_.add_stage(steward);

    for event in events {
        sender.send(event).await.unwrap();
    }
    drop(sender);
    loop_.run(Shutdown::new()).await.unwrap();

    loop_.log().store().clone()
}

fn refusals(store: &MemoryStore) -> Vec<Record> {
    store
        .read()
        .unwrap()
        .into_iter()
        .filter(|record| record.kind().as_str() == ConfigRefused::KIND)
        .collect()
}

// --------------------------------------------------------------- the vocabulary

#[test]
fn every_setting_round_trips_through_its_text_form() {
    let cases = [
        (MODEL, "a-model"),
        (PROVIDER, "command"),
        (velora_config::BASE_URL, "http://localhost:11434/v1"),
        (COMMAND, "claude"),
        (velora_config::COMMAND_ARGS, r#"["-p","--verbose"]"#),
        (PERSONA, "You are careful with other people's machines."),
        (MAX_STEPS, "12"),
        (velora_config::DISABLED_PROCEDURES, "local.fs.remove,local.shell.run"),
    ];

    for (key, value) in cases {
        let setting = Setting::parse(key, value).unwrap_or_else(|err| panic!("{key}: {err}"));
        assert_eq!(setting.key(), key);
        let again = Setting::parse(setting.key(), &setting.value()).unwrap();
        assert_eq!(again, setting, "{key} did not round-trip");
    }
}

/// The boundary that makes self-configuration safe to allow at all. The event
/// cannot be built carrying a credential, because by the time anything could
/// refuse it, history would already have it.
#[test]
fn a_credential_cannot_be_put_into_an_event() {
    for key in ["llm.api_key", "llm.token", "llm.secret", "llm.password"] {
        let err = ConfigChange::new(key, "sk-live-secret").unwrap_err();
        assert!(matches!(err, SettingError::Credential { .. }), "{key}: {err}");
        assert!(
            err.to_string().contains("append-only"),
            "the refusal should say why: {err}"
        );
    }

    // And the same boundary governs the setting vocabulary itself.
    assert!(Setting::parse("llm.api_key", "sk-live-secret").is_err());
}

#[test]
fn a_key_nobody_has_thought_about_is_refused() {
    let err = Setting::parse("llm.temperature", "0.5").unwrap_err();
    assert!(matches!(err, SettingError::Unknown { .. }), "{err}");
}

#[test]
fn a_value_that_cannot_be_used_is_refused() {
    assert!(Setting::parse(MODEL, "  ").is_err(), "an empty model");
    assert!(Setting::parse(MAX_STEPS, "0").is_err(), "no steps at all");
    assert!(Setting::parse(MAX_STEPS, "10000").is_err(), "no ceiling at all");
    assert!(Setting::parse(MAX_STEPS, "lots").is_err(), "not a number");
    assert!(Setting::parse(PROVIDER, "telepathy").is_err(), "not a provider");
    assert!(
        Setting::parse(velora_config::COMMAND_ARGS, "-p --verbose").is_err(),
        "arguments are a JSON array"
    );
}

// ------------------------------------------------------------------- the fold

#[test]
fn settings_fold_changes_over_a_baseline() {
    let mut settings = baseline("/bin/cat", "original");
    assert_eq!(settings.model(), "original");
    settings.apply(&Setting::parse(MODEL, "second").unwrap());
    settings.apply(&Setting::parse(PERSONA, "be careful").unwrap());

    assert_eq!(settings.model(), "second");
    assert_eq!(settings.persona(), "be careful");
    assert_eq!(settings.provider(), "command", "untouched settings stay");
}

/// What a restart sees must be what the system decided, not what the file said.
#[test]
fn a_restart_sees_what_the_system_decided() {
    let mut log = velora_event::EventLog::open(MemoryStore::new()).unwrap();
    log.append(velora_event::RecordDraft::new(
        ConfigChange::KIND,
        PayloadVersion::new(1),
        raw_change(MODEL, "chosen-by-itself"),
        Timestamp::from_parts(1, 0),
        Sensitivity::INTERNAL,
    ))
    .unwrap();
    let history = log.replay().unwrap();

    let resumed = baseline("/bin/cat", "from-the-file").fold(&history);

    assert_eq!(resumed.model(), "chosen-by-itself");
    assert_eq!(resumed.provider(), "command", "the file still supplies the rest");
}

#[test]
fn a_refused_change_is_refused_again_when_history_is_folded() {
    let mut log = velora_event::EventLog::open(MemoryStore::new()).unwrap();
    log.append(velora_event::RecordDraft::new(
        ConfigChange::KIND,
        PayloadVersion::new(1),
        raw_change("llm.api_key", "sk-never"),
        Timestamp::from_parts(1, 0),
        Sensitivity::INTERNAL,
    ))
    .unwrap();

    let resumed = baseline("/bin/cat", "model").fold(&log.replay().unwrap());
    assert_eq!(resumed.provider(), "command");
    assert_eq!(resumed.model(), "model", "the refusal changed nothing");
}

#[test]
fn switching_to_an_api_provider_with_no_credential_cannot_work() {
    let settings = baseline("/bin/cat", "model");
    let mut trial = settings.clone();
    trial.apply(&Setting::parse(PROVIDER, "openai").unwrap());

    let err = trial.to_engine_config().unwrap_err();
    assert!(
        err.to_string().contains("credentials are never set by events"),
        "{err}"
    );
}

// ------------------------------------------------------------------ the steward

#[tokio::test]
async fn the_system_can_change_the_model_it_thinks_with() {
    let settings = SettingsHandle::new(baseline("/bin/cat", "first"));
    let engine = settings
        .snapshot()
        .to_engine_config()
        .unwrap()
        .build()
        .unwrap();
    let live = LiveEngine::new(engine);
    assert_eq!(live.model(), "first");

    run(
        StewardStage::new(settings.clone()).with_engine(live.clone()),
        vec![change(MODEL, "second")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(live.model(), "second", "the engine in force changed");
    assert_eq!(settings.model(), "second");
}

#[tokio::test]
async fn a_persona_change_does_not_go_near_the_engine() {
    let settings = SettingsHandle::new(baseline("/bin/cat", "model"));
    let engine = settings
        .snapshot()
        .to_engine_config()
        .unwrap()
        .build()
        .unwrap();
    let live = LiveEngine::new(engine);

    run(
        StewardStage::new(settings.clone()).with_engine(live.clone()),
        vec![change(PERSONA, "be careful with other people's machines")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(settings.persona(), "be careful with other people's machines");
    assert_eq!(live.model(), "model");
}

/// A system that quietly declined to become something would look, afterwards,
/// like a system nobody asked.
#[tokio::test]
async fn a_refused_change_is_recorded() {
    let settings = SettingsHandle::new(baseline("/bin/cat", "model"));

    // A value the constructor would refuse, arriving from elsewhere.
    let store = run(
        StewardStage::new(settings.clone()),
        vec![raw_change_event(MAX_STEPS, "0")],
        MemoryStore::new(),
    )
    .await;

    let refused = refusals(&store);
    assert_eq!(refused.len(), 1);
    let refusal = refused[0].decode_payload::<ConfigRefused>().unwrap();
    assert_eq!(refusal.key(), MAX_STEPS);
    assert!(refusal.reason().contains("between 1 and"), "{}", refusal.reason());
    assert_eq!(settings.review_steps(), velora_config::DEFAULT_MAX_STEPS);
}

/// A credential that arrives from somewhere less careful is refused, and the
/// refusal does not repeat it.
#[tokio::test]
async fn a_credential_that_arrives_anyway_is_refused_and_not_echoed() {
    let settings = SettingsHandle::new(baseline("/bin/cat", "model"));
    let secret = "sk-live-must-not-be-echoed";

    let store = run(
        StewardStage::new(settings.clone()),
        vec![raw_change_event("llm.api_key", secret)],
        MemoryStore::new(),
    )
    .await;

    // The request event itself carries it, and nothing can undo that: the loop
    // appends before any stage sees the event, so history is written before
    // anything could refuse. The real defence is that this crate's own code
    // cannot build such an event; the backstop below is about never *acting* on
    // one.
    let carried = store
        .read()
        .unwrap()
        .iter()
        .filter(|record| record.kind().as_str() == ConfigChange::KIND)
        .any(|record| String::from_utf8_lossy(record.payload()).contains(secret));
    assert!(carried, "the request is in history before anyone could refuse it");

    let refused = refusals(&store);
    assert_eq!(refused.len(), 1);
    let refusal = refused[0].decode_payload::<ConfigRefused>().unwrap();
    assert_eq!(refusal.value(), "(not recorded)");
    assert!(refusal.reason().contains("append-only"));

    let echoed = refused
        .iter()
        .any(|record| String::from_utf8_lossy(record.payload()).contains(secret));
    assert!(!echoed, "a refusal must not repeat the credential");
}

#[tokio::test]
async fn an_unusable_engine_change_leaves_the_settings_alone() {
    let settings = SettingsHandle::new(baseline("/bin/cat", "model"));
    let engine = settings
        .snapshot()
        .to_engine_config()
        .unwrap()
        .build()
        .unwrap();
    let live = LiveEngine::new(engine);

    let store = run(
        StewardStage::new(settings.clone()).with_engine(live.clone()),
        vec![raw_change_event(COMMAND, "")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(refusals(&store).len(), 1);
    assert_eq!(settings.model(), "model", "nothing half-applied");
    assert_eq!(live.model(), "model");
    assert_eq!(
        live.current().stats().attempts,
        0,
        "and nothing was asked of any provider"
    );
}
