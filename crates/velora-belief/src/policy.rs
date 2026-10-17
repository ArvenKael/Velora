use crate::belief::Belief;
use crate::confidence::Confidence;
use crate::evidence::Evidence;

/// What to do when an incoming belief agrees with one already held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reinforcement {
    /// Keep the held confidence, add the incoming evidence.
    MergeEvidence,
    /// Replace the held belief outright.
    Adopt,
}

/// What to do when an incoming belief disagrees with one already held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revision {
    /// The incoming belief stands; the held one becomes a challenge.
    Supersede,
    /// The held belief stands; the incoming one becomes a challenge.
    Retain,
}

/// What to do about a withdrawal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retraction {
    /// The withdrawal stands and the belief is dropped.
    Accept,
    /// The belief stands. The withdrawal is still recorded, because a
    /// withdrawal that failed is also something that happened.
    Refuse,
}

/// The rule for revising a belief.
///
/// This is policy, and policy belongs to the caller: the store records what
/// happened and surfaces disagreements, and this decides whose claim stands.
/// Nothing here is about what a belief means — only about which of two
/// competing claims the system acts on.
///
/// Both methods have defaults, so a policy can replace one decision without
/// restating the other.
pub trait RevisionPolicy: Send + Sync {
    /// Stable name, used in logs.
    fn name(&self) -> &'static str;

    /// How to take in an agreeing claim.
    ///
    /// The default keeps the higher confidence and merges the evidence, so
    /// support accumulates rather than replacing itself.
    fn reinforce(&self, held: &Belief, incoming: &Belief) -> Reinforcement {
        let _ = (held, incoming);
        Reinforcement::MergeEvidence
    }

    /// How to settle a disagreement.
    ///
    /// The default is confidence-first: the more confident claim wins, and a
    /// tie goes to the later event, which is deterministic because events are
    /// folded in sequence order.
    fn resolve(&self, held: &Belief, incoming: &Belief) -> Revision {
        if incoming.confidence() >= held.confidence() {
            Revision::Supersede
        } else {
            Revision::Retain
        }
    }

    /// How to settle a withdrawal.
    ///
    /// A withdrawal is a claim like any other — "this is no longer so" — and it
    /// carries a confidence of its own. The default applies the same rule as for
    /// disagreements, so withdrawing a strongly held belief takes a strongly held
    /// withdrawal. Under the default, a model that is capped below a rule's
    /// confidence cannot unilaterally overturn what a rule established.
    fn retract(&self, held: &Belief, confidence: Confidence, evidence: &Evidence) -> Retraction {
        let _ = evidence;
        if confidence >= held.confidence() {
            Retraction::Accept
        } else {
            Retraction::Refuse
        }
    }
}

/// The default policy: confidence first, later event breaking a tie.
///
/// Conflicts are never discarded under this policy — the losing claim is kept
/// as a challenge, so a disagreement stays visible instead of being silently
/// overwritten.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConfidenceFirst;

impl RevisionPolicy for ConfidenceFirst {
    fn name(&self) -> &'static str {
        "confidence-first"
    }
}
