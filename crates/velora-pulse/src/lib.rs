//! A source that fires when nothing has happened.
//!
//! Everything else in Velora happens *because* something happened: a person
//! spoke, a source reported, a step came back. That is the right shape for a
//! system that acts on events, and it leaves one thing out — a person does not
//! only think when spoken to. They notice that the house has gone quiet, that
//! they still have not finished something, that they have believed a thing for a
//! while and never looked at it again.
//!
//! This is that, and it is deliberately an ordinary source. It sits on the ingress
//! channel beside the screen and the observers, it produces events, and nothing
//! downstream knows or cares that its reason for producing them was a clock. That
//! is what keeps the invariant intact: a turn is still its events, and every
//! thought the system has on its own traces back to a record that says why.
//!
//! Two events, on a quiet moment:
//!
//! - [`Idle`], which the resolver turns into a thought: *nothing has happened for
//!   a while — is there anything that needs doing?*
//! - a [`RECONSIDER_KIND`](velora_mind::RECONSIDER_KIND) event naming one belief,
//!   which the judge already knows how to ask about: *you have held this for a
//!   while; does it still hold?*
//!
//! The second is the "memory" half and needed no new machinery at all — the
//! question and its answer were built and tested long before anything asked it.

use std::time::{Duration, Instant};

use velora_agent::EventWatch;
use velora_belief::{BeliefHandle, BeliefStore, ClaimKey};
use velora_event::codec::{Reader, Writer};
use velora_event::{DecodeError, Payload, PayloadVersion, Sensitivity, Store, Timestamp};
use velora_log::{debug, info};
use velora_loop::{Event, EventSender, History, Shutdown};
use velora_mind::RECONSIDER_KIND;

/// The name events and logs from here are attributed to.
pub const NAME: &str = "pulse";

/// The kind the resolver watches for a quiet moment.
pub const IDLE_KIND: &str = "idle.noted";

/// How the pulse behaves.
#[derive(Debug, Clone)]
pub struct Pulse {
    /// How long the log has to be unchanged before a moment counts as quiet.
    quiet: Duration,
    /// The least time between two of them.
    every: Duration,
    /// How often the log is looked at.
    poll: Duration,
}

impl Default for Pulse {
    fn default() -> Self {
        Pulse {
            quiet: Duration::from_secs(10 * 60),
            every: Duration::from_secs(30 * 60),
            poll: Duration::from_secs(15),
        }
    }
}

impl Pulse {
    /// Sets how long the log has to be unchanged.
    pub fn with_quiet(mut self, quiet: Duration) -> Self {
        self.quiet = quiet;
        self
    }

    /// Sets the least time between two quiet moments.
    pub fn with_every(mut self, every: Duration) -> Self {
        self.every = every;
        self
    }

    /// Sets how often the log is looked at.
    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll.max(Duration::from_millis(10));
        self
    }

    /// How long the log has to be unchanged.
    pub fn quiet(&self) -> Duration {
        self.quiet
    }

    /// The least time between two quiet moments.
    pub fn every(&self) -> Duration {
        self.every
    }

    /// Whether a quiet moment has arrived.
    ///
    /// Pure, so that the only interesting decision here can be tested without a
    /// clock or a loop.
    pub fn due(&self, unchanged_for: Duration, since_last: Option<Duration>) -> bool {
        if unchanged_for < self.quiet {
            return false;
        }

        since_last.is_none_or(|since| since >= self.every)
    }

    /// The watch that turns a quiet moment into a thought.
    ///
    /// The task is written to make doing nothing the ordinary answer, because it
    /// is: most of the time nothing needs doing, and a system that finds
    /// something to do every time it goes quiet has not become alive, it has
    /// become restless.
    pub fn watch() -> EventWatch {
        EventWatch::kind(
            IDLE_KIND,
            "Nothing has happened for a while, and this thought is your own — nobody \
             is waiting for it. Look at what you hold and at what you are in the \
             middle of. If something needs doing, do it. If something you hold no \
             longer looks right, say so. If nothing does, do nothing: ending this \
             without acting is the usual and correct outcome.",
        )
        .only_when_idle()
    }
}

/// What was noticed: that nothing has happened.
///
/// # Encoding (version 1)
///
/// ```text
/// quiet_for  u64, seconds the log had been unchanged
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Idle {
    quiet_for: u64,
}

impl Idle {
    /// A quiet moment, and how long it had been quiet for.
    pub const fn new(quiet_for: u64) -> Self {
        Idle { quiet_for }
    }

    /// How long the log had been unchanged, in seconds.
    pub const fn quiet_for(self) -> u64 {
        self.quiet_for
    }
}

impl Payload for Idle {
    const KIND: &'static str = IDLE_KIND;
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.quiet_for);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let quiet_for = reader.u64()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("idle.noted has trailing bytes"));
                }
                Ok(Idle { quiet_for })
            }
            other => Err(DecodeError::new(format!(
                "unsupported idle.noted version {other}"
            ))),
        }
    }
}

/// Which belief to look at again, one at a time.
///
/// Round-robin in key order, so that everything held is eventually looked at
/// instead of the same interesting belief being re-examined forever. Not
/// persisted: a restart starts the walk again, which is a fairer failure than
/// never reaching the end of it.
#[derive(Debug, Default)]
pub struct Rotation {
    last: Option<String>,
}

impl Rotation {
    /// The next belief in the walk, if anything is held.
    pub fn next(&mut self, beliefs: &BeliefStore) -> Option<ClaimKey> {
        let mut keys: Vec<ClaimKey> = beliefs.iter().map(|belief| belief.key().clone()).collect();
        keys.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        if keys.is_empty() {
            return None;
        }

        let at = match &self.last {
            Some(last) => keys
                .iter()
                .position(|key| key.as_str() > last.as_str())
                .unwrap_or(0),
            None => 0,
        };

        self.last = Some(keys[at].as_str().to_owned());
        Some(keys[at].clone())
    }
}

