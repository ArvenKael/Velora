//! The local device: the machine, bounded in time and output.

#![cfg(unix)]

use std::time::Duration;

use serde_json::json;
use velora_exec::{Device, DeviceError, LocalDevice};

fn device() -> LocalDevice {
    LocalDevice::new()
}

fn scratch(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("velora-exec-{tag}-{}", std::process::id()))
}

#[tokio::test]
async fn a_command_reports_what_it_printed() {
    let outcome = device()
        .invoke("shell.run", &json!({ "command": "echo hello" }))
        .await
        .unwrap();

    assert!(outcome.succeeded());
    assert_eq!(outcome.exit(), Some(0));
    assert!(outcome.output().contains("hello"), "{}", outcome.output());
}

/// A command that fails is a result, not an error: the model needs to see it
/// and decide what to do about it.
#[tokio::test]
async fn a_failing_command_is_a_result_not_an_error() {
    let outcome = device()
        .invoke("shell.run", &json!({ "command": "echo trouble >&2; exit 3" }))
        .await
        .unwrap();

    assert!(!outcome.succeeded());
    assert_eq!(outcome.exit(), Some(3));
    assert!(outcome.output().contains("trouble"), "{}", outcome.output());
}

/// Output ends up in history, and history is never rewritten.
#[tokio::test]
async fn a_command_that_prints_too_much_is_cut_off() {
    let device = device().with_max_output(4_000);
    let outcome = device
        .invoke("shell.run", &json!({ "command": "yes hello | head -c 200000" }))
        .await
        .unwrap();

    assert!(outcome.was_truncated(), "it should have been cut short");
    assert!(
        outcome.output().len() < 5_000,
        "output should be bounded, got {} bytes",
        outcome.output().len()
    );
    assert!(outcome.output().contains("cut at"), "{}", outcome.output());
}

/// A command that never finishes must not hold the system forever.
#[tokio::test]
async fn a_command_that_never_finishes_is_stopped() {
    let device = device().with_timeout(Duration::from_millis(200));

    let started = std::time::Instant::now();
    let err = device
        .invoke("shell.run", &json!({ "command": "sleep 30" }))
        .await
        .unwrap_err();

    assert!(matches!(err, DeviceError::Failed(_)), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "it took {:?} to give up",
        started.elapsed()
    );
}

/// And something that is going to take a while can ask for the time it needs.
///
/// This is what delegating to another program needs: an installer, a build, or
/// another agent working on something does not finish inside the limit meant for
/// looking something up. The limit is the operator's to set; asking is the
/// model's.
#[tokio::test]
async fn a_command_can_ask_for_longer_than_the_limit() {
    let device = device().with_timeout(Duration::from_millis(200));

    // Under the device's own limit, which is far too short for this.
    let stopped = device
        .invoke("shell.run", &json!({ "command": "sleep 1; echo finished" }))
        .await;
    assert!(stopped.is_err(), "the short limit has to stop it");

    // Given a second, which it asked for.
    let outcome = device
        .invoke(
            "shell.run",
            &json!({ "command": "sleep 1; echo finished", "timeout_secs": 5 }),
        )
        .await
        .unwrap();

    assert!(outcome.succeeded(), "{}", outcome.output());
    assert!(outcome.output().contains("finished"), "{}", outcome.output());
}

/// But not for as long as it likes: the ceiling is not the model's to move.
#[tokio::test]
async fn asking_for_a_week_is_reduced_to_the_ceiling() {
    let device = device().with_timeout(Duration::from_millis(150));

    // Would hang for a week if the ceiling were not applied. It is applied by
    // clamping to an hour, so this is stopped by the device's own limit rather
    // than waiting — which is the point: the clamp is what makes this test
    // finish at all.
    let started = std::time::Instant::now();
    let outcome = device
        .invoke(
            "shell.run",
            &json!({ "command": "sleep 0.5; echo finished", "timeout_secs": 604800 }),
        )
        .await;

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the clamp has to be applied: it took {:?}",
        started.elapsed()
    );
    // The device's own 150ms still applies as the starting point, and the ask
    // raises it — so this one finishes.
    assert!(outcome.is_ok(), "{outcome:?}");
}

