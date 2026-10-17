use std::sync::Arc;

use velora_belief::{Belief, BeliefStore};
use velora_operator::Transcript;
use velora_event::Record;

use crate::budget::Budget;
use crate::context::{Context, Section, fit};
use crate::explain::{Explain, Plain};

/// What the system is being asked to think about, and what it has to think
/// with.
///
/// Deliberately a description of the situation rather than a prompt: the
/// standing instructions and the persona belong to whoever is doing the
/// thinking, and this is only the part that comes from history.
pub struct Situation<'a> {
    /// The question, goal or change being thought about.
    pub task: &'a str,
    /// What is currently believed.
    pub beliefs: &'a BeliefStore,
    /// The history that belief evidence is resolved against.
    pub history: &'a [Record],
    /// The event that prompted this thought, if there is one.
    pub focus: Option<&'a Record>,
}

/// Builds what the model is shown.
///
/// The point of this crate, and the reason it is separate: a belief is only
/// worth showing if what backs it comes with it. A model told *that* something
/// is believed can only take it on faith or ignore it; a model told that it is
/// believed *and* shown the events it was drawn from can disagree with the
/// reasoning instead of with the conclusion.
///
/// Nothing here reaches for anything. No clock, no network, no randomness — the
/// same beliefs about the same history produce the same context, so a thought
/// can be explained afterwards by what it was given.
#[derive(Debug, Clone)]
pub struct Assembler {
    budget: Budget,
    explain: Arc<dyn Explain>,
    /// What the system is already in the middle of, as the caller knows it.
    ///
    /// A situation is about *this* thought, so it cannot say what else is open:
    /// two turns can be running at once — an install while the operator is being
    /// answered — and the one being assembled is only one of them. A thought that
    /// does not know the others exist answers as though the system were idle, or
    /// starts work that is already running.
    in_progress: Vec<String>,
}

impl Default for Assembler {
    fn default() -> Self {
        Assembler {
            budget: Budget::default(),
            explain: Arc::new(Plain),
            in_progress: Vec::new(),
        }
    }
}

impl Assembler {
    /// Builds a context assembler that reads payloads as text.
    pub fn new() -> Self {
        Assembler::default()
    }

    /// Sets different bounds.
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// Sets how records are read back for the model.
    ///
    /// Without this, every event is shown as its raw bytes — which is the
    /// literal truth and close to useless: a belief is a claim, a confidence and
    /// a list of sequences, and none of that survives being read as text.
    pub fn with_explain(mut self, explain: Arc<dyn Explain>) -> Self {
        self.explain = explain;
        self
    }

    /// Sets what the system is already busy with.
    pub fn with_in_progress(mut self, in_progress: Vec<String>) -> Self {
        self.in_progress = in_progress;
        self
    }

    /// The bounds in force.
    pub fn budget(&self) -> Budget {
        self.budget
    }

