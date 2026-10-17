//! What the model is shown, and what it is told it was not shown.

use velora_belief::{Belief, BeliefStore, ClaimKey, ClaimValue, Confidence, Evidence};
use velora_context::{Assembler, Budget, Situation};
use velora_event::{
    EventLog, MemoryStore, PayloadVersion, Record, RecordDraft, Sensitivity, Sequence, Timestamp,
};

fn history(events: &[(&str, &str)]) -> Vec<Record> {
    let mut log = EventLog::open(MemoryStore::new()).unwrap();
    for (kind, text) in events {
        log.append(RecordDraft::new(
            *kind,
            PayloadVersion::new(1),
            text.as_bytes().to_vec(),
            Timestamp::from_parts(1, 0),
            Sensitivity::INTERNAL,
        ))
        .unwrap();
    }
    log.replay().unwrap()
}

fn store(entries: &[(&str, &str, u16, &[u64])]) -> BeliefStore {
    let mut store = BeliefStore::new();
    for (key, value, confidence, evidence) in entries {
        let at = evidence.iter().copied().max().unwrap_or(0) + 1;
        store
            .assert(Belief::new(
                ClaimKey::new(*key),
                ClaimValue::new(PayloadVersion::new(1), value.as_bytes().to_vec()),
                Confidence::from_basis_points(*confidence).unwrap(),
                Evidence::new(evidence.iter().copied().map(Sequence::new)).unwrap(),
                Sequence::new(at),
            ))
            .unwrap();
    }
    store
}

#[test]
fn the_task_comes_first() {
    let beliefs = BeliefStore::new();
    let context = Assembler::new().assemble(&Situation {
        task: "decide whether anything needs doing",
        beliefs: &beliefs,
        history: &[],
        focus: None,
    });

    assert!(
        context.render().starts_with("## The task\ndecide whether anything needs doing"),
        "{}",
        context.render()
    );
}

/// A thought is not always the only thing happening.
///
/// Once the system can work on two things at once, "install this" means
/// something different when something else is already half-installed, and a
/// question is a different question when the system is in the middle of
/// something the person asking may be asking about. The situation is about this
/// thought alone, so what else is open comes from the caller.
#[test]
fn what_the_system_is_already_doing_is_part_of_the_situation() {
    let beliefs = BeliefStore::new();
    let context = Assembler::new()
        .with_in_progress(vec![
            "Decide what to do about operator.wants. (last step: local.shell.run \
             {\"command\":\"brew install ffmpeg\"})"
                .to_owned(),
            "A step of a plan you are working through is ready. Do it, and then \
             mark it done with plan.step.done."
                .to_owned(),
        ])
        .assemble(&Situation {
            task: "answer the operator",
            beliefs: &beliefs,
            history: &[],
            focus: None,
        });

    let rendered = context.render();
    let at = rendered
        .find("## What you are already in the middle of")
        .unwrap_or_else(|| panic!("the section is missing: {rendered}"));

    // Before the beliefs: it changes what the task means, so it is read first.
    assert!(
        at < rendered.find("## What is believed").unwrap_or(usize::MAX),
        "it belongs with the task, not after the beliefs: {rendered}"
    );
    assert!(rendered.contains("brew install ffmpeg"), "{rendered}");
    assert!(rendered.contains("A step of a plan you are working through"), "{rendered}");
}

#[test]
fn nothing_else_running_is_not_a_section_at_all() {
    let beliefs = BeliefStore::new();
    let context = Assembler::new().assemble(&Situation {
        task: "answer the operator",
        beliefs: &beliefs,
        history: &[],
        focus: None,
    });

    assert!(
        !context.render().contains("already in the middle of"),
        "an empty section is noise: {}",
        context.render()
    );
}

/// The reason this crate exists: a conclusion without its grounds is something
/// to take on faith.
#[test]
fn a_belief_is_shown_with_the_events_behind_it() {
    let history = history(&[
        ("observation.text", "theme=dark"),
        ("observation.text", "unrelated"),
        ("observation.text", "theme=dark"),
    ]);
    let beliefs = store(&[("theme", "dark", 9_000, &[1, 3])]);

    let context = Assembler::new()
        // Recent events are a different section; this test is about evidence.
        .with_budget(Budget::default().with_max_recent_events(0))
        .assemble(&Situation {
            task: "what theme is in use?",
            beliefs: &beliefs,
            history: &history,
            focus: None,
        });
    let rendered = context.render();

    assert!(rendered.contains("- theme = dark (90.00% confident)"), "{rendered}");
    assert!(rendered.contains("#1 observation.text theme=dark"), "{rendered}");
    assert!(rendered.contains("#3 observation.text theme=dark"), "{rendered}");
    assert!(
        !rendered.contains("#2 observation.text unrelated"),
        "an event nobody cited should not appear as evidence: {rendered}"
    );
}

#[test]
fn the_reasoning_behind_a_belief_is_shown_too() {
    let history = history(&[("observation.text", "theme=dark")]);
    let mut beliefs = BeliefStore::new();
    beliefs
        .assert(
            Belief::new(
                ClaimKey::new("theme"),
                ClaimValue::new(PayloadVersion::new(1), b"dark".to_vec()),
                Confidence::from_basis_points(9_000).unwrap(),
                Evidence::one(Sequence::FIRST),
                Sequence::new(2),
            )
            .with_rationale("the setting states it directly"),
        )
        .unwrap();

    let context = Assembler::new().assemble(&Situation {
        task: "theme?",
        beliefs: &beliefs,
        history: &history,
        focus: None,
    });

    assert!(
        context.render().contains("because: the setting states it directly"),
        "{}",
        context.render()
    );
}

