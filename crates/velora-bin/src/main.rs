//! The Velora process entry point.
//!
//! This is the runtime half of the split the project is built around: it owns
//! everything OS-facing — the history file, Ctrl-C, the clock, the environment —
//! and hands the OS-agnostic loop a store, a channel and a shutdown handle. The
//! loop knows nothing about signals, paths or timers.
//!
//! The pipeline wired here is the smallest one that shows the architecture
//! working end to end:
//!
//! ```text
//!   observation ──▶ append to history ──▶ belief-judge ──▶ belief.* ──▶ beliefs
//!   (an adapter)     (durable first)       │                            (fold)
//!                                          ├─ a rule claimed the kind: decide now
//!                                          └─ otherwise: ask the model, and
//!                                             record the decision it made
//! ```
//!
//! Note what the runtime no longer does: it defines no interpretation of its
//! own. What an event means is either a rule or a model, both supplied here as
//! configuration, and the loop stays a loop.

use std::sync::Arc;
use std::time::Duration;

mod config;
mod explain;
mod setup;
mod storage;

use velora_agent::{EventWatch, ResolverStage, Watch};
use velora_belief::{BeliefHandle, BeliefStage, Confidence};
use velora_capability::{CapabilityStage, Capabilities, Limits, Observation};
use velora_config::{Settings, SettingsHandle, StewardStage};
use velora_event::{Payload, PayloadVersion, Sensitivity, Store, Timestamp};
use velora_context::Explain;
use velora_operator::{OperatorChannel, Said};
use velora_exec::{Catalog, Device, ExecutorConfig, ExecutorStage, LocalDevice};
use velora_llm::{Engine, LiveEngine};
use velora_log::{error, info, warn};
use velora_web::{View, about};
use velora_loop::{Event, Loop, Shutdown, event_channel};
use velora_mind::{JudgeConfig, JudgeStage, KeyValueRule, Wake};
use velora_plan::{PlanStage, Plans, StepReady};
use velora_pulse::Pulse;
use velora_reason::{ReasonerStage, protocol};
use velora_storage::AnyStore;
/// Where the screen is served unless `VELORA_UI` says otherwise.
///
/// Loopback only. The page can run shell commands on this machine through the
/// system it is showing, so it is not something to expose on a network by
/// accident — reaching it from elsewhere is a deliberate act, not a default.
const DEFAULT_UI: &str = "127.0.0.1:7788";

/// Kinds the demo distinguishes.
const SETTING: &str = "observation.setting";
const NOTE: &str = "observation.note";

/// What the demo source observes, in order.
///
/// Settings are decided by a prespecified rule, so they cost nothing and are
/// always interpreted the same way. The note has no rule, so it is what the
/// model is for. The two `theme` settings contradict each other, so even a short
/// run shows a belief being revised.
const DEMO_OBSERVATIONS: [(&str, &str); 4] = [
    (SETTING, "theme=dark"),
    (SETTING, "locale=en-GB"),
    (NOTE, "the operator keeps asking about the weather"),
    (SETTING, "theme=light"),
];

/// How many observations the placeholder source produces before stopping.
///
/// `None` means it does not run at all, which is the default and should be: a
/// real system must not be generating invented observations about an operator
/// who keeps asking about the weather. Those are events, they are judged, and
/// they cost model calls — and they arrive ahead of, and compete with, the
/// person actually talking to it.
///
/// `VELORA_DEMO_EVENTS=N` produces N and stops. `VELORA_DEMO=1` runs until
/// interrupted, for watching the loop move.
fn demo_limit() -> Option<u64> {
    if let Some(count) = std::env::var("VELORA_DEMO_EVENTS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        return Some(count);
    }

    std::env::var("VELORA_DEMO")
        .ok()
        .filter(|value| !value.is_empty() && value != "0")
        .map(|_| u64::MAX)
}

