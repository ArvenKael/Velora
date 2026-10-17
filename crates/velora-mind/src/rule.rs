use std::fmt;

use velora_belief::Confidence;
use velora_loop::Event;

use crate::judgement::Judgement;

/// A prespecified meaning: an event of this kind always changes belief this way.
///
/// A rule is for the things that do not need thinking. A setting that arrives as
/// `key=value` means what it says; asking a model about it would spend a request
/// learning nothing, and would make a deterministic fact depend on a
/// non-deterministic process. Rules are how the system stays predictable where
/// predictability is possible.
pub trait BeliefRule: Send + Sync {
    /// The event kind this rule handles, and handles alone.
    fn kind(&self) -> &str;

    /// What this event means. Nothing means it changes nothing.
    fn judge(&self, event: &Event) -> Vec<Judgement>;
}

/// The rules a judge applies without asking anyone.
///
/// A kind is handled by at most one rule: the first registered rule that claims
/// a kind wins, so a caller can override a default by registering its own first.
#[derive(Default)]
pub struct RuleSet {
    rules: Vec<Box<dyn BeliefRule>>,
}

impl fmt::Debug for RuleSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuleSet")
            .field("kinds", &self.kinds())
            .finish()
    }
}

impl RuleSet {
    /// An empty rule set.
    pub fn new() -> Self {
        RuleSet::default()
    }

    /// Adds a rule.
    pub fn with(mut self, rule: impl BeliefRule + 'static) -> Self {
        self.add(rule);
        self
    }

    /// Adds a rule.
    pub fn add(&mut self, rule: impl BeliefRule + 'static) {
        self.rules.push(Box::new(rule));
    }

    /// Whether a rule claims this kind.
    pub fn handles(&self, kind: &str) -> bool {
        self.rules.iter().any(|rule| rule.kind() == kind)
    }

    /// The kinds this set claims.
    pub fn kinds(&self) -> Vec<&str> {
        self.rules.iter().map(|rule| rule.kind()).collect()
    }

    /// What this event means, or `None` if no rule claims its kind.
    ///
    /// `Some(vec![])` means a rule claimed the event and found nothing to
    /// change — which is different from no rule claiming it at all, because the
    /// second case is what the model gets asked about.
    pub fn judge(&self, event: &Event) -> Option<Vec<Judgement>> {
        let kind = event.kind().as_str();
        self.rules
            .iter()
            .find(|rule| rule.kind() == kind)
            .map(|rule| rule.judge(event))
    }
}

/// Reads `key=value` out of an event's text payload.
///
/// The simplest useful rule: an observation that names a setting and a value
/// asserts exactly that. It is deterministic, so the same history always yields
/// the same beliefs — which a model cannot promise.
#[derive(Debug, Clone)]
pub struct KeyValueRule {
    kind: String,
    confidence: Confidence,
}

impl KeyValueRule {
    /// Handles `kind`, claiming its value with `confidence`.
    pub fn new(kind: impl Into<String>, confidence: Confidence) -> Self {
        KeyValueRule {
            kind: kind.into(),
            confidence,
        }
    }
}

impl BeliefRule for KeyValueRule {
    fn kind(&self) -> &str {
        &self.kind
    }

    fn judge(&self, event: &Event) -> Vec<Judgement> {
        let text = String::from_utf8_lossy(event.payload());
        let Some((key, value)) = text.split_once('=') else {
            return Vec::new();
        };

        let key = key.trim();
        if key.is_empty() {
            return Vec::new();
        }

        vec![
            Judgement::assert_text(key, value.trim(), self.confidence)
                .with_rationale(format!("the event states {key} directly")),
        ]
    }
}
