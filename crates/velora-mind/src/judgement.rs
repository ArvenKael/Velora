use velora_belief::{AssertBelief, ClaimKey, ClaimValue, Confidence, Evidence, RetractBelief};
use velora_event::{PayloadVersion, Sensitivity, Timestamp};
use velora_loop::Event;

/// A change the system has decided its beliefs need.
///
/// This is the currency of deciding: a prespecified rule and a model both
/// produce these, and both are then recorded as belief events. A judgement is
/// *not* a belief — it is a proposal, and whether it ends up held is the belief
/// store's business.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgement {
    /// Hold that a key has a value.
    Assert {
        /// What is claimed.
        key: ClaimKey,
        /// What is claimed to be so.
        value: ClaimValue,
        /// How strongly.
        confidence: Confidence,
        /// Why, in the decider's words.
        rationale: String,
    },
    /// Stop holding a key.
    Retract {
        /// What is no longer claimed.
        key: ClaimKey,
        /// How strongly it is withdrawn. A weak withdrawal cannot overturn a
        /// strongly held belief, so this is not a formality.
        confidence: Confidence,
        /// Why.
        rationale: String,
    },
}

impl Judgement {
    /// A claim with an already-encoded value.
    pub fn assert(key: impl Into<ClaimKey>, value: ClaimValue, confidence: Confidence) -> Self {
        Judgement::Assert {
            key: key.into(),
            value,
            confidence,
            rationale: String::new(),
        }
    }

    /// A claim whose value is text.
    ///
    /// This is what a model can produce: it writes prose, not encodings. A
    /// domain that needs structured values supplies them through a rule, which
    /// is exactly the division of labour rules exist for.
    pub fn assert_text(
        key: impl Into<ClaimKey>,
        text: impl Into<String>,
        confidence: Confidence,
    ) -> Self {
        Judgement::assert(
            key,
            ClaimValue::new(PayloadVersion::new(1), text.into().into_bytes()),
            confidence,
        )
    }

    /// A withdrawal, held with certainty.
    ///
    /// A rule withdrawing something is decisive, so this is the right default;
    /// a model's withdrawal is given its own confidence instead.
    pub fn retract(key: impl Into<ClaimKey>) -> Self {
        Judgement::Retract {
            key: key.into(),
            confidence: Confidence::CERTAIN,
            rationale: String::new(),
        }
    }

    /// Sets how strongly the judgement is made.
    pub fn with_confidence(mut self, confidence: Confidence) -> Self {
        match &mut self {
            Judgement::Assert {
                confidence: held, ..
            }
            | Judgement::Retract {
                confidence: held, ..
            } => *held = confidence,
        }
        self
    }

    /// How strongly it is made.
    pub fn confidence(&self) -> Confidence {
        match self {
            Judgement::Assert { confidence, .. } | Judgement::Retract { confidence, .. } => {
                *confidence
            }
        }
    }

    /// Adds the reasoning behind the judgement.
    pub fn with_rationale(mut self, rationale: impl Into<String>) -> Self {
        match &mut self {
            Judgement::Assert { rationale: r, .. } | Judgement::Retract { rationale: r, .. } => {
                *r = rationale.into();
            }
        }
        self
    }

    /// What this judgement is about.
    pub fn key(&self) -> &ClaimKey {
        match self {
            Judgement::Assert { key, .. } | Judgement::Retract { key, .. } => key,
        }
    }

    /// Why, in the decider's words.
    pub fn rationale(&self) -> &str {
        match self {
            Judgement::Assert { rationale, .. } | Judgement::Retract { rationale, .. } => rationale,
        }
    }

    /// Records this judgement as an event, grounded in `evidence`.
    ///
    /// The evidence is what makes the belief answerable later: the recorded
    /// assertion cites the event that prompted it, so a belief can always be
    /// traced back through the reasoning to what actually happened.
    pub fn into_event(
        self,
        source: &'static str,
        time: Timestamp,
        sensitivity: Sensitivity,
        evidence: Evidence,
    ) -> Event {
        match self {
            Judgement::Assert {
                key,
                value,
                confidence,
                rationale,
            } => {
                let assertion = AssertBelief::new(key, value, confidence, evidence)
                    .with_rationale(rationale);
                Event::typed(source, &assertion, time, sensitivity)
            }
            Judgement::Retract {
                key,
                confidence,
                rationale,
            } => {
                let retraction =
                    RetractBelief::new(key, rationale, evidence).with_confidence(confidence);
                Event::typed(source, &retraction, time, sensitivity)
            }
        }
    }
}
