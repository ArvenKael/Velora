//! How much of a plan there may be.
//!
//! These are bounds on history, not on ambition. Every plan, step and note is an
//! event in a log that is never rewritten, and a system that can add to a plan
//! without limit can make a log nobody can read.

/// The bounds a plan lives inside.
#[derive(Debug, Clone)]
pub struct Limits {
    max_plans: usize,
    max_steps: usize,
    max_id: usize,
    max_goal: usize,
    max_step: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_plans: 16,
            max_steps: 16,
            max_id: 64,
            max_goal: 1_024,
            max_step: 1_024,
        }
    }
}

impl Limits {
    /// How many plans may be open at once.
    pub fn max_plans(&self) -> usize {
        self.max_plans
    }

    /// How many steps a plan may have.
    pub fn max_steps(&self) -> usize {
        self.max_steps
    }

    /// How long an id may be.
    pub fn max_id(&self) -> usize {
        self.max_id
    }

    /// How long a goal may be.
    pub fn max_goal(&self) -> usize {
        self.max_goal
    }

    /// How long a step may be.
    pub fn max_step(&self) -> usize {
        self.max_step
    }

    /// Sets how many plans may be open at once.
    pub fn with_max_plans(mut self, limit: usize) -> Self {
        self.max_plans = limit.max(1);
        self
    }

    /// Sets how many steps a plan may have.
    pub fn with_max_steps(mut self, limit: usize) -> Self {
        self.max_steps = limit.max(1);
        self
    }
}