/// The pulse, or nothing if it was turned off.
///
/// `VELORA_PULSE=off` runs a system that only thinks when something happens;
/// `VELORA_PULSE=<minutes>` sets how long it waits between quiet thoughts, and
/// the quiet time is a third of that. On by default, because a system that never
/// thinks unless spoken to is the thing this exists to change — and off by a
/// word, because it is a model call.
fn pulse() -> Option<Pulse> {
    match std::env::var("VELORA_PULSE") {
        Ok(asked) if asked.eq_ignore_ascii_case("off") || asked == "0" => None,
        Ok(asked) => match asked.parse::<u64>() {
            Ok(minutes) => {
                let every = Duration::from_secs(minutes.max(1) * 60);
                Some(
                    Pulse::default()
                        .with_every(every)
                        .with_quiet(every / 3),
                )
            }
            Err(_) => {
                warn!(
                    asked = %asked,
                    "VELORA_PULSE is not `off` or a number of minutes, so the pulse runs at its default"
                );
                Some(Pulse::default())
            }
        },
        Err(_) => Some(Pulse::default()),
    }
}

/// How many things the system may work on at once.
///
/// One is the old behaviour and the safe default: everything in turn, in the
/// order it arrived. More than one is what "install this while I ask you
/// something" needs, and what it costs is that the system is genuinely doing two
/// things — two model calls, two procedures, and two answers that may arrive in
/// either order. `VELORA_PARALLEL` sets it; the ceiling is there because
/// attention is finite and this is the number of ways it can be split.
fn parallel() -> usize {
    const CEILING: usize = 8;

    match std::env::var("VELORA_PARALLEL").ok().and_then(|asked| asked.parse::<usize>().ok()) {
        Some(asked) => asked.clamp(1, CEILING),
        None => 3,
    }
}

/// How certain a model's judgement is allowed to be.
///
/// The default ceiling is below certainty on purpose: a model is reading the
/// operator's words, not measuring them, so what it concludes must not outrank
/// what a rule established from the event itself. But an operator who tells the
/// system something about themselves is the only source there will ever be, and
/// `VELORA_MAX_BELIEF_CONFIDENCE=10000` (or `certain`) is how to say that such a
/// statement may be held outright.
fn belief_ceiling() -> Option<Confidence> {
    let asked = std::env::var("VELORA_MAX_BELIEF_CONFIDENCE").ok()?;

    if asked.eq_ignore_ascii_case("certain") {
        return Some(Confidence::CERTAIN);
    }

    match asked
        .trim()
        .parse::<u16>()
        .ok()
        .and_then(|basis_points| Confidence::from_basis_points(basis_points).ok())
    {
        Some(confidence) => Some(confidence),
        None => {
            error!(
                asked = %asked,
                "VELORA_MAX_BELIEF_CONFIDENCE is not 0..=10000, so the default ceiling is used"
            );
            None
        }
    }
}

/// Where to serve the screen, or nothing if it was turned off.
///
/// `VELORA_UI=off` runs the system with no screen, which is what a script or a
/// test wants. Anything else is `host:port`.
fn ui_address() -> Option<std::net::SocketAddr> {
    let asked = std::env::var("VELORA_UI").unwrap_or_else(|_| DEFAULT_UI.to_owned());

    if asked.eq_ignore_ascii_case("off") || asked.is_empty() {
        return None;
    }

    match asked.parse() {
        Ok(addr) => Some(addr),
        Err(err) => {
            error!(
                asked = %asked,
                error = %err,
                "VELORA_UI is not host:port, so no screen is being served"
            );
            None
        }
    }
}

