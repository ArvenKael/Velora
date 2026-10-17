//! Replays a turn captured from a real run, if one happens to be on disk.
//!
//! Ignored by default: it reads `target/velora/history.db`, which exists on the
//! machine where the bug happened and nowhere else.

use velora_event::Payload;
use velora_reason::{ReasonRequest, ReasonThought, Turn};

#[test]
#[ignore = "reads a local history database"]
fn a_captured_turn_replays_into_a_conversation_a_provider_would_read() {
    let path = std::path::Path::new("../../target/velora/history.db");
    if !path.exists() {
        eprintln!("no history to check");
        return;
    }

    let store = velora_storage::SqliteStore::open(path).unwrap();
    let records = velora_event::Store::read(&store).unwrap();

    // Every turn in the history, not just the one that happened to break: a
    // rebuilt turn that a provider would refuse is a bug wherever it is.
    let opened: Vec<&velora_event::Record> = records
        .iter()
        .filter(|record| record.kind().as_str() == ReasonRequest::KIND)
        .collect();

    assert!(!opened.is_empty(), "no turns in this history");

    let mut checked = 0;
    let mut broken = Vec::new();
    // Turns where a chain of thought was recorded and did not come back out of
    // the rebuild. That is the bug this whole area keeps producing: DeepSeek
    // demands the reasoning be given back, and a replay that drops it produces a
    // conversation the API refuses with "The `reasoning_content` in the thinking
    // mode must be passed back to the API."
    let mut lost: Vec<(u64, usize, usize)> = Vec::new();
    // Assistant messages that thought nothing at all. Harmless — the wire fills
    // in an empty field for those — but worth seeing.
    let mut unthought = 0;

    for record in &opened {
        let Ok(request) = ReasonRequest::decode(record.version(), record.payload()) else {
            continue;
        };

        let turn = Turn::replay(record.sequence(), &request, &records);
        checked += 1;

        let mut open: Vec<String> = Vec::new();
        let mut asking = 0;

        for message in turn.messages() {
            match message.role() {
                velora_llm::Role::Assistant if !message.tool_calls().is_empty() => {
                    asking += 1;

                    if message.reasoning().is_none_or(|why| why.trim().is_empty()) {
                        unthought += 1;
                    }

                    if !open.is_empty() {
                        broken.push((record.sequence().get(), open.clone()));
                    }
                    open = message
                        .tool_calls()
                        .iter()
                        .map(|call| call.id().to_owned())
                        .collect();
                }
                velora_llm::Role::Tool => {
                    let id = message.tool_call_id().unwrap_or("").to_owned();
                    open.retain(|each| each != &id);
                }
                _ => {}
            }
        }

        if !open.is_empty() {
            broken.push((record.sequence().get(), open.clone()));
        }

        // What the log holds, against what came back out.
        let thought = records
            .iter()
            .filter(|held| held.kind().as_str() == ReasonThought::KIND)
            .filter_map(|held| ReasonThought::decode(held.version(), held.payload()).ok())
            .filter(|thought| {
                thought.turn() == record.sequence() && !thought.reasoning().trim().is_empty()
            })
            .count();
        let carried = turn
            .messages()
            .iter()
            .filter(|message| message.reasoning().is_some_and(|why| !why.trim().is_empty()))
            .count();

        if thought != carried {
            lost.push((record.sequence().get(), thought, carried));
        }

        // Reported so the shape of a real turn is visible rather than assumed.
        println!(
            "turn #{}: {} messages, {} asking for calls",
            record.sequence(),
            turn.messages().len(),
            asking
        );
    }

    for (turn, recorded, carried) in &lost {
        println!("turn #{turn}: {recorded} chains of thought recorded, {carried} came back");
    }

    println!(
        "checked {checked} turns, {} unanswered calls, {} lost chains of thought, \
         {unthought} messages that thought nothing",
        broken.len(),
        lost.len()
    );

    assert!(
        lost.is_empty(),
        "a chain of thought in the log has to come back out of the rebuild: {lost:?}"
    );

    assert!(
        broken.is_empty(),
        "turns from the real history still have unanswered calls: {broken:?}"
    );
}
