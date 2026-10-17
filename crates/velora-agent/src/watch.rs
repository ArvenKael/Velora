/// An event kind worth thinking about.
///
/// The same idea as a [`Watch`], for an event rather than a claim: a kind, and
/// the task to think about when one arrives. Kept separate because a claim has a
/// key to substitute into the task and an event has nothing — the event itself is
/// the subject, and the task says what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventWatch {
    kind: String,
    task: String,
    idle_only: bool,
}

impl EventWatch {
    /// Watches one kind of event.
    pub fn kind(kind: impl Into<String>, task: impl Into<String>) -> Self {
        EventWatch {
            kind: kind.into(),
            task: task.into(),
            idle_only: false,
        }
    }

    /// Only think about this when nothing else is being thought about.
    ///
    /// For a kind that exists to fill a quiet moment rather than to report
    /// something: a pulse that arrives while the system is already working is not
    /// a second thing to do, it is the wrong moment, and acting on it would open
    /// a turn about nothing on top of a turn about something.
    pub fn only_when_idle(mut self) -> Self {
        self.idle_only = true;
        self
    }

    /// Whether it waits for a quiet moment.
    pub fn wants_idle(&self) -> bool {
        self.idle_only
    }

    /// Whether this is the kind watched for.
    pub fn matches(&self, kind: &str) -> bool {
        self.kind == kind
    }

    /// What to ask about it.
    pub fn task(&self) -> &str {
        &self.task
    }

    /// The kind as written.
    pub fn watched(&self) -> &str {
        &self.kind
    }
}

/// A belief change worth thinking about.
///
/// The system cannot think about everything it comes to believe; thinking costs
/// a model call and a turn of its attention. A watch is how that is decided,
/// and it is deliberately a *rule* rather than a judgement: deciding whether to
/// think should be cheap, predictable, and readable, so that what the system
/// pays attention to is something a person can review rather than something a
/// model chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watch {
    matching: Matching,
    task: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Matching {
    /// One claim, exactly.
    Key(String),
    /// Every claim whose key starts with this.
    Prefix(String),
}

impl Watch {
    /// Watches one claim.
    pub fn key(key: impl Into<String>, task: impl Into<String>) -> Self {
        Watch {
            matching: Matching::Key(key.into()),
            task: task.into(),
        }
    }

    /// Watches every claim under a prefix, such as `file.` or `device.`.
    pub fn prefix(prefix: impl Into<String>, task: impl Into<String>) -> Self {
        Watch {
            matching: Matching::Prefix(prefix.into()),
            task: task.into(),
        }
    }

    /// Whether a claim is one this watches.
    pub fn matches(&self, key: &str) -> bool {
        match &self.matching {
            Matching::Key(watched) => watched == key,
            Matching::Prefix(prefix) => key.starts_with(prefix.as_str()),
        }
    }

    /// What to ask about a particular change.
    ///
    /// `{key}` in the task is replaced with the claim, so one watch over a
    /// prefix can ask a question about whichever claim moved.
    pub fn task_for(&self, key: &str) -> String {
        self.task.replace("{key}", key)
    }

    /// The task as written.
    pub fn task(&self) -> &str {
        &self.task
    }
}
