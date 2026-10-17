use std::fmt;

use velora_event::Sequence;

use crate::claim::{ClaimKey, ClaimValue};
use crate::confidence::Confidence;
use crate::evidence::Evidence;

/// A claim the system holds, together with the evidence for it.
///
/// A belief is never bare: it always carries the events it was drawn from and
/// the confidence attached to it, and it always knows which event in history
/// put it in its current shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Belief {
    key: ClaimKey,
    value: ClaimValue,
    confidence: Confidence,
    evidence: Evidence,
    rationale: String,
    established_at: Sequence,
    revised_at: Sequence,
}

impl Belief {
    /// Builds a belief established and last revised by the event at `at`.
    pub fn new(
        key: ClaimKey,
        value: ClaimValue,
        confidence: Confidence,
        evidence: Evidence,
        at: Sequence,
    ) -> Self {
        Belief {
            key,
            value,
            confidence,
            evidence,
            rationale: String::new(),
            established_at: at,
            revised_at: at,
        }
    }

    /// Adds the reasoning behind the belief.
    pub fn with_rationale(mut self, rationale: impl Into<String>) -> Self {
        self.rationale = rationale.into();
        self
    }

    /// Why, in the words of whoever decided. Empty when nothing was recorded,
    /// which is the case for everything asserted before version 2.
    pub fn rationale(&self) -> &str {
        &self.rationale
    }

    /// What this belief is about.
    pub fn key(&self) -> &ClaimKey {
        &self.key
    }

    /// What is claimed to be so.
    pub fn value(&self) -> &ClaimValue {
        &self.value
    }

    /// How strongly it is held.
    pub fn confidence(&self) -> Confidence {
        self.confidence
    }

    /// The events that justify it.
    pub fn evidence(&self) -> &Evidence {
        &self.evidence
    }

    /// The event that first established this claim.
    pub fn established_at(&self) -> Sequence {
        self.established_at
    }

    /// The event that last changed it — a reinforcement, not a fresh claim.
    pub fn revised_at(&self) -> Sequence {
        self.revised_at
    }

    /// The same belief, last revised by a later event.
    pub fn revised(mut self, at: Sequence) -> Self {
        self.revised_at = at;
        self
    }

    /// The same belief with additional support merged in.
    ///
    /// The value has not changed, so the belief stays established where it was;
    /// a newer rationale replaces an older one, because the most recent
    /// reasoning for holding the claim is the one worth reading.
    pub fn reinforced(
        mut self,
        confidence: Confidence,
        evidence: Evidence,
        rationale: &str,
        at: Sequence,
    ) -> Self {
        self.confidence = self.confidence.max(confidence);
        self.evidence = self.evidence.merge(&evidence);
        if !rationale.is_empty() {
            self.rationale = rationale.to_owned();
        }
        self.revised_at = at;
        self
    }
}

/// Why an event was not accepted into the store.
///
/// Rejection is not failure: refusing an unjustified claim is the store working
/// correctly. Every variant is a reason to hold nothing rather than to hold
/// something unsupported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The claim has no identity, so it could never be found or revised again.
    EmptyKey,
    /// An assertion may not cite itself or anything after itself: evidence is
    /// what is already known, and a belief cannot rest on its own conclusion.
    EvidenceNotBefore {
        /// The sequence the event cited.
        cited: Sequence,
        /// The sequence of the event doing the citing.
        at: Sequence,
    },
    /// Sequence zero is reserved to mean "before the beginning" and names no
    /// event.
    ReservedSequence(Sequence),
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejection::EmptyKey => f.write_str("the claim has no key"),
            Rejection::EvidenceNotBefore { cited, at } => write!(
                f,
                "event {at} cites event {cited} as evidence, which is not before it"
            ),
            Rejection::ReservedSequence(sequence) => {
                write!(f, "sequence {sequence} names no event")
            }
        }
    }
}

impl std::error::Error for Rejection {}

/// What accepting a belief event did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A claim not previously held is now held.
    Established,
    /// An agreeing claim added its evidence to one already held.
    Reinforced,
    /// A disagreeing claim won, and the previous belief became a challenge.
    Superseded,
    /// A disagreeing claim lost, and was kept as a challenge.
    Retained,
    /// A held belief was withdrawn.
    Withdrawn,
    /// A withdrawal was attempted and refused, and the belief stands.
    WithdrawalRefused,
    /// A withdrawal named a claim that was not held.
    AlreadyAbsent,
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Outcome::Established => "established",
            Outcome::Reinforced => "reinforced",
            Outcome::Superseded => "superseded",
            Outcome::Retained => "retained",
            Outcome::Withdrawn => "withdrawn",
            Outcome::WithdrawalRefused => "withdrawal refused",
            Outcome::AlreadyAbsent => "already absent",
        };
        f.write_str(text)
    }
}