#[test]
fn the_belief_the_task_is_about_survives_a_tight_cap() {
    let history = history(&[("a", "one"), ("b", "two"), ("c", "three")]);
    let beliefs = store(&[("alpha", "1", 5_000, &[1]), ("theme", "dark", 5_000, &[3])]);

    let context = Assembler::new()
        .with_budget(Budget::default().with_max_beliefs(1))
        .assemble(&Situation {
            task: "what about the theme?",
            beliefs: &beliefs,
            history: &history,
            focus: None,
        });
    let rendered = context.render();

    assert!(rendered.contains("- theme = dark"), "{rendered}");
    assert!(
        !rendered.contains("- alpha"),
        "the irrelevant belief should have been the one dropped: {rendered}"
    );
}

#[test]
fn evidence_beyond_the_cap_is_counted_rather_than_hidden() {
    let history = history(&[("a", "one"), ("b", "two"), ("c", "three")]);
    let beliefs = store(&[("k", "v", 5_000, &[1, 2, 3])]);

    let context = Assembler::new()
        .with_budget(Budget::default().with_max_evidence_per_belief(1))
        .assemble(&Situation {
            task: "t",
            beliefs: &beliefs,
            history: &history,
            focus: None,
        });
    let rendered = context.render();

    assert!(rendered.contains("#3 c three"), "the most recent is kept: {rendered}");
    assert!(
        rendered.contains("2 earlier events not shown"),
        "leaving evidence out silently would be worse than leaving it out: {rendered}"
    );
}

#[test]
fn the_budget_bounds_what_the_model_actually_reads() {
    let history = history(&[("a", "one"), ("b", "two"), ("c", "three")]);
    let beliefs = store(&[
        ("alpha", "1", 5_000, &[1]),
        ("beta", "2", 5_000, &[2]),
        ("gamma", "3", 5_000, &[3]),
    ]);

    for ceiling in [120usize, 200, 400, 1_000] {
        let context = Assembler::new()
            .with_budget(Budget::default().with_max_total_chars(ceiling))
            .assemble(&Situation {
                task: "a task that takes up some room of its own, quite deliberately",
                beliefs: &beliefs,
                history: &history,
                focus: history.first(),
            });

        assert!(
            context.char_count() <= ceiling,
            "ceiling {ceiling} produced {} characters",
            context.char_count()
        );
    }
}

#[test]
fn what_did_not_fit_is_named() {
    let history = history(&[("a", "one")]);
    let beliefs = store(&[("k", "v", 5_000, &[1])]);

    let context = Assembler::new()
        .with_budget(Budget::default().with_max_total_chars(120))
        .assemble(&Situation {
            task: "a task long enough to use up the little room there is here",
            beliefs: &beliefs,
            history: &history,
            focus: None,
        });

    assert!(context.was_truncated());
    assert!(
        !context.omitted().is_empty(),
        "a thought taken without part of the picture should say so"
    );
    assert!(context.render().contains("Left out for length"), "{}", context.render());
}

#[test]
fn a_belief_whose_evidence_is_missing_says_so() {
    let full = history(&[("a", "one"), ("b", "two"), ("c", "three")]);
    let beliefs = store(&[("k", "v", 5_000, &[3])]);

    // Only the first record is offered as history, so the cited event is gone.
    let context = Assembler::new().assemble(&Situation {
        task: "t",
        beliefs: &beliefs,
        history: &full[..1],
        focus: None,
    });

    assert!(
        context.render().contains("could not be resolved"),
        "a belief nobody can check should not look checkable: {}",
        context.render()
    );
}

#[test]
fn what_prompted_the_thought_is_shown() {
    let history = history(&[("observation.text", "the operator asked for a summary")]);
    let beliefs = BeliefStore::new();

    let context = Assembler::new().assemble(&Situation {
        task: "decide what to do",
        beliefs: &beliefs,
        history: &history,
        focus: history.first(),
    });

    let rendered = context.render();
    assert!(rendered.contains("## What prompted this"), "{rendered}");
    assert!(rendered.contains("the operator asked for a summary"), "{rendered}");
}

#[test]
fn nothing_believed_is_said_plainly() {
    let context = Assembler::new().assemble(&Situation {
        task: "t",
        beliefs: &BeliefStore::new(),
        history: &[],
        focus: None,
    });

    let rendered = context.render();
    assert!(rendered.contains("Nothing is currently believed."), "{rendered}");
    assert!(rendered.contains("Nothing has happened yet."), "{rendered}");
}

/// A thought has to be explainable by what it was given, which it cannot be if
/// what it was given changes on its own.
#[test]
fn assembly_is_deterministic() {
    let history = history(&[("a", "one"), ("b", "two"), ("c", "three")]);
    let beliefs = store(&[("alpha", "1", 5_000, &[1]), ("beta", "2", 6_000, &[2, 3])]);
    let assembler = Assembler::new();
    let situation = Situation {
        task: "decide something",
        beliefs: &beliefs,
        history: &history,
        focus: history.last(),
    };

    assert_eq!(
        assembler.assemble(&situation).render(),
        assembler.assemble(&situation).render()
    );
}

/// Newlines inside a payload must not look like more events than there were.
#[test]
fn a_multi_line_payload_stays_one_line() {
    let history = history(&[("note", "first line\nsecond line\nthird line")]);
    let beliefs = store(&[("k", "v", 5_000, &[1])]);

    let context = Assembler::new().assemble(&Situation {
        task: "t",
        beliefs: &beliefs,
        history: &history,
        focus: None,
    });

    for line in context.render().lines() {
        assert!(
            !line.starts_with("second line"),
            "a payload's own lines should be flattened: {}",
            context.render()
        );
    }
    assert!(context.render().contains("first line second line third line"));
}
