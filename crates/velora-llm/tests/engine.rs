//! Engine behavior: retries, deadlines, refusal, and what it counts.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use velora_llm::{
    Capabilities, Engine, EngineError, Provider, ProviderError, Request, Response, RetryPolicy,
    StopReason, Usage,
};

/// A provider that fails a set number of times before answering.
struct Scripted {
    failures: usize,
    retryable: bool,
    delay: Option<Duration>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }

    fn model(&self) -> &str {
        "stub"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    async fn complete(&self, _request: &Request) -> Result<Response, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);

        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }

        if call < self.failures {
            return Err(if self.retryable {
                ProviderError::Transport("connection reset".to_owned())
            } else {
                ProviderError::Configuration("no such model".to_owned())
            });
        }

        Ok(Response::new(
            Some("ok".to_owned()),
            Vec::new(),
            "stub",
            Some(StopReason::EndTurn),
            Some(Usage::new(Some(3), Some(4))),
        ))
    }
}

fn engine(script: Scripted, attempts: u32) -> Engine {
    Engine::new(script).with_retry_policy(RetryPolicy::new(
        attempts,
        Duration::from_millis(1),
        Duration::from_millis(5),
    ))
}

#[tokio::test]
async fn a_failure_worth_retrying_is_retried_until_it_works() {
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(
        Scripted {
            failures: 2,
            retryable: true,
            delay: None,
            calls: Arc::clone(&calls),
        },
        3,
    );

    let response = engine.complete(&Request::user("hi")).await.unwrap();
    assert_eq!(response.text(), Some("ok"));
    assert_eq!(calls.load(Ordering::SeqCst), 3, "two failures then a success");

    let stats = engine.stats();
    assert_eq!(stats.attempts, 3);
    assert_eq!(stats.retries, 2);
    assert_eq!(stats.failures, 0);
}

#[tokio::test]
async fn a_failure_not_worth_retrying_stops_immediately() {
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(
        Scripted {
            failures: 9,
            retryable: false,
            delay: None,
            calls: Arc::clone(&calls),
        },
        3,
    );

    let err = engine.complete(&Request::user("hi")).await.unwrap_err();
    assert!(matches!(err, EngineError::Provider(_)), "{err}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a bad key or a bad model is not made better by asking again"
    );
}

#[tokio::test]
async fn giving_up_reports_how_many_attempts_were_made() {
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(
        Scripted {
            failures: 99,
            retryable: true,
            delay: None,
            calls: Arc::clone(&calls),
        },
        3,
    );

    let err = engine.complete(&Request::user("hi")).await.unwrap_err();
    let EngineError::RetriesExhausted { attempts, .. } = err else {
        panic!("expected exhaustion, got {err}");
    };
    assert_eq!(attempts, 3);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(engine.stats().failures, 1);
}

#[tokio::test]
async fn an_attempt_that_hangs_is_cut_off() {
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = Engine::new(Scripted {
        failures: 0,
        retryable: true,
        delay: Some(Duration::from_secs(30)),
        calls: Arc::clone(&calls),
    })
    .with_retry_policy(RetryPolicy::none())
    .with_timeout(Duration::from_millis(20));

    let err = engine.complete(&Request::user("hi")).await.unwrap_err();
    let EngineError::TimedOut { attempts, .. } = err else {
        panic!("expected a timeout, got {err}");
    };
    assert_eq!(attempts, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn tokens_are_counted_across_calls() {
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(
        Scripted {
            failures: 0,
            retryable: true,
            delay: None,
            calls,
        },
        1,
    );

    engine.complete(&Request::user("one")).await.unwrap();
    engine.complete(&Request::user("two")).await.unwrap();

    let stats = engine.stats();
    assert_eq!(stats.attempts, 2);
    assert_eq!(stats.input_tokens, 6);
    assert_eq!(stats.output_tokens, 8);
}

#[tokio::test]
async fn the_engine_reports_what_it_is_talking_to() {
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(
        Scripted {
            failures: 0,
            retryable: true,
            delay: None,
            calls,
        },
        1,
    );

    assert_eq!(engine.provider_name(), "scripted");
    assert_eq!(engine.model(), "stub");
    assert_eq!(engine.capabilities(), Capabilities::FULL);
}

#[test]
fn a_retry_policy_always_permits_one_attempt() {
    assert_eq!(RetryPolicy::new(0, Duration::ZERO, Duration::ZERO).max_attempts(), 1);
}
