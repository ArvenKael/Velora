//! Revision semantics: what the store holds, what it refuses, and what it never
//! throws away.

use std::sync::Arc;

use velora_belief::{
    Belief, BeliefStore, ClaimKey, ClaimValue, Confidence, Evidence, Outcome, Rejection,
    Reinforcement, Revision, RevisionPolicy,
};
use velora_event::{PayloadVersion, Sequence};

fn belief(
    key: &str,
    value: &[u8],
    confidence: u16,
    evidence: impl IntoIterator<Item = Sequence>,
    at: u64,
) -> Belief {
    Belief::new(
        ClaimKey::new(key),
        ClaimValue::new(PayloadVersion::new(1), value.to_vec()),
        Confidence::from_basis_points(confidence).unwrap(),
        Evidence::new(evidence).unwrap(),
        Sequence::new(at),
    )
}

fn key(name: &str) -> ClaimKey {
    ClaimKey::new(name)
}

#[test]
fn an_assertion_establishes_a_belief() {
    let mut store = BeliefStore::new();
    let outcome = store
        .assert(belief("user.theme", b"dark", 8_000, [Sequence::FIRST], 2))
        .unwrap();

    assert_eq!(outcome, Outcome::Established);
    let held = store.get(&key("user.theme")).unwrap();
    assert_eq!(held.value().bytes(), b"dark");
    assert_eq!(held.confidence().basis_points(), 8_000);
    assert_eq!(held.evidence().sequences(), &[Sequence::FIRST]);
    assert_eq!(held.established_at(), Sequence::new(2));
    assert_eq!(store.len(), 1);
    assert!(!store.is_contested(&key("user.theme")));
}

#[test]
fn agreeing_evidence_accumulates_instead_of_replacing() {
    let mut store = BeliefStore::new();
    store
        .assert(belief("k", b"v", 6_000, [Sequence::new(1)], 2))
        .unwrap();
    let outcome = store
        .assert(belief("k", b"v", 7_000, [Sequence::new(3)], 4))
        .unwrap();

    assert_eq!(outcome, Outcome::Reinforced);
    let held = store.get(&key("k")).unwrap();
    assert_eq!(held.confidence().basis_points(), 7_000, "kept the stronger");
    assert_eq!(
        held.evidence().sequences(),
        &[Sequence::new(1), Sequence::new(3)],
        "both events support the claim"
    );
    assert_eq!(held.established_at(), Sequence::new(2), "unchanged");
    assert_eq!(held.revised_at(), Sequence::new(4), "last touched here");
}

#[test]
fn a_stronger_disagreement_supersedes_but_keeps_the_loser() {
    let mut store = BeliefStore::new();
    store
        .assert(belief("k", b"yes", 9_000, [Sequence::new(1)], 2))
        .unwrap();
    let outcome = store
        .assert(belief("k", b"no", 9_500, [Sequence::new(3)], 4))
        .unwrap();

    assert_eq!(outcome, Outcome::Superseded);
    assert_eq!(store.get(&key("k")).unwrap().value().bytes(), b"no");

    assert!(store.is_contested(&key("k")));
    let challenges = store.challenges(&key("k"));
    assert_eq!(challenges.len(), 1);
    assert_eq!(
        challenges[0].value().bytes(),
        b"yes",
        "the claim that lost is still available to examine"
    );
}

#[test]
fn a_weaker_disagreement_is_retained_and_still_recorded() {
    let mut store = BeliefStore::new();
    store
        .assert(belief("k", b"yes", 9_000, [Sequence::new(1)], 2))
        .unwrap();
    let outcome = store
        .assert(belief("k", b"no", 1_000, [Sequence::new(3)], 4))
        .unwrap();

    assert_eq!(outcome, Outcome::Retained);
    assert_eq!(store.get(&key("k")).unwrap().value().bytes(), b"yes");
    assert_eq!(store.challenges(&key("k"))[0].value().bytes(), b"no");
}

#[test]
fn equal_confidence_is_broken_by_sequence() {
    let mut store = BeliefStore::new();
    store
        .assert(belief("k", b"first", 5_000, [Sequence::new(1)], 2))
        .unwrap();
    let outcome = store
        .assert(belief("k", b"second", 5_000, [Sequence::new(3)], 4))
        .unwrap();

    assert_eq!(outcome, Outcome::Superseded);
    assert_eq!(
        store.get(&key("k")).unwrap().value().bytes(),
        b"second",
        "the later event wins a tie, which is deterministic because events fold in order"
    );
}

