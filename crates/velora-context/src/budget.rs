/// The bounds on what the model is shown.
///
/// A context is not free: it is sent on every thought, and it is assembled from
/// things that grow — beliefs accumulate, and evidence for them accumulates
/// faster. Every one of these is a decision about what to leave out, and the
/// assembly reports what it left out rather than pretending it showed
/// everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    max_beliefs: usize,
    max_evidence_per_belief: usize,
    max_recent_events: usize,
    max_transcript: usize,
    max_field_chars: usize,
    max_total_chars: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_beliefs: 12,
            max_evidence_per_belief: 4,
            max_recent_events: 20,
            max_transcript: 20,
            max_field_chars: 400,
            max_total_chars: 12_000,
        }
    }
}

impl Budget {
    /// How many beliefs may be shown.
    pub fn max_beliefs(&self) -> usize {
        self.max_beliefs
    }

    /// How many backing events may be shown for each.
    pub fn max_evidence_per_belief(&self) -> usize {
        self.max_evidence_per_belief
    }

    /// How many recent events may be shown.
    pub fn max_recent_events(&self) -> usize {
        self.max_recent_events
    }

    /// How many things said may be shown.
    ///
    /// Counted in utterances rather than in turns, because a turn is not a unit
    /// of a conversation: the operator may say three things before the system
    /// answers once, and cutting between them is what would lose the thread.
    pub fn max_transcript(&self) -> usize {
        self.max_transcript
    }

    /// How long any single quoted value may be.
    pub fn max_field_chars(&self) -> usize {
        self.max_field_chars
    }

    /// The ceiling on the whole context.
    pub fn max_total_chars(&self) -> usize {
        self.max_total_chars
    }

    /// Sets how many beliefs may be shown.
    pub fn with_max_beliefs(mut self, limit: usize) -> Self {
        self.max_beliefs = limit;
        self
    }

    /// Sets how many backing events may be shown for each belief.
    pub fn with_max_evidence_per_belief(mut self, limit: usize) -> Self {
        self.max_evidence_per_belief = limit;
        self
    }

    /// Sets how many recent events may be shown.
    pub fn with_max_recent_events(mut self, limit: usize) -> Self {
        self.max_recent_events = limit;
        self
    }

    /// Sets how many things said may be shown.
    pub fn with_max_transcript(mut self, limit: usize) -> Self {
        self.max_transcript = limit;
        self
    }

    /// Sets how long any single quoted value may be.
    pub fn with_max_field_chars(mut self, limit: usize) -> Self {
        self.max_field_chars = limit;
        self
    }

    /// Sets the ceiling on the whole context.
    pub fn with_max_total_chars(mut self, limit: usize) -> Self {
        self.max_total_chars = limit;
        self
    }
}