/// Prints what is believed, and why.
fn report(
    beliefs: &BeliefHandle,
    engine: Option<&Engine>,
    velora: &Loop<AnyStore>,
) -> Result<(), Box<dyn std::error::Error>> {
    // What it did about what it believed, read straight out of history.
    let history = velora.log().replay()?;
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for record in &history {
        *counts.entry(record.kind().as_str()).or_default() += 1;
    }
    let did: Vec<String> = counts
        .iter()
        .filter(|(kind, _)| {
            kind.starts_with("reason.") || kind.starts_with("step.") || kind.starts_with("config.")
        })
        .map(|(kind, count)| format!("{kind}={count}"))
        .collect();
    if !did.is_empty() {
        info!(what_it_did = %did.join(" "), "the loop at work");
    }

    for belief in beliefs.beliefs() {
        let cited: Vec<String> = belief
            .evidence()
            .resolve(&velora.log().replay()?)?
            .iter()
            .map(|record| {
                format!(
                    "#{} {}",
                    record.sequence(),
                    String::from_utf8_lossy(record.payload())
                )
            })
            .collect();

        info!(
            key = %belief.key(),
            value = %String::from_utf8_lossy(belief.value().bytes()),
            confidence = %belief.confidence(),
            contested = beliefs.is_contested(belief.key()),
            rationale = belief.rationale(),
            evidence = %cited.join(" | "),
            "belief held"
        );
    }

    let stats = beliefs.stats();
    info!(
        held = stats.held,
        accepted = stats.accepted,
        refused = stats.refused,
        "belief store"
    );

    if let Some(engine) = engine {
        let stats = engine.stats();
        info!(
            provider = engine.provider_name(),
            model = engine.model(),
            attempts = stats.attempts,
            retries = stats.retries,
            failures = stats.failures,
            input_tokens = stats.input_tokens,
            output_tokens = stats.output_tokens,
            "llm engine"
        );
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Install the global logger before anything else. Filtering is controlled
    // by the VELORA_LOG environment variable (e.g. `VELORA_LOG=debug`).
    velora_log::init();

    // Configuration can be asked for rather than waited for. The first-run
    // prompt only appears when there is nothing configured and a terminal to ask
    // on, which leaves someone who *wants* to give Velora a key with no way to
    // do it but edit a file they have to be told about. This is that way.
    if std::env::args().skip(1).any(|arg| arg == "--setup") {
        return match config::configure().await {
            Ok(()) => {
                info!("configuration saved; start Velora again to use it");
                Ok(())
            }
            Err(problem) => {
                error!(problem = %problem, "nothing was configured");
                Err(problem.into())
            }
        };
    }

    // Where history lives: the backend the environment chose, opened, and
    // filled from an existing file history the first time a database is used
    // instead of one. Everything below this line is written against the store
    // seam and does not know which backend it got.
    let (sender, events) = event_channel(1024);
    let mut velora = Loop::open(storage::open()?, events)?;

    {
        let log = velora.log();
        let store = log.store();
        let records = log.len();

        match store.recovery() {
            // This backend can lose a tail, and it is telling us it did.
            Some(recovery) if !recovery.is_clean() => warn!(
                records,
                store = %store.describe(),
                recovery = %recovery,
                "history recovered with a discarded tail"
            ),
            Some(_) => info!(
                records,
                store = %store.describe(),
                durability = %store.durability(),
                "history recovered"
            ),
            // Not "clean": a backend that commits cannot tear, so there is no
            // tail to have an opinion about.
            None => info!(
                records,
                store = %store.describe(),
                durability = %store.durability(),
                "history opened; this backend commits each record whole"
            ),
        }
    }

    // Where the model comes from: the machine-wide file, the project file, the
    // environment, or a first-run prompt. Read once, at the start.
    //
    // Velora runs without a model rather than pretending. Rules still decide
    // what they cover, and events that need a model are held for it.
    let model = match config::resolve().await {
        Ok(config::Model::Ready { engine, config }) => {
            info!(
                provider = engine.provider_name(),
                model = engine.model(),
                streaming = engine.capabilities().streaming,
                tools = engine.capabilities().tools,
                "llm engine ready"
            );

            // What the system is configured to be is the configuration files,
            // with every decision it has recorded about itself folded over
            // them — so a restart resumes the system the system chose.
            let baseline = Settings::baseline(&config)
                .with_persona(protocol::DEFAULT_PERSONA.to_owned());
            let settings = SettingsHandle::new(baseline);
            let live = LiveEngine::new(*engine);

            Some((settings, live))
        }
        Ok(config::Model::Absent(reason)) => {
            warn!(
                reason = %reason,
                "running without a model: rules still decide what they cover, and events that \
                 need a model are held until one is available"
            );
            // How to fix it, not only that it is broken. Saying "no model" and
            // stopping there is how a system runs for a week without anyone
            // realising it was never asked for a key.
            info!(
                configuration = %config::machine_path().map_or_else(
                    || "~/.config/velora/config.toml".to_owned(),
                    |path| path.display().to_string(),
                ),
                "to give Velora a model, run this again with --setup, or write an [llm] section \
                 into that file (see velora.toml.example)"
            );
            None
        }
        Err(fatal) => {
            error!(problem = %fatal, "Velora will not run half-configured");
            std::process::exit(1);
        }
    };

    // The belief store is derived state: on start-up its stage replays history
    // and recomputes it. The judge is where new events get decided, and its
    // decisions are what the store folds.
    //
    // The judge is handed the loop's feedback channel because a model call is
    // slow and the loop is serial: it asks off to the side and the answer comes
    // back through the loop, rather than every event waiting behind a thought.
    let beliefs = BeliefHandle::new();
    let feedback = velora.feedback();
    // The catalog is what the system can do, and speaking to the operator is one
    // of the things it can do. It is here rather than implicit so that answering
    // is a decision — the same decision as running a command — rather than
    // something that happens to a thought when it ends.
    //
    // And what the system can do is not fixed. `capabilities` is the one device
    // that grows while the system runs: a procedure it writes for itself becomes
    // a tool in the next thought, and a source it sets up keeps reporting. The
    // catalog asks each device what it can do on every lookup rather than
    // caching, so nothing has to be reloaded for that to take effect.
    let capabilities = Arc::new(Capabilities::new().with_limits(
        Limits::default().with_max_observers(8),
    ));

    // How many things at once, read once and used by both stages that need to
    // agree about it: more turns open than procedures that may run is a queue,
    // and the other way round is a limit that never binds.
    let parallel = parallel();
    if parallel > 1 {
        info!(parallel, "the system will work on several things at once");
    }

    // And what it is in the middle of. A plan is not a turn: a turn ends, and a
    // plan is what is still there afterwards — including after a restart, which
    // is why the stage that holds them hands out the next step at start.
    let plans = Arc::new(Plans::new());

    let mut catalog = Catalog::new()
        .with(LocalDevice::new())
        .with(OperatorChannel);
    catalog.add(Arc::clone(&capabilities) as Arc<dyn Device>);
    catalog.add(Arc::clone(&plans) as Arc<dyn Device>);

    // Speaking is a procedure, and a provider is offered procedures under names
    // it will accept — which means the dots are flattened. Naming it in one place
    // keeps the offer and the task from disagreeing about it.
    let channel = velora_reason::protocol::wire_name(velora_operator::SAY_PROCEDURE);

    // The one thing in the runtime that knows what every record says, shared by
    // the model's context and by the screen, so the two cannot describe the same
    // event differently.
    let explain: Arc<dyn Explain> = Arc::new(explain::Vocabulary);

    let mut pipeline = Vec::new();

    let mut judge = JudgeStage::new(beliefs.clone())
        .with_feedback(feedback.clone())
        // A setting states itself, so it is decided by rule and never costs a
        // request. This is the "prespecified" path.
        .with_rule(KeyValueRule::new(
            SETTING,
            Confidence::from_basis_points(9_000).unwrap_or(Confidence::CERTAIN),
        ))
        // A free-form note has to be thought about.
        .with_candidate(NOTE)
        // And so does something the operator says. Not because every remark is
        // worth believing, but because the alternative is a system that holds a
        // conversation and learns nothing from it: the transcript tells the
        // model what was said recently, and beliefs are what it still knows
        // after the transcript has moved on.
        .with_candidate(Said::KIND)
        // And so does what a source the system set up reports. An observation is
        // an event like any other: it is judged into beliefs, which is how the
        // system learns from a camera it configured rather than only hearing
        // from it. Whether a report is worth *acting* on is a separate decision,
        // and one this stage does not make.
        .with_candidate(Observation::KIND);

    // The judge decides what an event means, so it has to be shown the event
    // rather than its encoding: a message read as bytes arrives with its length
    // prefix on the front, and "what does this mean for what Velora believes" is
    // not a question anyone can answer about that.
    judge = judge.with_explain(Arc::clone(&explain));

    if let Some(ceiling) = belief_ceiling() {
        judge = judge.with_config(JudgeConfig::default().with_max_model_confidence(ceiling));
    }

    if let Some((_, live)) = &model {
        judge = judge.with_engine(live.current());
    }

    velora.add_stage(judge);
    velora.add_stage(BeliefStage::new(beliefs.clone()));
    // Kept for the pulse, which reads what is held to decide what to look at
    // again. Cloned here rather than reached for later, because by then the
    // stages own it.
    let beliefs_for_the_pulse = beliefs.clone();
    // Before the reasoner, so a procedure defined by one step is folded before
    // the thought that asked for it carries on — the difference between the next
    // step finding it and not.
    velora.add_stage(
        CapabilityStage::new(Arc::clone(&capabilities)).with_feedback(feedback.clone()),
    );
    velora.add_stage(PlanStage::new(Arc::clone(&plans)));
    pipeline.push("belief-judge".to_owned());
    pipeline.push("beliefs".to_owned());
    pipeline.push("capability".to_owned());
    pipeline.push("plans".to_owned());

    // Held for the closing report, which reads back what the system did.
    let thinking: Option<Engine> = model.as_ref().map(|(_, live)| live.current());

    // What the screen is told about the thing doing the thinking, read before
    // the model is handed to the stages that will consume it.
    let model_about = model.as_ref().map(|(_, live)| {
        let engine = live.current();
        let capabilities = engine.capabilities();
        (
            engine.provider_name().to_owned(),
            engine.model().to_owned(),
            capabilities.streaming,
            capabilities.tools,
        )
    });

    // What the system can do to this computer, taken before the catalog is
    // handed to the executor.
    let procedures: Vec<String> = catalog
        .procedures()
        .into_iter()
        .map(|procedure| procedure.name().to_owned())
        .collect();

    // The rest of the loop: what the system does with what it believes.
    if let Some((settings, live)) = model {
        velora.add_stage(StewardStage::new(settings.clone()).with_engine(live.clone()));

        velora.add_stage(
            ResolverStage::new(beliefs.clone(), velora.history())
                .with_explain(Arc::clone(&explain))
                // A rule, not a judgement: deciding whether to think should be
                // cheap and readable, and what to do is the expensive question.
                // What the operator says is an event like any other. Acting on it
                // is a decision made here, not a branch in the resolver: the
                // system is told that this kind of event is worth a thought, and
                // what to do about it.
                .with_event(EventWatch::kind(Said::KIND, velora_operator::task(&channel)))
                // And the judge's own decision that an event calls for action.
                // This is what makes attention a decision rather than a list:
                // a source the system invented for itself reports, the judge
                // says whether *this* report matters, and the decision is an
                // event this rule acts on. What the judge decided is in the
                // event, so the model reads what it is being woken for.
                .with_event(EventWatch::kind(
                    Wake::KIND,
                    "You decided this event called for action. Decide what to do about it, \
                     and do it.",
                ))
                // And a quiet moment, which nobody asked for. This is the one
                // rule that is not about something happening: the pulse says
                // nothing has, and the thought that follows is the system's own.
                .with_event(Pulse::watch())
                // And a step of a plan. This is what makes a plan work through
                // itself: the step is handed out as an event, the turn does it,
                // and finishing it hands out the next one.
                .with_event(EventWatch::kind(
                    StepReady::KIND,
                    "A step of a plan you are working through is ready. Do it, and then mark \
                     it done with plan.step.done. If the plan is wrong, revise it; if it \
                     cannot be done, give up on it and say why.",
                ))
                .with_watch(Watch::prefix(
                    "operator.",
                    "Something the operator cares about has changed ({key}). \
                     Decide whether anything needs doing, and do it.",
                ))
                // How many things the system may be working on at once. More
                // than one is the difference between being asked for two things
                // and doing them in turn, and being able to answer while
                // something long runs — which is what a person means by "do this
                // for me" while they carry on talking.
                .with_max_open_turns(parallel),
        );

        velora.add_stage(
            ReasonerStage::new(live, settings, catalog.clone())
                .with_feedback(feedback.clone()),
        );

        velora.add_stage(
            ExecutorStage::new(catalog)
                .with_feedback(feedback)
                // Matching the resolver, and for the same reason: with one
                // procedure at a time, a question waits behind an install.
                .with_config(ExecutorConfig::default().with_max_in_flight(parallel)),
        );

        pipeline.push("steward".to_owned());
        pipeline.push("resolver".to_owned());
        pipeline.push("reasoner".to_owned());
        pipeline.push("executor".to_owned());
    }

    // The screen. It reads the same history everyone else reads, describes
    // records with the same vocabulary the model is given, and submits what the
    // operator types as an ordinary event on the same ingress channel a source
    // would use — so nothing about watching the system is special to the system.
    //
    // Note what this changes: while the page is open it holds an ingress sender,
    // so the loop no longer stops when the other sources finish. That is the
    // point of a screen — the system stays up to be talked to — and it means a
    // run with the screen on ends by being asked to, not by running out of work.
    if let Some(addr) = ui_address() {
        let view = View {
            say: sender.clone(),
            history: velora.history(),
            beliefs: beliefs.clone(),
            explain: Arc::clone(&explain),
            about: about(
                velora.log().store().describe(),
                velora.log().store().durability(),
                model_about,
                pipeline,
                procedures,
                Some(protocol::DEFAULT_PERSONA.to_owned()),
            ),
        };

        tokio::spawn(async move {
            match velora_web::serve(addr, view).await {
                Ok(bound) => info!(url = %format!("http://{bound}"), "open this to watch and talk to it"),
                Err(err) => error!(address = %addr, error = %err, "the screen could not be served"),
            }
        });
    }

    // Files what the system said to the operator where they will find it. It
    // runs whether or not a model is configured: the channel is plumbing, and
    // the decision to use it belongs to the reasoning engine.
    let shutdown = Shutdown::new();

    // Ctrl-C is the runtime's business, not the loop's.
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                info!("interrupt received; stopping the loop");
                shutdown.trigger();
            }
        });
    }

    // The pulse: a source whose reason for producing an event is a clock. It
    // sends on the same channel as the screen and the observers, so nothing
    // downstream knows or cares that nothing happened — which is what keeps the
    // promise that a turn is its events.
    if let Some(pulse) = pulse() {
        let sender = sender.clone();
        let shutdown = shutdown.clone();
        let history = velora.history();
        let beliefs = beliefs_for_the_pulse.clone();

        tokio::spawn(async move {
            velora_pulse::serve(sender, shutdown, history, beliefs, pulse).await;
        });
    }

    // A placeholder source, and only when asked for. A real adapter (the
    // screen, the filesystem, a microphone) sends into this same channel and
    // changes nothing else — and none of them invent observations, which is
    // what this does.
    if let Some(limit) = demo_limit() {
        let sender = sender.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            let mut emitted = 0u64;
            loop {
                ticker.tick().await;

                let (kind, text) = DEMO_OBSERVATIONS[emitted as usize % DEMO_OBSERVATIONS.len()];
                emitted += 1;

                let event = Event::new(
                    "velora.demo",
                    kind,
                    PayloadVersion::new(1),
                    text.as_bytes().to_vec(),
                    Timestamp::now(),
                    Sensitivity::INTERNAL,
                );

                if sender.send(event).await.is_err() {
                    break;
                }

                if emitted >= limit {
                    info!(observations = emitted, "demo source finished; stopping the loop");
                    shutdown.trigger();
                    break;
                }
            }
        });
    }

    // The loop stops when it is asked to, or when every sender is gone.
    drop(sender);

    velora.run(shutdown).await?;

    report(&beliefs, thinking.as_ref(), &velora)?;

    let (records, store) = {
        let log = velora.log();
        (log.len(), log.store().describe())
    };
    info!(records, store = %store, "velora stopped");

    Ok(())
}
