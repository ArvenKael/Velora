use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use velora_event::{DecodeError, Payload, PayloadKind, PayloadVersion, Record, Sequence};

use crate::belief::{Belief, Outcome, Rejection};
use crate::claim::ClaimKey;
use crate::confidence::Confidence;
use crate::evidence::Evidence;
use crate::payload::{AssertBelief, RetractBelief};
use crate::policy::{ConfidenceFirst, Retraction, Reinforcement, Revision, RevisionPolicy};

/// A belief that was withdrawn, and the evidence that withdrew it.
///
/// A withdrawal that was refused is kept too: a withdrawal that failed is also
/// something that happened, and hiding it would make a contested belief look
/// uncontested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    belief: Belief,
    evidence: Evidence,
    confidence: Confidence,
    at: Sequence,
    accepted: bool,
}

impl Withdrawal {
    /// The belief as it stood when the withdrawal was attempted.
    pub fn belief(&self) -> &Belief {
        &self.belief
    }

    /// The events that were offered as grounds.
    pub fn evidence(&self) -> &Evidence {
        &self.evidence
    }

    /// How strongly it was withdrawn.
    pub fn confidence(&self) -> Confidence {
        self.confidence
    }

    /// The event that attempted it.
    pub fn at(&self) -> Sequence {
        self.at
    }

    /// Whether it took effect.
    pub fn was_accepted(&self) -> bool {
        self.accepted
    }
}

/// Failure while folding an event into the store.
#[derive(Debug)]
pub enum BeliefError {
    /// The payload bytes were malformed.
    Decode(DecodeError),
    /// The event was well formed but not acceptable as evidence.
    Rejected(Rejection),
}

impl fmt::Display for BeliefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BeliefError::Decode(err) => write!(f, "could not decode belief event: {err}"),
            BeliefError::Rejected(rejection) => write!(f, "belief rejected: {rejection}"),
        }
    }
}

impl std::error::Error for BeliefError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BeliefError::Decode(err) => Some(err),
            BeliefError::Rejected(rejection) => Some(rejection),
        }
    }
}

/// Counters describing what a store has folded.
///
/// Kept with the state, not with the stage, because the loop owns its stages:
/// a counter nobody can read is not an observation. Like everything else here,
/// these are recomputed identically by a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StoreStats {
    /// Beliefs currently held.
    pub held: usize,
    /// Belief events that changed something.
    pub accepted: u64,
    /// Belief events refused for being malformed or unjustified.
    pub refused: u64,
}

/// The beliefs currently held, as a projection of history.
///
/// This is derived state, never a source of truth. Every belief here was put
/// there by an event, and the whole store can be thrown away and rebuilt by
/// replaying those events — which is exactly what [`crate::BeliefStage`] does
/// on start-up.
///
/// Three things are tracked, so that nothing is ever silently dropped:
///
/// - **held** — the belief the policy says stands for a key.
/// - **challenged** — claims that disagreed and lost, kept visible.
/// - **withdrawn** — beliefs that were withdrawn, and on what evidence.
///
/// Iteration is ordered by key, not by hash, so a rebuilt store is identical to
/// the store it replaces and any future digest of projection state is stable.
#[derive(Clone)]
pub struct BeliefStore {
    policy: Arc<dyn RevisionPolicy>,
    held: BTreeMap<ClaimKey, Belief>,
    challenged: BTreeMap<ClaimKey, Vec<Belief>>,
    withdrawals: BTreeMap<ClaimKey, Vec<Withdrawal>>,
    accepted: u64,
    refused: u64,
}

impl fmt::Debug for BeliefStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BeliefStore")
            .field("policy", &self.policy.name())
            .field("held", &self.held.len())
            .field("challenged", &self.challenged.len())
            .field("withdrawals", &self.withdrawals.len())
            .field("accepted", &self.accepted)
            .field("refused", &self.refused)
            .finish()
    }
}

impl Default for BeliefStore {
    fn default() -> Self {
        BeliefStore::new()
    }
}

impl BeliefStore {
    /// Creates an empty store using the default revision policy.
    pub fn new() -> Self {
        BeliefStore::with_policy(Arc::new(ConfidenceFirst))
    }

    /// Creates an empty store with a caller-supplied policy.
    pub fn with_policy(policy: Arc<dyn RevisionPolicy>) -> Self {
        BeliefStore {
            policy,
            held: BTreeMap::new(),
            challenged: BTreeMap::new(),
            withdrawals: BTreeMap::new(),
            accepted: 0,
            refused: 0,
        }
    }