#[test]
fn an_event_cannot_cite_itself_as_evidence() {
    let mut store = BeliefStore::new();
    let err = store
        .assert(belief("k", b"v", 5_000, [Sequence::new(7)], 7))
        .unwrap_err();

    assert_eq!(
        err,
        Rejection::EvidenceNotBefore {
            cited: Sequence::new(7),
            at: Sequence::new(7),
        }
    );
    assert!(store.is_empty(), "nothing is held from an unjustified claim");
}

#[test]
fn evidence_from_the_future_is_refused() {
    let mut store = BeliefStore::new();
    let err = store
        .assert(belief("k", b"v", 5_000, [Sequence::new(9)], 7))
        .unwrap_err();

    assert_eq!(
        err,
        Rejection::EvidenceNotBefore {
            cited: Sequence::new(9),
            at: Sequence::new(7),
        }
    );
    assert!(store.is_empty());
}

#[test]
fn sequence_zero_is_refused_as_evidence() {
    let mut store = BeliefStore::new();
    let err = store
        .assert(belief("k", b"v", 5_000, [Sequence::new(0)], 2))
        .unwrap_err();

    assert_eq!(err, Rejection::ReservedSequence(Sequence::new(0)));
}

#[test]
fn a_claim_without_a_key_is_refused() {
    let mut store = BeliefStore::new();
    let err = store
        .assert(belief("", b"v", 5_000, [Sequence::FIRST], 2))
        .unwrap_err();

    assert_eq!(err, Rejection::EmptyKey);
}

#[test]
fn evidence_cannot_be_empty() {
    assert!(Evidence::new([]).is_err());
    assert_eq!(Evidence::one(Sequence::new(4)).sequences(), &[Sequence::new(4)]);
}

#[test]
fn a_grounded_withdrawal_drops_the_belief_but_remembers_it() {
    let mut store = BeliefStore::new();
    store
        .assert(belief("k", b"v", 9_000, [Sequence::new(1)], 2))
        .unwrap();

    let outcome = store
        .retract(&key("k"), Confidence::CERTAIN, &Evidence::one(Sequence::new(3)), Sequence::new(4))
        .unwrap();

    assert_eq!(outcome, Outcome::Withdrawn);
    assert!(store.get(&key("k")).is_none());
    assert!(store.is_empty());

    let withdrawn = store.withdrawals(&key("k"));
    assert_eq!(withdrawn.len(), 1);
    assert_eq!(withdrawn[0].belief().value().bytes(), b"v");
    assert_eq!(withdrawn[0].evidence().sequences(), &[Sequence::new(3)]);
    assert_eq!(withdrawn[0].at(), Sequence::new(4));
}

#[test]
fn withdrawing_something_not_held_is_not_an_error() {
    let mut store = BeliefStore::new();
    let outcome = store
        .retract(
            &key("never"),
            Confidence::CERTAIN,
            &Evidence::one(Sequence::FIRST),
            Sequence::new(2),
        )
        .unwrap();

    assert_eq!(outcome, Outcome::AlreadyAbsent);
}

#[test]
fn a_withdrawal_needs_grounds_too() {
    let mut store = BeliefStore::new();
    store
        .assert(belief("k", b"v", 9_000, [Sequence::new(1)], 2))
        .unwrap();

    let err = store
        .retract(&key("k"), Confidence::CERTAIN, &Evidence::one(Sequence::new(9)), Sequence::new(4))
        .unwrap_err();

    assert!(matches!(err, Rejection::EvidenceNotBefore { .. }));
    assert!(
        store.get(&key("k")).is_some(),
        "a withdrawal that cannot be justified does not withdraw anything"
    );
}

/// A policy that refuses to ever revise, to show the rule really is swappable.
struct AlwaysRetain;

impl RevisionPolicy for AlwaysRetain {
    fn name(&self) -> &'static str {
        "always-retain"
    }

    fn reinforce(&self, _held: &Belief, _incoming: &Belief) -> Reinforcement {
        Reinforcement::Adopt
    }

    fn resolve(&self, _held: &Belief, _incoming: &Belief) -> Revision {
        Revision::Retain
    }
}

#[test]
fn the_revision_rule_comes_from_the_caller() {
    let mut store = BeliefStore::with_policy(Arc::new(AlwaysRetain));
    assert_eq!(store.policy_name(), "always-retain");

    store
        .assert(belief("k", b"first", 100, [Sequence::new(1)], 2))
        .unwrap();
    let outcome = store
        .assert(belief("k", b"second", 9_999, [Sequence::new(3)], 4))
        .unwrap();

    assert_eq!(
        outcome,
        Outcome::Retained,
        "the default would have superseded here"
    );
    assert_eq!(store.get(&key("k")).unwrap().value().bytes(), b"first");
}