/// Runs until the loop stops, producing a quiet moment when there is one.
///
/// Holds the ingress sender, so a system with a pulse does not stop when its
/// other sources do — which is the point of it: something has to be there to
/// notice that nothing is.
pub async fn serve<S: Store + Send + Sync + 'static>(
    sender: EventSender,
    shutdown: Shutdown,
    history: History<S>,
    beliefs: BeliefHandle,
    pulse: Pulse,
) {
    let mut ticker = tokio::time::interval(pulse.poll);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut seen = history.len();
    let mut unchanged_since = Instant::now();
    let mut last: Option<Instant> = None;
    let mut rotation = Rotation::default();

    info!(
        quiet_secs = pulse.quiet.as_secs(),
        every_secs = pulse.every.as_secs(),
        "the pulse is running: the system will think on its own when nothing is happening"
    );

    loop {
        ticker.tick().await;

        if shutdown.is_triggered() {
            break;
        }

        let now = history.len();
        if now != seen {
            seen = now;
            unchanged_since = Instant::now();
            continue;
        }

        if !pulse.due(unchanged_since.elapsed(), last.map(|at| at.elapsed())) {
            continue;
        }

        let quiet_for = unchanged_since.elapsed().as_secs();
        let moment = Event::typed(
            NAME,
            &Idle::new(quiet_for),
            Timestamp::now(),
            Sensitivity::INTERNAL,
        );

        if sender.send(moment).await.is_err() {
            break;
        }

        // And one thing that is held, to look at again. This is the half that
        // needed nothing new: the judge already knows how to ask whether a belief
        // still holds.
        if let Some(key) = rotation.next(&beliefs.snapshot()) {
            let asked = Event::new(
                NAME,
                RECONSIDER_KIND,
                PayloadVersion::new(1),
                key.as_str().as_bytes().to_vec(),
                Timestamp::now(),
                Sensitivity::INTERNAL,
            );

            if sender.send(asked).await.is_err() {
                break;
            }

            debug!(key = %key, "asking whether a belief still holds");
        }

        last = Some(Instant::now());
        unchanged_since = Instant::now();
        seen = history.len();
    }

    debug!("the pulse stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pulse() -> Pulse {
        Pulse::default()
            .with_quiet(Duration::from_secs(60))
            .with_every(Duration::from_secs(300))
    }

    #[test]
    fn a_quiet_moment_needs_quiet() {
        let pulse = pulse();

        assert!(!pulse.due(Duration::from_secs(59), None));
        assert!(pulse.due(Duration::from_secs(60), None));
    }

    #[test]
    fn and_it_needs_the_last_one_to_be_a_while_ago() {
        // Otherwise a system with nothing to do would think about it every
        // fifteen seconds, forever, at a model call each.
        let pulse = pulse();

        assert!(!pulse.due(Duration::from_secs(600), Some(Duration::from_secs(299))));
        assert!(pulse.due(Duration::from_secs(600), Some(Duration::from_secs(300))));
    }

    #[test]
    fn a_moment_survives_history() {
        let idle = Idle::new(600);

        assert_eq!(idle.encode(), 600u64.to_be_bytes());
        let read = Idle::decode(PayloadVersion::new(1), &idle.encode()).expect("decoded");
        assert_eq!(read, idle);

        for cut in 0..idle.encode().len() {
            assert!(Idle::decode(PayloadVersion::new(1), &idle.encode()[..cut]).is_err());
        }
    }

    #[test]
    fn the_watch_waits_for_a_quiet_moment() {
        let watch = Pulse::watch();

        assert!(watch.matches(IDLE_KIND));
        assert!(watch.wants_idle(), "it must not open a turn on top of a turn");
        assert!(
            watch.task().contains("do nothing"),
            "the ordinary answer has to be the one the task names: {}",
            watch.task()
        );
    }

    /// A store holding the given keys, for walking.
    fn holding(keys: &[&str]) -> BeliefStore {
        use velora_belief::{AssertBelief, ClaimValue, Confidence, Evidence};
        use velora_event::Sequence;

        let mut store = BeliefStore::new();
        for (at, key) in keys.iter().enumerate() {
            let assertion = AssertBelief::new(
                ClaimKey::new(*key),
                ClaimValue::new(PayloadVersion::new(1), b"x".to_vec()),
                Confidence::from_basis_points(8_000).unwrap(),
                Evidence::new([Sequence::new(1)]).unwrap(),
            );

            store
                .assert(assertion.into_belief(Sequence::new(at as u64 + 2)))
                .unwrap();
        }

        store
    }

    #[test]
    fn every_belief_is_looked_at_before_any_is_looked_at_twice() {
        let store = holding(&["operator.a", "operator.c", "operator.b"]);

        let mut rotation = Rotation::default();
        let walked: Vec<String> = (0..3)
            .filter_map(|_| rotation.next(&store))
            .map(|key| key.as_str().to_owned())
            .collect();

        assert_eq!(walked, vec!["operator.a", "operator.b", "operator.c"]);
        assert_eq!(
            rotation.next(&store).map(|key| key.as_str().to_owned()),
            Some("operator.a".to_owned()),
            "and then it starts again"
        );
    }

    #[test]
    fn nothing_held_is_nothing_to_look_at() {
        let empty = BeliefStore::new();

        assert_eq!(Rotation::default().next(&empty), None);
        assert!(Rotation::default().next(&holding(&["operator.a"])).is_some());
    }
}
