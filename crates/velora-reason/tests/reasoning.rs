//! A turn of thought: think, act, observe, think — each one an event.

#![cfg(unix)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use velora_config::{Settings, SettingsHandle};
use velora_event::{MemoryStore, Payload, Sensitivity, Sequence, Store, Timestamp};
use velora_exec::{Catalog, ExecutorStage, LocalDevice, StepRefused, StepRequest, StepResult};
use velora_llm::{
    Capabilities, Engine, EngineConfig, LiveEngine, Provider, ProviderError, Request, Response,
    RetryPolicy, Role, StopReason, ToolCall,
};
use velora_loop::{Event, Loop, Shutdown, event_channel};
use velora_reason::{ReasonConfig, ReasonFailed, ReasonFinished, ReasonRequest, ReasonerStage};

/// The name the model is *offered*, and therefore the only name it can ask
/// with. Providers constrain a function name to `^[a-zA-Z0-9_-]+$`, so the dot
/// in `local.shell.run` would be refused with a 400 before the model saw it.
const SHELL: &str = "local_shell_run";

/// What the same procedure is called in the record: the name a person reads on
/// the screen and in the log.
const SHELL_PROCEDURE: &str = "local.shell.run";

/// The pattern OpenAI-shaped APIs enforce on a function name, and the pattern
/// DeepSeek refused this system for: `local.shell.run` came back as
/// "Invalid 'tools[0].function.name': string does not match pattern".
fn acceptable(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn settings() -> SettingsHandle {
    let config = EngineConfig::from_lookup(|key: &str| match key {
        velora_llm::ENV_PROVIDER => Some("command".to_owned()),
        velora_llm::ENV_MODEL => Some("stub".to_owned()),
        velora_llm::ENV_COMMAND => Some("/bin/cat".to_owned()),
        _ => None,
    })
    .unwrap();

    SettingsHandle::new(Settings::baseline(&config))
}

/// What the scripted model does.
enum Reply {
    /// Ask for a step until something comes back, then conclude from it.
    ///
    /// This is enough to drive a whole turn, and it is how a real model
    /// behaves: act when it knows nothing, conclude when it has been told
    /// something.
    ActThenConclude {
        thought: String,
        procedure: String,
        arguments: String,
    },
    /// Ask for a step on every single turn of thought.
    AlwaysAct,
    /// Do something different every step, and stop the moment a review asks
    /// whether the work is still going somewhere.
    ConcludeAtReview,
    /// Work until told it is out of steps, then write down what is left.
    ///
    /// This is what a real model did: it was writing a script, testing it and
    /// setting it up, ran out of room mid-way, and lost all of it.
    ActThenWrapUp {
        procedure: String,
        arguments: String,
    },
    /// Ask for a step, then never answer again — a model that goes away.
    ActThenVanish {
        procedure: String,
        arguments: String,
    },
    /// Answer without looking at anything.
    Say(String),
}

struct Scripted {
    calls: Arc<AtomicUsize>,
    reply: Reply,
}

#[async_trait]
impl Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }

    fn model(&self) -> &str {
        "stub"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    async fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);

        // Every request that offers tools is checked here, so no test can pass
        // with a name a real provider would refuse. This is the assertion that
        // was missing when DeepSeek rejected the first real request.
        for tool in request.tools() {
            assert!(
                acceptable(tool.name()),
                "{:?} is not a name an OpenAI-shaped API accepts",
                tool.name()
            );
        }

        let observed = request
            .messages()
            .iter()
            .rev()
            .find(|message| message.role() == Role::Tool)
            .and_then(|message| message.content())
            .map(str::to_owned);

        let act = |thought: &str, procedure: &str, arguments: &str| {
            Ok(Response::new(
                Some(thought.to_owned()),
                vec![ToolCall::new("call_1", procedure, arguments)],
                "stub",
                Some(StopReason::ToolUse),
                None,
            ))
        };

        match &self.reply {
            Reply::ActThenConclude {
                thought,
                procedure,
                arguments,
            } => match observed {
                None => act(thought, procedure, arguments),
                Some(seen) => Ok(Response::new(
                    Some(format!("I found this: {}", seen.trim())),
                    Vec::new(),
                    "stub",
                    Some(StopReason::EndTurn),
                    None,
                )),
            },
            Reply::AlwaysAct => act("looking around", SHELL, r#"{"command":"echo still going"}"#),
            Reply::ConcludeAtReview => {
                let reviewed = request.messages().iter().any(|message| {
                    message.role() == Role::System
                        && message
                            .content()
                            .is_some_and(|text| text.contains("## Where this has got to"))
                });

                if reviewed {
                    return Ok(Response::new(
                        Some("I have looked at where this is going and it is finished.".to_owned()),
                        Vec::new(),
                        "stub",
                        Some(StopReason::EndTurn),
                        None,
                    ));
                }

                // A different command every time, so the circle check has nothing
                // to say about this turn and the only thing that can stop it is
                // the model answering the review.
                let n = self.calls.load(Ordering::SeqCst);
                act("working", SHELL, &format!(r#"{{"command":"echo step {n}"}}"#))
            }
            Reply::ActThenWrapUp {
                procedure,
                arguments,
            } => {
                // The instruction is in the system prompt, which is where a real
                // model reads it too.
                let finishing = request.messages().iter().any(|message| {
                    message.role() == Role::System
                        && message.content().is_some_and(|text| text.contains("last reply"))
                });

                if finishing {
                    return act("Writing down what is left.", procedure, arguments);
                }
                act("Still working on it.", SHELL, r#"{"command":"echo working"}"#)
            }
            Reply::ActThenVanish {
                procedure,
                arguments,
            } => {
                if observed.is_some() {
                    return Err(ProviderError::Transport("the model went away".to_owned()));
                }
                act("I will look", procedure, arguments)
            }
            Reply::Say(text) => Ok(Response::new(
                Some(text.clone()),
                Vec::new(),
                "stub",
                Some(StopReason::EndTurn),
                None,
            )),
        }
    }
}

fn script(reply: Reply) -> (Engine, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Scripted {
        calls: Arc::clone(&calls),
        reply,
    };

    (
        Engine::new(provider).with_retry_policy(RetryPolicy::none()),
        calls,
    )
}

fn request(task: &str) -> Event {
    Event::typed(
        "test",
        &ReasonRequest::new(task, Sequence::new(0), "Nothing is currently believed."),
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

fn catalog() -> Catalog {
    Catalog::new().with(LocalDevice::new())
}

async fn run(
    engine: Engine,
    settings: SettingsHandle,
    events: Vec<Event>,
    store: MemoryStore,
) -> MemoryStore {
    run_until(engine, settings, events, store, Stop::Never).await
}

/// When a test wants the loop stopped partway.
enum Stop {
    /// Let it finish on its own.
    Never,
    /// Stop once a record of this kind is in history, or when `within` is up.
    ///
    /// Waiting for the record rather than sleeping for a guess: a step runs a
    /// real command on a real machine, and "150 milliseconds is probably
    /// enough" fails on a loaded one. It did.
    WhenRecorded(&'static str, Duration),
}

/// Runs with a real executor, so steps actually happen on this machine.
async fn run_until(
    engine: Engine,
    settings: SettingsHandle,
    events: Vec<Event>,
    store: MemoryStore,
    stop: Stop,
) -> MemoryStore {
    let (sender, receiver) = event_channel(64);
    let mut loop_ = Loop::open(store, receiver).unwrap();
    let feedback = loop_.feedback();
    let history = loop_.history();

    loop_.add_stage(ExecutorStage::new(catalog()).with_feedback(feedback.clone()));
    loop_.add_stage(
        ReasonerStage::new(LiveEngine::new(engine), settings, catalog())
            .with_feedback(feedback)
            .with_config(
                ReasonConfig::default()
                    .with_retry_schedule(Duration::from_millis(1), Duration::from_millis(5)),
            ),
    );

    let shutdown = Shutdown::new();
    if let Stop::WhenRecorded(kind, within) = stop {
        let trigger = shutdown.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + within;

            loop {
                let seen = history
                    .replay()
                    .is_ok_and(|records| records.iter().any(|record| record.kind().as_str() == kind));
                if seen || tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }

            trigger.trigger();
        });
    }

    for event in events {
        sender.send(event).await.unwrap();
    }
    drop(sender);
    loop_.run(shutdown).await.unwrap();

    loop_.log().store().clone()
}

fn kinds(store: &MemoryStore) -> Vec<String> {
    store
        .read()
        .unwrap()
        .iter()
        .map(|record| record.kind().to_string())
        .collect()
}

fn count(store: &MemoryStore, kind: &str) -> usize {
    store
        .read()
        .unwrap()
        .iter()
        .filter(|record| record.kind().as_str() == kind)
        .count()
}

fn only<T: Payload + Clone>(store: &MemoryStore, kind: &str) -> T {
    let records = store.read().unwrap();
    let record = records
        .iter()
        .find(|record| record.kind().as_str() == kind)
        .unwrap_or_else(|| panic!("no {kind} in {:?}", kinds(store)));

    T::decode(record.version(), record.payload()).expect("it decodes")
}

fn turn_of(store: &MemoryStore) -> Sequence {
    store
        .read()
        .unwrap()
        .iter()
        .find(|record| record.kind().as_str() == ReasonRequest::KIND)
        .map(|record| record.sequence())
        .expect("the turn was opened")
}

// ------------------------------------------------------------------ the point

/// The loop is the agent loop: the thought produced a step, the step really
/// ran, its output came back, and the next thought was about it.
#[tokio::test]
async fn a_turn_thinks_acts_and_concludes() {
    let (engine, calls) = script(Reply::ActThenConclude {
        thought: "I will find out what is there first.".to_owned(),
        procedure: SHELL.to_owned(),
        arguments: r#"{"command":"echo hello from velora"}"#.to_owned(),
    });

    let store = run(engine, settings(), vec![request("say hello")], MemoryStore::new()).await;

    assert_eq!(
        kinds(&store),
        vec![
            ReasonRequest::KIND,
            "reason.thought",
            StepRequest::KIND,
            StepResult::KIND,
            ReasonFinished::KIND,
        ],
        "the order of the log is the order of the thought"
    );

    let result: StepResult = only(&store, StepResult::KIND);
    assert!(result.succeeded(), "{}", result.output());
    assert!(
        result.output().contains("hello from velora"),
        "{}",
        result.output()
    );

    let finished: ReasonFinished = only(&store, ReasonFinished::KIND);
    assert_eq!(finished.steps(), 1);
    assert!(
        finished.answer().contains("hello from velora"),
        "the conclusion must be about what actually happened: {}",
        finished.answer()
    );

    assert_eq!(calls.load(Ordering::SeqCst), 2, "one thought, one answer");
}

#[tokio::test]
async fn a_turn_that_needs_nothing_simply_answers() {
    let (engine, calls) = script(Reply::Say("Nothing here needs doing.".to_owned()));
    let store = run(engine, settings(), vec![request("is anything wrong?")], MemoryStore::new()).await;

    assert_eq!(
        kinds(&store),
        vec![ReasonRequest::KIND, ReasonFinished::KIND],
        "no step and no thinking aloud: just an answer"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let finished: ReasonFinished = only(&store, ReasonFinished::KIND);
    assert_eq!(finished.answer(), "Nothing here needs doing.");
}

/// A thought that keeps asking for the same thing is stopped.
///
/// There is no step limit any more: a turn ends when the model says it has, or
/// when a review finds it going in circles. The same procedure with the same
/// arguments, again and again, is the one kind of stuck that needs no judgement.
#[tokio::test]
async fn a_turn_that_repeats_itself_is_stopped() {
    let (engine, _calls) = script(Reply::AlwaysAct);
    let settings = SettingsHandle::new(settings().snapshot().with_review_steps(3));

    let store = run(engine, settings, vec![request("keep going")], MemoryStore::new()).await;

    // Three steps, then the review sees the same step three times and stops it —
    // and the last reply it is given to leave what is left is a step too.
    assert_eq!(
        count(&store, StepRequest::KIND),
        4,
        "a thought that keeps doing the same thing stops itself"
    );

    // It is not a failure: the turn concludes having written down where it got
    // to, which is the difference between being stopped and being thrown away.
    assert_eq!(count(&store, ReasonFailed::KIND), 0);
    let finished: ReasonFinished = only(&store, ReasonFinished::KIND);
    assert_eq!(finished.steps(), 4);
}

/// A turn is not stopped at the review — it is asked a question.
///
/// The model here does something different every step, and finishes as soon as
/// it is asked whether the work is still going somewhere. Reaching the review at
/// all is what makes this pass: it is the interval firing, and the answer to it
/// being the model's own conclusion rather than a stop.
#[tokio::test]
async fn a_turn_is_asked_at_the_review_rather_than_stopped() {
    let (engine, calls) = script(Reply::ConcludeAtReview);
    let settings = SettingsHandle::new(settings().snapshot().with_review_steps(3));

    let store = run(engine, settings, vec![request("do some work")], MemoryStore::new()).await;

    assert_eq!(
        count(&store, StepRequest::KIND),
        3,
        "it worked until it was asked, and then stopped because it was asked"
    );
    assert_eq!(count(&store, ReasonFailed::KIND), 0);

    let finished: ReasonFinished = only(&store, ReasonFinished::KIND);
    assert_eq!(
        finished.answer(),
        "I have looked at where this is going and it is finished."
    );

    // Four replies: three steps and the answer to the review.
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

/// A turn that was stopped writes down what is left.
///
/// The model that asked for files to be watched wrote a working script, tested
/// it, and ran out of room before setting up the source. Everything it did was
/// real and none of it was kept: the turn failed, and the next event started the
/// whole job again from the beginning. Now the last reply is where it writes
/// down what is left, and the plan it writes is what the next turn is handed.
#[tokio::test]
async fn a_turn_that_was_stopped_writes_down_what_is_left() {
    let (engine, calls) = script(Reply::ActThenWrapUp {
        procedure: SHELL.to_owned(),
        arguments: r#"{"command":"echo writing it down"}"#.to_owned(),
    });
    let settings = SettingsHandle::new(settings().snapshot().with_review_steps(3));

    let store = run(engine, settings, vec![request("watch my files")], MemoryStore::new()).await;

    assert_eq!(count(&store, ReasonFailed::KIND), 0, "being stopped is not a failure");

    let finished: ReasonFinished = only(&store, ReasonFinished::KIND);
    assert_eq!(
        finished.answer(),
        "Writing down what is left.",
        "what it said in its last reply is the answer, not an apology"
    );

    // Three steps of the same thing, then the one the last reply asked for.
    assert_eq!(count(&store, StepRequest::KIND), 4);
    let asked: Vec<String> = store
        .read()
        .expect("history")
        .iter()
        .filter_map(|record| record.decode_payload::<StepRequest>().ok())
        .map(|step| step.arguments().to_owned())
        .collect();
    assert!(
        asked.last().is_some_and(|last| last.contains("writing it down")),
        "the last thing it asked for is what it chose to do with its last reply: {asked:?}"
    );

    // Five replies: three steps of work, the one that was asking for the fourth
    // of the same thing — which is the reply the circle check throws away — and
    // the one answer to being stopped.
    assert_eq!(calls.load(Ordering::SeqCst), 5);
}

/// A model asking for something it was not offered is guessing, and is told so
/// rather than being allowed to reach past its brief.
#[tokio::test]
async fn a_procedure_that_is_not_on_offer_is_refused() {
    let (engine, _calls) = script(Reply::ActThenConclude {
        thought: "I will reach for something that does not exist.".to_owned(),
        procedure: "local.teleport".to_owned(),
        arguments: "{}".to_owned(),
    });

    let store = run(engine, settings(), vec![request("teleport")], MemoryStore::new()).await;

    let refused: StepRefused = only(&store, StepRefused::KIND);
    assert_eq!(refused.procedure(), "local.teleport");
    assert!(refused.reason().contains("on offer"), "{}", refused.reason());
    assert_eq!(
        count(&store, StepRequest::KIND),
        0,
        "nothing should have reached the executor at all"
    );
}

/// And what the system decided not to be offered is not offered.
#[tokio::test]
async fn a_disabled_procedure_is_refused_even_when_the_model_asks() {
    let (engine, _calls) = script(Reply::ActThenConclude {
        thought: "I will run a shell command.".to_owned(),
        procedure: SHELL.to_owned(),
        arguments: r#"{"command":"echo nope"}"#.to_owned(),
    });

    let settings =
        SettingsHandle::new(
            settings()
                .snapshot()
                .with_disabled_procedures([SHELL_PROCEDURE]),
        );

    let store = run(engine, settings, vec![request("run something")], MemoryStore::new()).await;

    let refused: StepRefused = only(&store, StepRefused::KIND);
    assert_eq!(refused.procedure(), SHELL);
    assert_eq!(
        count(&store, StepRequest::KIND),
        0,
        "a disabled procedure must not reach the executor"
    );
    assert_eq!(count(&store, StepResult::KIND), 0, "and must not have run");
}

/// The promise that makes a turn a turn: it survives being interrupted.
#[tokio::test]
async fn an_interrupted_turn_is_resumed_from_history() {
    // First run: it thinks, acts, and then the model goes away. The turn is
    // left open in history.
    let (engine, _calls) = script(Reply::ActThenVanish {
        procedure: SHELL.to_owned(),
        arguments: r#"{"command":"echo durable"}"#.to_owned(),
    });

    let store = run_until(
        engine,
        settings(),
        vec![request("find out what is here")],
        MemoryStore::new(),
        Stop::WhenRecorded(StepResult::KIND, Duration::from_secs(10)),
    )
    .await;

    assert_eq!(
        count(&store, ReasonFinished::KIND),
        0,
        "the turn should still be open: {:?}",
        kinds(&store)
    );
    let result: StepResult = only(&store, StepResult::KIND);
    assert!(result.output().contains("durable"));

    // Second run: a model that answers. The open turn is found in history and
    // carried on from what it had been given.
    let (engine, calls) = script(Reply::ActThenConclude {
        thought: "unused".to_owned(),
        procedure: SHELL.to_owned(),
        arguments: "{}".to_owned(),
    });

    let store = run(engine, settings(), Vec::new(), store).await;

    let finished: ReasonFinished = only(&store, ReasonFinished::KIND);
    assert!(
        finished.answer().contains("durable"),
        "a resumed turn must conclude from the step taken before the interruption: {}",
        finished.answer()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "it thought once more");
    assert_eq!(count(&store, ReasonFailed::KIND), 0, "it resumed, not gave up");
}

#[tokio::test]
async fn a_step_and_a_conclusion_know_their_turn() {
    let (engine, _calls) = script(Reply::ActThenConclude {
        thought: "looking".to_owned(),
        procedure: SHELL.to_owned(),
        arguments: r#"{"command":"echo hi"}"#.to_owned(),
    });

    let store = run(engine, settings(), vec![request("t")], MemoryStore::new()).await;
    let opened = turn_of(&store);

    let step: StepRequest = only(&store, StepRequest::KIND);
    assert_eq!(step.turn(), opened);

    let finished: ReasonFinished = only(&store, ReasonFinished::KIND);
    assert_eq!(finished.turn(), opened);
}

// ------------------------------------------------- a provider without tools

/// A model reached as a command has no notion of a tool call, so it writes the
/// same decision out. It must be a first-class way to think, not a degraded
/// one.
struct Writes {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for Writes {
    fn name(&self) -> &str {
        "writes"
    }

    fn model(&self) -> &str {
        "stub"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::TEXT_ONLY
    }

    async fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);

        let observed = request
            .messages()
            .iter()
            .rev()
            .find(|message| message.role() == Role::Tool)
            .and_then(|message| message.content());

        let system = request
            .messages()
            .iter()
            .find(|message| message.role() == Role::System)
            .and_then(|message| message.content())
            .unwrap_or("");

        assert!(
            system.contains(SHELL),
            "a provider that cannot be handed tools must be told what it can ask for"
        );
        assert!(
            !system.contains(SHELL_PROCEDURE),
            "and told it under the name a provider will accept, not the dotted one"
        );

        let body = match observed {
            None => format!(
                r#"{{"thought":"I will look","steps":[{{"procedure":"{SHELL}","arguments":{{"command":"echo written out"}}}}]}}"#
            ),
            Some(seen) => format!(r#"{{"answer":"I found this: {}"}}"#, seen.trim()),
        };

        Ok(Response::new(
            Some(body),
            Vec::new(),
            "stub",
            Some(StopReason::EndTurn),
            None,
        ))
    }
}

/// A model that panics once and then behaves. Not a stretch: a panic is what an
/// unwrap on an unexpected reply is, and providers return unexpected replies.
struct Panics {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for Panics {
    fn name(&self) -> &str {
        "panics"
    }

    fn model(&self) -> &str {
        "stub"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    async fn complete(&self, _request: &Request) -> Result<Response, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            panic!("the provider fell over");
        }

        Ok(Response::new(
            Some("second time lucky.".to_owned()),
            Vec::new(),
            "stub",
            Some(StopReason::EndTurn),
            None,
        ))
    }
}

/// A panic inside a thought must cost that thought, not the ability to think.
///
/// There is one worker for the whole process, so before this the panic unwound
/// the worker, every turn stayed open, `is_idle` never came true, and the loop
/// never stopped. The test hung rather than failed, which is how it was found.
#[tokio::test]
async fn a_panicking_model_fails_its_turn_instead_of_the_whole_system() {
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = Engine::new(Panics {
        calls: Arc::clone(&calls),
    })
    .with_retry_policy(RetryPolicy::none());

    let store = run(engine, settings(), vec![request("say hello")], MemoryStore::new()).await;

    // The turn ends, and it says why in a way someone can read.
    let failed: ReasonFailed = only(&store, ReasonFailed::KIND);
    assert!(
        failed.reason().contains("panicked"),
        "the panic must be reported, not absorbed: {}",
        failed.reason()
    );
    assert!(
        failed.reason().contains("the provider fell over"),
        "and it must say what the panic said: {}",
        failed.reason()
    );
}

#[tokio::test]
async fn a_provider_without_tools_can_still_think_in_steps() {
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = Engine::new(Writes {
        calls: Arc::clone(&calls),
    })
    .with_retry_policy(RetryPolicy::none());

    let store = run(engine, settings(), vec![request("find out")], MemoryStore::new()).await;

    assert_eq!(
        kinds(&store),
        vec![
            ReasonRequest::KIND,
            "reason.thought",
            StepRequest::KIND,
            StepResult::KIND,
            ReasonFinished::KIND,
        ],
        "the written step must be taken as a step: {:?}",
        kinds(&store)
    );

    let result: StepResult = only(&store, StepResult::KIND);
    assert!(result.output().contains("written out"), "{}", result.output());

    let finished: ReasonFinished = only(&store, ReasonFinished::KIND);
    assert!(
        finished.answer().contains("written out"),
        "{}",
        finished.answer()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
