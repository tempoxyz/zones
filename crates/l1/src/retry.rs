//! Shared classification and bounded backoff for replay-safe Tempo L1 reads.
//!
//! Retry individual reads before converting transport errors to RPC errors or strings. Do not
//! wrap transaction submission, remote proving, persistence, or proof verification in this helper.

use std::{future::Future, time::Duration};

use alloy_transport::{
    RpcError, TransportError, TransportErrorKind,
    layers::{RateLimitRetryPolicy, RetryPolicy as _},
};
use eyre::Result;

/// Retry budget and delays for one L1 read or acquisition operation.
#[derive(Clone, Copy, Debug)]
pub struct L1ReadRetryConfig {
    /// Total attempts, including the initial request. Must be at least one.
    pub max_attempts: u32,
    /// Delay after the first failed attempt.
    pub initial_delay: Duration,
    /// Maximum delay between attempts.
    pub max_delay: Duration,
}

impl Default for L1ReadRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// Bounded exponential backoff shared by request and acquisition retries.
#[derive(Debug)]
pub struct L1ReadBackoff {
    config: L1ReadRetryConfig,
    attempts: u32,
    delay: Duration,
}

impl L1ReadBackoff {
    /// Start a retry budget. A zero attempt limit still permits the initial request.
    pub fn new(config: L1ReadRetryConfig) -> Self {
        Self {
            config,
            attempts: 0,
            delay: config.initial_delay.min(config.max_delay),
        }
    }

    /// Charge one failed attempt, returning whether the budget is exhausted.
    pub fn exhausted(&mut self) -> bool {
        self.attempts = self.attempts.saturating_add(1);
        self.attempts >= self.config.max_attempts
    }

    /// Record a failure, returning an error when the shared acquisition budget is exhausted.
    pub fn record(&mut self, error: &eyre::Report, operation: &str) -> Result<()> {
        record_retry_attempt(
            &mut self.attempts,
            self.config.max_attempts,
            error,
            operation,
        )
    }

    /// Number of failed attempts charged to this budget.
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Total attempt limit for this budget.
    pub const fn max_attempts(&self) -> u32 {
        self.config.max_attempts
    }

    /// Wait before retrying and increase the next delay up to the configured cap.
    pub async fn wait(&mut self) {
        self.wait_with_hint(None).await;
    }

    /// Like [`Self::wait`], but wait at least as long as a server-provided retry hint.
    pub async fn wait_with_hint(&mut self, hint: Option<Duration>) {
        tokio::time::sleep(hint.unwrap_or_default().max(self.delay)).await;
        self.delay = self.delay.saturating_mul(2).min(self.config.max_delay);
    }
}

/// Charge one failed attempt against `max_attempts`, returning an error once it is exhausted.
pub fn record_retry_attempt(
    attempts: &mut u32,
    max_attempts: u32,
    error: &eyre::Report,
    operation: &str,
) -> Result<()> {
    *attempts = attempts.saturating_add(1);
    eyre::ensure!(
        *attempts < max_attempts,
        "{operation} retry budget exhausted after {attempts} failed attempts: {error:#}"
    );
    Ok(())
}

/// Classify retryable L1 acquisition errors without relying on provider-specific messages.
///
/// Internal errors (`-32603`) are retried because checker acquisition and storage reads can hit
/// backend resets during rollouts. [`retry_l1_read`] does not retry them.
pub fn is_retryable_rpc_error(error: &TransportError) -> bool {
    is_retryable_read_error(error)
        || matches!(error, RpcError::ErrorResp(error) if error.code == -32603)
}

/// Classify errors that [`retry_l1_read`] replays.
///
/// Excludes internal errors (`-32603`), which are often deterministic for witness and proof reads
/// (for example pruned state) and would otherwise exhaust the full budget before surfacing.
fn is_retryable_read_error(error: &TransportError) -> bool {
    match error {
        RpcError::ErrorResp(error) => error.is_retry_err() || matches!(error.code, -32001 | -32002),
        RpcError::UnsupportedFeature(_)
        | RpcError::LocalUsageError(_)
        | RpcError::SerError(_)
        | RpcError::DeserError { .. }
        | RpcError::Transport(TransportErrorKind::NonRetryable(_)) => false,
        _ => true,
    }
}

/// Find a retryable structured transport error through a read error's context.
fn retryable_read_error(error: &eyre::Report) -> Option<&TransportError> {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<TransportError>())
        .find(|error| is_retryable_read_error(error))
}