    /// What this store has taken in, refused, and currently holds.
    pub fn stats(&self) -> StoreStats {
        StoreStats {
            held: self.held.len(),
            accepted: self.accepted,
            refused: self.refused,
        }
    }

    /// The name of the revision policy in force.
    pub fn policy_name(&self) -> &'static str {
        self.policy.name()
    }

    /// The belief currently held about `key`, if any.
    pub fn get(&self, key: &ClaimKey) -> Option<&Belief> {
        self.held.get(key)
    }

    /// Every held belief, ordered by key.
    pub fn iter(&self) -> impl Iterator<Item = &Belief> {
        self.held.values()
    }

    /// How many beliefs are held.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// Whether no beliefs are held.
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Claims that disagreed and did not win.
    pub fn challenges(&self, key: &ClaimKey) -> &[Belief] {
        self.challenged.get(key).map_or(&[], Vec::as_slice)
    }

    /// The beliefs most likely to be affected by some text, capped.
    ///
    /// Taking the first N by key would be arbitrary: the belief a note
    /// contradicts is usually the one the note is about. So beliefs whose key
    /// appears in the text come first, and the rest follow by recency, because
    /// what was last revised is more likely to be in play than what has sat
    /// untouched.
    ///
    /// This is a heuristic and a blunt one — a belief about the same subject
    /// under a different name is not ranked up, and the scan is linear in the
    /// number of beliefs. It exists so that the cap drops the least relevant
    /// beliefs rather than the last ones alphabetically.
    pub fn relevant_to(&self, text: &str, limit: usize) -> Vec<Belief> {
        let text = text.to_lowercase();

        let mut scored: Vec<(&Belief, bool)> = self
            .held
            .values()
            .map(|belief| {
                let named = text.contains(&belief.key().as_str().to_lowercase());
                (belief, named)
            })
            .collect();

        scored.sort_by(|left, right| {
            right
                .1
                .cmp(&left.1)
                .then_with(|| right.0.revised_at().cmp(&left.0.revised_at()))
                .then_with(|| left.0.key().cmp(right.0.key()))
        });

        scored
            .into_iter()
            .take(limit)
            .map(|(belief, _)| belief.clone())
            .collect()
    }

    /// Whether anything currently disagrees with the held belief.
    ///
    /// A contested belief is still held — the policy decided — but the
    /// disagreement exists and can be seen rather than guessed at.
    pub fn is_contested(&self, key: &ClaimKey) -> bool {
        self.challenged.get(key).is_some_and(|claims| !claims.is_empty())
    }

    /// Every withdrawal attempted for `key`, accepted or refused.
    ///
    /// Refused attempts are included: a withdrawal that failed is also
    /// something that happened, and a belief that survived one is not
    /// uncontested.
    pub fn withdrawals(&self, key: &ClaimKey) -> &[Withdrawal] {
        self.withdrawals.get(key).map_or(&[], Vec::as_slice)
    }

    /// Takes in one assertion.
    ///
    /// # Errors
    ///
    /// Fails if the claim has no key, or if its evidence is not strictly
    /// earlier than the event asserting it. A claim that cannot be justified
    /// from what already happened is not held.
    pub fn assert(&mut self, belief: Belief) -> Result<Outcome, Rejection> {
        if belief.key().is_empty() {
            return Err(Rejection::EmptyKey);
        }
        validate_evidence(belief.evidence(), belief.revised_at())?;

        let key = belief.key().clone();
        let Some(held) = self.held.get(&key).cloned() else {
            self.held.insert(key, belief);
            return Ok(Outcome::Established);
        };

        if held.value() == belief.value() {
            let updated = match self.policy.reinforce(&held, &belief) {
                Reinforcement::MergeEvidence => held.reinforced(
                    belief.confidence(),
                    belief.evidence().clone(),
                    belief.rationale(),
                    belief.revised_at(),
                ),
                Reinforcement::Adopt => belief,
            };
            self.held.insert(key, updated);
            return Ok(Outcome::Reinforced);
        }

        match self.policy.resolve(&held, &belief) {
            Revision::Supersede => {
                // The claim that lost is kept, not overwritten: a disagreement
                // that leaves no trace is a disagreement nobody can examine.
                self.challenged.entry(key.clone()).or_default().push(held);
                self.held.insert(key, belief);
                Ok(Outcome::Superseded)
            }
            Revision::Retain => {
                self.challenged.entry(key).or_default().push(belief);
                Ok(Outcome::Retained)
            }
        }
    }

    /// Withdraws a claim.
    ///
    /// A withdrawal is a claim like any other, so it goes through
    /// [`RevisionPolicy::retract`] rather than taking effect on its own: a weak
    /// withdrawal cannot quietly overturn a strongly held belief. Either way the
    /// attempt is recorded, so a refused withdrawal is visible instead of
    /// vanishing.
    ///
    /// # Errors
    ///
    /// Fails on the same grounds as [`BeliefStore::assert`]: a withdrawal is
    /// grounded in events just as an assertion is.
    pub fn retract(
        &mut self,
        key: &ClaimKey,
        confidence: Confidence,
        evidence: &Evidence,
        at: Sequence,
    ) -> Result<Outcome, Rejection> {
        if key.is_empty() {
            return Err(Rejection::EmptyKey);
        }
        validate_evidence(evidence, at)?;

        let Some(held) = self.held.get(key).cloned() else {
            return Ok(Outcome::AlreadyAbsent);
        };

        let accepted = self.policy.retract(&held, confidence, evidence) == Retraction::Accept;
        if accepted {
            self.held.remove(key);
        }

        self.withdrawals
            .entry(key.clone())
            .or_default()
            .push(Withdrawal {
                belief: held,
                evidence: evidence.clone(),
                confidence,
                at,
                accepted,
            });

        Ok(if accepted {
            Outcome::Withdrawn
        } else {
            Outcome::WithdrawalRefused
        })
    }

    /// Folds one stored record into the store.
    ///
    /// Returns `Ok(None)` for a record that is not a belief event, which is the
    /// common case: a history holds everything that happened, and only some of
    /// it is about belief.
    ///
    /// # Errors
    ///
    /// Fails if a belief event is malformed or unjustified. The caller decides
    /// whether that stops anything; the belief stage treats it as a refusal
    /// rather than a fault.
    pub fn apply(&mut self, record: &Record) -> Result<Option<Outcome>, BeliefError> {
        self.apply_payload(
            record.kind(),
            record.version(),
            record.payload(),
            record.sequence(),
        )
    }

    /// Folds one event's payload into the store.
    ///
    /// Takes bytes rather than a record so that both a rebuilt projection (over
    /// records) and a live one (over loop events) go through exactly the same
    /// path — a projection that folds differently depending on where the event
    /// came from is not a projection.
    pub fn apply_payload(
        &mut self,
        kind: &PayloadKind,
        version: PayloadVersion,
        payload: &[u8],
        at: Sequence,
    ) -> Result<Option<Outcome>, BeliefError> {
        let outcome = if kind.as_str() == AssertBelief::KIND {
            match AssertBelief::decode(version, payload) {
                Ok(assertion) => self
                    .assert(assertion.into_belief(at))
                    .map(Some)
                    .map_err(BeliefError::Rejected),
                Err(err) => Err(BeliefError::Decode(err)),
            }
        } else if kind.as_str() == RetractBelief::KIND {
            match RetractBelief::decode(version, payload) {
                Ok(retraction) => self
                    .retract(
                        retraction.key(),
                        retraction.confidence(),
                        retraction.evidence(),
                        at,
                    )
                    .map(Some)
                    .map_err(BeliefError::Rejected),
                Err(err) => Err(BeliefError::Decode(err)),
            }
        } else {
            return Ok(None);
        };

        match &outcome {
            Ok(Some(_)) => self.accepted += 1,
            Err(_) => self.refused += 1,
            Ok(None) => {}
        }

        outcome
    }
}

/// Refuses evidence that does not already exist at the moment of citing.
///
/// Because history is contiguous, "strictly before this event" is the same as
/// "already in the log", so this single check is what keeps a belief from
/// resting on its own conclusion or on an event that never happened.
fn validate_evidence(evidence: &Evidence, at: Sequence) -> Result<(), Rejection> {
    for cited in evidence.sequences() {
        if *cited < Sequence::FIRST {
            return Err(Rejection::ReservedSequence(*cited));
        }
        if *cited >= at {
            return Err(Rejection::EvidenceNotBefore { cited: *cited, at });
        }
    }
    Ok(())
}