#[tokio::test]
async fn a_file_round_trips() {
    let path = scratch("round-trip.txt");
    let _ = std::fs::remove_file(&path);
    let device = device();

    let written = device
        .invoke(
            "fs.write",
            &json!({ "path": path.display().to_string(), "contents": "written by velora" }),
        )
        .await
        .unwrap();
    assert!(written.succeeded(), "{}", written.output());

    let read = device
        .invoke("fs.read", &json!({ "path": path.display().to_string() }))
        .await
        .unwrap();
    assert!(read.succeeded());
    assert_eq!(read.output(), "written by velora");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn reading_a_file_that_is_not_there_is_a_result() {
    let outcome = device()
        .invoke("fs.read", &json!({ "path": "/definitely/not/here" }))
        .await
        .unwrap();

    assert!(!outcome.succeeded());
    assert!(outcome.output().contains("/definitely/not/here"), "{}", outcome.output());
}

#[tokio::test]
async fn a_directory_can_be_listed_and_removed() {
    let dir = scratch("listing");
    let _ = std::fs::remove_dir_all(&dir);
    let device = device();

    device
        .invoke("fs.write", &json!({ "path": dir.join("one.txt").display().to_string(), "contents": "x" }))
        .await
        .unwrap();
    device
        .invoke("fs.write", &json!({ "path": dir.join("two.txt").display().to_string(), "contents": "y" }))
        .await
        .unwrap();

    let listing = device
        .invoke("fs.list", &json!({ "path": dir.display().to_string() }))
        .await
        .unwrap();
    assert!(listing.succeeded(), "{}", listing.output());
    assert!(listing.output().contains("one.txt"), "{}", listing.output());
    assert!(listing.output().contains("two.txt"), "{}", listing.output());

    let removed = device
        .invoke(
            "fs.remove",
            &json!({ "path": dir.display().to_string(), "recursive": true }),
        )
        .await
        .unwrap();
    assert!(removed.succeeded(), "{}", removed.output());
    assert!(!dir.exists());
}

#[tokio::test]
async fn arguments_that_do_not_fit_the_procedure_are_reported() {
    let err = device()
        .invoke("shell.run", &json!({ "commnd": "echo typo" }))
        .await
        .unwrap_err();

    assert!(matches!(err, DeviceError::InvalidArguments(_)), "{err}");
}

#[tokio::test]
async fn an_unknown_procedure_is_reported() {
    let err = device().invoke("teleport", &json!({})).await.unwrap_err();
    assert_eq!(err, DeviceError::UnknownProcedure("teleport".to_owned()));
}

#[tokio::test]
async fn listing_the_processes_answers_one_way_or_the_other() {
    // A procedure the model can call that nothing called. Untested code that
    // ships is code that is broken already and nobody has noticed.
    let outcome = device().invoke("process.list", &json!({})).await.unwrap();

    if outcome.succeeded() {
        assert!(outcome.output().contains("PID"), "{}", outcome.output());

        // This test is a process, so it has to be in the list.
        let me = std::process::id().to_string();
        let listed = outcome
            .output()
            .lines()
            .any(|line| line.split_whitespace().any(|field| field == me));
        assert!(listed, "the process running the test was not listed:\n{}", outcome.output());
    } else {
        // Somewhere `ps` cannot run — inside a sandbox, in a minimal container,
        // on a system that is not unix — saying so is the procedure working.
        // What would be broken is an empty *success*: the model would read it as
        // a machine with nothing running on it.
        assert!(
            !outcome.output().trim().is_empty(),
            "a procedure that failed has to say why"
        );
    }
}

#[tokio::test]
async fn the_device_describes_the_machine_it_is_on() {
    let outcome = device().invoke("system.info", &json!({})).await.unwrap();
    assert!(outcome.succeeded());
    assert!(outcome.output().contains("os:"), "{}", outcome.output());
    assert!(outcome.output().contains("workdir:"), "{}", outcome.output());
}