#[test]
fn iteration_is_ordered_by_key_so_a_rebuild_is_identical() {
    let mut store = BeliefStore::new();
    for (index, name) in ["zeta", "alpha", "mu"].iter().enumerate() {
        store
            .assert(belief(
                name,
                b"v",
                5_000,
                [Sequence::new(index as u64 + 1)],
                index as u64 + 2,
            ))
            .unwrap();
    }

    let order: Vec<String> = store.iter().map(|held| held.key().to_string()).collect();
    assert_eq!(order, vec!["alpha", "mu", "zeta"]);
}

/// A withdrawal is a claim too, and a weak one cannot overturn a strong belief.
#[test]
fn a_weak_withdrawal_is_refused_and_the_belief_stands() {
    let mut store = BeliefStore::new();
    store
        .assert(belief("k", b"v", 9_000, [Sequence::new(1)], 2))
        .unwrap();

    let outcome = store
        .retract(
            &key("k"),
            Confidence::from_basis_points(1_000).unwrap(),
            &Evidence::one(Sequence::new(3)),
            Sequence::new(4),
        )
        .unwrap();

    assert_eq!(outcome, Outcome::WithdrawalRefused);
    assert!(
        store.get(&key("k")).is_some(),
        "a weakly held withdrawal must not drop a strongly held belief"
    );

    let attempts = store.withdrawals(&key("k"));
    assert_eq!(attempts.len(), 1, "the failed attempt is still recorded");
    assert!(!attempts[0].was_accepted());
    assert_eq!(attempts[0].confidence().basis_points(), 1_000);
}

#[test]
fn a_withdrawal_at_least_as_strong_as_the_belief_is_accepted() {
    let mut store = BeliefStore::new();
    store
        .assert(belief("k", b"v", 5_000, [Sequence::new(1)], 2))
        .unwrap();

    let outcome = store
        .retract(
            &key("k"),
            Confidence::from_basis_points(5_000).unwrap(),
            &Evidence::one(Sequence::new(3)),
            Sequence::new(4),
        )
        .unwrap();

    assert_eq!(outcome, Outcome::Withdrawn);
    assert!(store.get(&key("k")).is_none());
    assert!(store.withdrawals(&key("k"))[0].was_accepted());
}

/// A policy that never lets anything go.
struct NeverWithdraw;

impl RevisionPolicy for NeverWithdraw {
    fn name(&self) -> &'static str {
        "never-withdraw"
    }

    fn retract(
        &self,
        _held: &Belief,
        _confidence: Confidence,
        _evidence: &Evidence,
    ) -> velora_belief::Retraction {
        velora_belief::Retraction::Refuse
    }
}

#[test]
fn the_withdrawal_rule_comes_from_the_caller() {
    let mut store = BeliefStore::with_policy(Arc::new(NeverWithdraw));
    store
        .assert(belief("k", b"v", 1_000, [Sequence::new(1)], 2))
        .unwrap();

    let outcome = store
        .retract(
            &key("k"),
            Confidence::CERTAIN,
            &Evidence::one(Sequence::new(3)),
            Sequence::new(4),
        )
        .unwrap();

    assert_eq!(
        outcome,
        Outcome::WithdrawalRefused,
        "the default would have accepted this"
    );
    assert!(store.get(&key("k")).is_some());
}

/// The heuristic that makes the prompt cap drop the least relevant beliefs.
#[test]
fn beliefs_named_by_the_text_are_ranked_first() {
    let mut store = BeliefStore::new();
    // "alpha" is oldest, "zeta" newest, "theme" in the middle.
    store
        .assert(belief("alpha", b"1", 5_000, [Sequence::new(1)], 2))
        .unwrap();
    store
        .assert(belief("theme", b"dark", 5_000, [Sequence::new(3)], 4))
        .unwrap();
    store
        .assert(belief("zeta", b"3", 5_000, [Sequence::new(5)], 6))
        .unwrap();

    // By recency the order is zeta, theme, alpha.
    let by_recency: Vec<String> = store
        .iter()
        .map(|held| held.key().to_string())
        .collect();
    assert_eq!(by_recency, vec!["alpha", "theme", "zeta"]);

    let ranked = store.relevant_to("the user changed their theme again", 2);
    assert_eq!(
        ranked[0].key().as_str(),
        "theme",
        "the belief the text is about must survive the cap"
    );
    assert_eq!(ranked.len(), 2);
}