/// Retry a replay-safe L1 read, preserving the original error on terminal failure or exhaustion.
///
/// Waits honor server-provided retry hints when they exceed the current backoff delay.
pub async fn retry_l1_read<T, F, Fut>(
    config: L1ReadRetryConfig,
    operation: &str,
    mut read: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut backoff = L1ReadBackoff::new(config);
    loop {
        let error = match read().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Some(hint) = retryable_read_error(&error)
            .map(|error| RateLimitRetryPolicy::default().backoff_hint(error))
        else {
            return Err(error);
        };
        if backoff.exhausted() {
            tracing::warn!(
                target: "zone::l1::retry",
                %error,
                operation,
                attempts = backoff.attempts(),
                "L1 read retry budget exhausted"
            );
            return Err(error);
        }
        tracing::warn!(
            target: "zone::l1::retry",
            %error,
            operation,
            attempt = backoff.attempts(),
            max_attempts = backoff.max_attempts(),
            "L1 read failed; retrying"
        );
        backoff.wait_with_hint(hint).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    use alloy_provider::{Provider as _, ProviderBuilder, mock::Asserter};

    fn rpc_error(code: i64, message: &str) -> TransportError {
        RpcError::ErrorResp(
            serde_json::from_value(serde_json::json!({"code": code, "message": message})).unwrap(),
        )
    }

    #[test]
    fn classifies_checker_codes_and_terminal_errors() {
        for code in [-32001, -32002, -32603, -32005, 429, 1008, -32055] {
            assert!(is_retryable_rpc_error(&rpc_error(code, "")), "{code}");
        }
        for code in [-32000, -32600, -32601, -32602, -32004, 3] {
            assert!(!is_retryable_rpc_error(&rpc_error(code, "")), "{code}");
        }
        assert!(is_retryable_rpc_error(&rpc_error(-32000, "rate limited")));
        assert!(is_retryable_rpc_error(&TransportErrorKind::backend_gone()));
        assert!(!is_retryable_rpc_error(
            &TransportErrorKind::non_retryable_str("invalid request")
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn retries_the_same_rpc_read_after_rate_limiting() {
        let asserter = Asserter::new();
        asserter.push_failure(
            serde_json::from_value(serde_json::json!({
                "code": -32005,
                "message": "opaque",
                "data": {"rate": {"backoff_seconds": 3}},
            }))
            .unwrap(),
        );
        asserter.push_success(&42_u64);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let attempts = Cell::new(0);
        let start = tokio::time::Instant::now();
        let value = retry_l1_read(L1ReadRetryConfig::default(), "eth_getMultiProof", || {
            attempts.set(attempts.get() + 1);
            async {
                Ok(provider
                    .client()
                    .request::<_, u64>("eth_getMultiProof", (7,))
                    .await?)
            }
        })
        .await
        .unwrap();

        assert_eq!(value, 42);
        assert_eq!(attempts.get(), 2);
        // The server hint outlasts the 1s initial backoff.
        assert_eq!(start.elapsed(), Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn bounds_attempts_and_preserves_the_structured_error() {
        let config = L1ReadRetryConfig {
            max_attempts: 4,
            max_delay: Duration::from_secs(2),
            ..Default::default()
        };
        let attempts = Cell::new(0);
        let start = tokio::time::Instant::now();
        let error = retry_l1_read(config, "L1 read", || {
            attempts.set(attempts.get() + 1);
            async { Err::<(), _>(eyre::Report::new(rpc_error(-32002, "upstream unavailable"))) }
        })
        .await
        .unwrap_err();

        assert_eq!(attempts.get(), 4);
        assert_eq!(start.elapsed(), Duration::from_secs(5));
        assert!(matches!(
            error.downcast_ref::<TransportError>(),
            Some(RpcError::ErrorResp(error)) if error.code == -32002
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_rpc_and_validation_errors_are_not_replayed() {
        for error in [
            eyre::Report::new(rpc_error(-32602, "invalid params")),
            // Often deterministic for proofs, e.g. pruned state.
            eyre::Report::new(rpc_error(-32603, "state at block is pruned")),
            eyre::eyre!("proof mismatch"),
        ] {
            let attempts = Cell::new(0);
            let mut error = Some(error);
            let start = tokio::time::Instant::now();
            let result = retry_l1_read(L1ReadRetryConfig::default(), "L1 read", || {
                attempts.set(attempts.get() + 1);
                let error = error.take().expect("terminal error must not be retried");
                async { Err::<(), _>(error) }
            })
            .await;

            assert!(result.is_err());
            assert_eq!(attempts.get(), 1);
            assert_eq!(start.elapsed(), Duration::ZERO);
        }
    }

    #[test]
    fn preserves_retry_classification_through_context() {
        let error = eyre::Report::new(rpc_error(-32002, "upstream unavailable"))
            .wrap_err("witness acquisition");
        assert!(retryable_read_error(&error).is_some());
        assert!(retryable_read_error(&eyre::eyre!("rate limited")).is_none());
    }

    #[test]
    fn acquisition_retries_are_bounded() {
        let mut attempts = 0;
        let error = eyre::eyre!("state unavailable");
        for _ in 1..3 {
            record_retry_attempt(&mut attempts, 3, &error, "local state acquisition").unwrap();
        }
        let error = record_retry_attempt(&mut attempts, 3, &error, "local state acquisition")
            .expect_err("the final attempt must exhaust the budget");
        assert_eq!(attempts, 3);
        assert!(error.to_string().contains("retry budget exhausted"));
    }
}
