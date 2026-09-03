use alloy_primitives::B256;

use super::{
    notifications::{NotificationKind, delivery},
    *,
};

fn test_limits() -> RuntimeLimits {
    RuntimeLimits {
        retry_delay: Duration::from_millis(1),
        max_retry_delay: Duration::from_millis(4),
        connect_attempt_timeout: Duration::from_millis(10),
        connect_total_timeout: Duration::from_millis(100),
        rpc_request_timeout: Duration::from_millis(10),
        bootstrap_total_timeout: Duration::from_millis(100),
        block_verification_timeout: Duration::from_millis(100),
    }
}

fn test_backoff(limits: RuntimeLimits) -> Backoff {
    Backoff::new(limits.retry_delay, limits.max_retry_delay)
}

/// Terminal acquisition failures stop after the first attempt.
#[tokio::test]
async fn disable_error_is_not_retried() {
    let attempts = std::cell::Cell::new(0);
    let result = retry_transient(
        || {
            attempts.set(attempts.get() + 1);
            future::ready(Err::<(), _>(AttemptError::Disable(eyre::eyre!(
                "invalid genesis"
            ))))
        },
        "operation",
        Duration::from_secs(10),
        test_backoff(test_limits()),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(attempts.get(), 1);
}

/// Retry backoff cannot extend an acquisition beyond its total deadline.
#[tokio::test(start_paused = true)]
async fn retry_backoff_cannot_exceed_total_deadline() {
    let mut limits = test_limits();
    limits.retry_delay = Duration::from_secs(1);
    limits.max_retry_delay = Duration::from_secs(2);
    let started = tokio::time::Instant::now();
    let result = retry_transient(
        || future::ready(Err::<(), _>(AttemptError::retry(eyre::eyre!("offline")))),
        "bounded acquisition",
        Duration::from_millis(1500),
        test_backoff(limits),
    )
    .await;

    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("deadline exhausted")
    );
    assert_eq!(started.elapsed(), Duration::from_millis(1500));
}

/// A transient acquisition failure can recover within the configured budget.
#[tokio::test]
async fn transient_acquisition_recovers_within_budget() {
    let attempts = std::cell::Cell::new(0);
    let limits = test_limits();
    let result = retry_transient(
        || {
            attempts.set(attempts.get() + 1);
            future::ready(if attempts.get() == 1 {
                Err(AttemptError::retry(eyre::eyre!("disconnected")))
            } else {
                Ok(42)
            })
        },
        "recovering acquisition",
        limits.connect_total_timeout,
        test_backoff(limits),
    )
    .await;

    assert_eq!(result.unwrap(), 42);
    assert_eq!(attempts.get(), 2);
}

/// Replayed commits cannot regress or replace the latest delivered commit.
#[test]
fn replayed_commits_do_not_regress_or_conflict_with_the_delivered_tip() {
    let mut progress = RuntimeProgress::new(BlockNumHash::default());
    progress
        .accept_delivery(delivery(10, NotificationKind::Committed))
        .unwrap();
    progress
        .accept_delivery(delivery(5, NotificationKind::Committed))
        .unwrap();
    assert_eq!(progress.last_delivered_tip.number, 10);

    assert!(
        progress
            .accept_delivery(DeliveredNotification {
                tip: BlockNumHash::new(10, B256::repeat_byte(99)),
                kind: NotificationKind::Committed,
            })
            .is_err()
    );
    assert_eq!(
        progress.last_delivered_tip,
        delivery(10, NotificationKind::Committed).tip
    );

    assert!(
        progress
            .accept_delivery(delivery(7, NotificationKind::Reverted))
            .is_err()
    );
    assert_eq!(progress.last_delivered_tip.number, 10);
}

/// A hung block verification disables at its enclosing block deadline.
#[tokio::test(start_paused = true)]
async fn hung_block_verification_ends_at_its_deadline() {
    let limits = test_limits();
    let started = tokio::time::Instant::now();

    let result = with_block_timeout(
        future::pending::<Result<(), BlockError>>(),
        3,
        limits.block_verification_timeout,
    )
    .await;

    assert!(matches!(result, Err(BlockError::Disable(_))));
    assert_eq!(started.elapsed(), limits.block_verification_timeout);
}