    /// Assembles what the model is shown.
    pub fn assemble(&self, situation: &Situation<'_>) -> Context {
        let mut sections: Vec<Section> = Vec::new();
        let mut omitted: Vec<String> = Vec::new();
        let mut truncated = false;
        // Reserved so the note about what was left out always fits: a budget
        // that silently overruns is not a budget.
        let ceiling = self.budget.max_total_chars();
        let mut room = ceiling.saturating_sub((ceiling / 4).min(96));

        // The task first: everything after it is there to serve it.
        self.push(
            &mut sections,
            &mut omitted,
            &mut truncated,
            &mut room,
            "The task",
            situation.task.to_owned(),
        );

        if let Some(focus) = situation.focus {
            self.push(
                &mut sections,
                &mut omitted,
                &mut truncated,
                &mut room,
                "What prompted this",
                self.render_record(focus, self.budget.max_field_chars()),
            );
        }

        // What else is going on. Pushed early, and before the beliefs, because it
        // changes what the task means: "install this" is a different instruction
        // when something else is already half-installed, and a question is a
        // different question when the system is in the middle of something the
        // operator may be asking about.
        if !self.in_progress.is_empty() {
            let body = self
                .in_progress
                .iter()
                .map(|line| format!("- {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            self.push(
                &mut sections,
                &mut omitted,
                &mut truncated,
                &mut room,
                "What you are already in the middle of",
                body,
            );
        }

        // What has been said, before anything the system believes. It is the one
        // thing in the context that cannot be reconstructed from anything else:
        // beliefs are a summary, and the events behind them do not say who said
        // what. It is derived from two ordinary kinds of record — there is no
        // conversation stored anywhere — and if only one section survives the
        // budget it has to be this one, which is why it is pushed here and not
        // after the beliefs.
        let transcript = Transcript::read(situation.history);
        if !transcript.is_empty() {
            self.push(
                &mut sections,
                &mut omitted,
                &mut truncated,
                &mut room,
                "What has been said",
                self.render_transcript(&transcript, self.budget.max_field_chars()),
            );
        }

        let beliefs = situation
            .beliefs
            .relevant_to(situation.task, self.budget.max_beliefs());
        let body = self.render_beliefs(&beliefs, situation.history);
        self.push(
            &mut sections,
            &mut omitted,
            &mut truncated,
            &mut room,
            "What is believed, and the events behind it",
            body,
        );

        let recent = self.render_recent(
            situation.history,
            self.budget.max_recent_events(),
            self.budget.max_field_chars(),
        );
        self.push(
            &mut sections,
            &mut omitted,
            &mut truncated,
            &mut room,
            "What has happened lately",
            recent,
        );

        Context::new(sections, omitted, truncated, self.budget.max_total_chars())
    }

    /// Renders what has been said, oldest first, with who said it.
    ///
    /// The labels are the whole point. Without them it is a list of remarks, and
    /// the model reading it has no way to tell its own earlier words from what
    /// it was told — which is exactly the state that makes a dialogue impossible
    /// to follow.
    fn render_transcript(&self, transcript: &Transcript, field: usize) -> String {
        let limit = self.budget.max_transcript();
        if limit == 0 {
            return "(not shown)\n".to_owned();
        }

        let shown = transcript.recent(limit);
        let mut out = String::new();

        let hidden = transcript.len() - shown.len();
        if hidden > 0 {
            out.push_str(&format!(
                "({hidden} earlier thing{} said not shown)\n",
                if hidden == 1 { "" } else { "s" }
            ));
        }

        for utterance in shown {
            out.push_str(utterance.speaker().line_label());
            out.push_str(": ");
            out.push_str(&clip(utterance.text(), field));
            out.push('\n');
        }

        // A dialogue with a hole in it is worse than one that says it has a
        // hole. This is the only place the model can be told that something was
        // said that nobody can read back.
        if transcript.unreadable() > 0 {
            out.push_str(&format!(
                "({} thing{} said could not be read back and {} shown as a placeholder)\n",
                transcript.unreadable(),
                if transcript.unreadable() == 1 { "" } else { "s" },
                if transcript.unreadable() == 1 { "is" } else { "are" },
            ));
        }

        out
    }

    /// Renders every belief with the events it cites.
    fn render_beliefs(&self, beliefs: &[Belief], history: &[Record]) -> String {
        if beliefs.is_empty() {
            return "Nothing is currently believed.\n".to_owned();
        }

        let mut out = String::new();
        let field = self.budget.max_field_chars();
        let shown = self.budget.max_evidence_per_belief();

        for belief in beliefs {
            out.push_str(&format!(
                "- {} = {} ({} confident)\n",
                belief.key(),
                clip(&String::from_utf8_lossy(belief.value().bytes()), field),
                belief.confidence()
            ));

            if !belief.rationale().is_empty() {
                out.push_str(&format!("  because: {}\n", clip(belief.rationale(), field)));
            }

            out.push_str("  drawn from:\n");
            match belief.evidence().resolve(history) {
                Ok(records) => {
                    let total = records.len();
                    let start = total.saturating_sub(shown);
                    if start > 0 {
                        out.push_str(&format!(
                            "    ({} earlier event{} not shown)\n",
                            start,
                            if start == 1 { "" } else { "s" }
                        ));
                    }
                    for record in &records[start..] {
                        out.push_str("    ");
                        out.push_str(&self.render_record(record, field));
                        out.push('\n');
                    }
                }
                // Worth saying rather than hiding: a belief whose evidence
                // cannot be found is a belief nobody can check.
                Err(err) => out.push_str(&format!("    (could not be resolved: {err})\n")),
            }
        }

        out
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &self,
        sections: &mut Vec<Section>,
        omitted: &mut Vec<String>,
        truncated: &mut bool,
        room: &mut usize,
        title: &str,
        body: String,
    ) {
        // The heading is part of what the model reads, so it is part of what
        // the budget pays for.
        let overhead = title.chars().count() + 6;
        if *room <= overhead {
            omitted.push(title.to_owned());
            *truncated = true;
            return;
        }
        *room -= overhead;

        // An empty body means there was nothing to say, not that there was no
        // room; `fit` reports which of those happened.
        let (body, cut) = fit(&body, *room);

        if cut {
            *truncated = true;
        }

        *room = room.saturating_sub(body.chars().count());
        sections.push(Section::new(title, body));
    }
}

impl Assembler {
    fn render_recent(&self, history: &[Record], limit: usize, field: usize) -> String {
        if history.is_empty() {
            return "Nothing has happened yet.\n".to_owned();
        }
        if limit == 0 {
            return "(not shown)\n".to_owned();
        }

        let start = history.len().saturating_sub(limit);
        let mut out = String::new();
        if start > 0 {
            out.push_str(&format!("({start} earlier events not shown)\n"));
        }
        for record in &history[start..] {
            out.push_str(&self.render_record(record, field));
            out.push('\n');
        }
        out
    }

    /// Renders one event as a single line: where it sits, what kind it is, and
    /// what the runtime's vocabulary says it means.
    ///
    /// One line per event on purpose: an event whose payload contains newlines
    /// would otherwise look like several events, and the model would be reading
    /// a shape that history does not have.
    fn render_record(&self, record: &Record, field: usize) -> String {
        format!(
            "#{} {} {}",
            record.sequence(),
            record.kind(),
            clip(&self.explain.explain(record), field)
        )
    }
}

/// Shortens a value to one readable line.
fn clip(text: &str, limit: usize) -> String {
    let flattened: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = flattened.trim();

    if trimmed.chars().count() <= limit {
        return trimmed.to_owned();
    }

    let head: String = trimmed.chars().take(limit.saturating_sub(1)).collect();
    format!("{head}…")
}

