//! The ExEx notification stream: classifying deliveries, enforcing append-only history, and
//! driving other work without letting the stream stall the node.
//!
//! Only a delivered tip is retained. The checker walks the canonical provider range itself, so
//! notification payloads are never buffered or replayed.

use alloy_consensus::BlockHeader as _;
use alloy_eips::BlockNumHash;
use futures::{Stream, StreamExt as _};
use reth_exex::{ExExContext, ExExNotification};
use reth_node_api::{FullNodeComponents, NodePrimitives};

use super::{AppendOnlyViolation, PERSISTENCE_POLL_INTERVAL, RuntimeProgress};
#[cfg(test)]
use alloy_primitives::B256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Which kind of chain change a notification reported.
pub(super) enum NotificationKind {
    Committed,
    Reorged,
    Reverted,
}

#[derive(Debug, Clone, Copy)]
/// A notification reduced to the tip it reported, so payloads need not be retained.
pub(super) struct DeliveredNotification {
    pub(super) tip: BlockNumHash,
    pub(super) kind: NotificationKind,
}

#[derive(Debug)]
/// Whether driven work failed, or the notification stream did.
pub(super) enum DriveError<E> {
    Work(E),
    Notifications(eyre::Report),
}

/// Why the notification loop woke up.
pub(super) enum Wake {
    Delivered(eyre::Result<DeliveredNotification>),
    /// The persistence poll fired; nothing was delivered.
    Poll,
    Closed,
}

/// Await the next delivered tip or the persistence poll, shared by the verifying and disabled
/// loops. Callers apply their own error policy, which is the only way the two differ.
pub(super) async fn next_wake<Node>(ctx: &mut ExExContext<Node>, poll_persistence: bool) -> Wake
where
    Node: FullNodeComponents,
{
    tokio::select! {
        notification = ctx.notifications.next() => {
            match notification {
                Some(notification) => Wake::Delivered(
                    notification.and_then(|notification| classify_notification(&notification)),
                ),
                None => Wake::Closed,
            }
        }
        () = tokio::time::sleep(PERSISTENCE_POLL_INTERVAL), if poll_persistence => Wake::Poll,
    }
}

/// Drive `work` against this context's notification stream.
pub(super) async fn drive_exex_work<Node, F, T, E>(
    ctx: &mut ExExContext<Node>,
    progress: &mut RuntimeProgress,
    work: F,
) -> Result<T, DriveError<E>>
where
    Node: FullNodeComponents,
    F: std::future::Future<Output = Result<T, E>>,
{
    let notifications = ctx
        .notifications
        .by_ref()
        .map(|result| result.and_then(|notification| classify_notification(&notification)));
    drive_while_draining(work, notifications, progress).await
}

/// Flatten a drive failure once the distinction no longer matters.
pub(super) fn drive_eyre_error(error: DriveError<eyre::Report>) -> eyre::Report {
    match error {
        DriveError::Work(error) | DriveError::Notifications(error) => error,
    }
}

/// Classify one delivered notification and extract its lightweight tip for flow control.
pub(super) fn classify_notification<N: NodePrimitives>(
    notification: &ExExNotification<N>,
) -> eyre::Result<DeliveredNotification> {
    // A revert reports the tip it rewinds to; the other two report their own new canonical tip.
    let (new, kind) = match notification {
        ExExNotification::ChainCommitted { new } => (new, NotificationKind::Committed),
        ExExNotification::ChainReorged { new, .. } => (new, NotificationKind::Reorged),
        ExExNotification::ChainReverted { old } => {
            let (_, block) = old
                .blocks()
                .iter()
                .next()
                .ok_or_else(|| eyre::eyre!("received an empty reverted ExEx notification"))?;
            return Ok(DeliveredNotification {
                tip: block.parent_num_hash(),
                kind: NotificationKind::Reverted,
            });
        }
    };
    let (_, block) = new
        .blocks()
        .iter()
        .next_back()
        .ok_or_else(|| eyre::eyre!("received an empty {kind:?} ExEx notification"))?;
    Ok(DeliveredNotification {
        tip: block.num_hash(),
        kind,
    })
}

/// Reject anything but a commit: Zone history only ever extends.
pub(super) fn ensure_append_only(
    delivery: DeliveredNotification,
) -> Result<(), AppendOnlyViolation> {
    match delivery.kind {
        NotificationKind::Committed => Ok(()),
        NotificationKind::Reorged => Err(AppendOnlyViolation::new(
            delivery.tip,
            format!(
                "received a reorg notification at {} ({})",
                delivery.tip.number, delivery.tip.hash
            ),
        )),
        NotificationKind::Reverted => Err(AppendOnlyViolation::new(
            delivery.tip,
            format!(
                "received a revert notification to {} ({})",
                delivery.tip.number, delivery.tip.hash
            ),
        )),
    }
}

/// Drive `work` while retaining only lightweight delivered tips. The checker later walks the
/// canonical provider range itself, so notification payloads never need to be buffered or replayed.
async fn drive_while_draining<F, S, T, E>(
    work: F,
    mut notifications: S,
    progress: &mut RuntimeProgress,
) -> Result<T, DriveError<E>>
where
    F: std::future::Future<Output = Result<T, E>>,
    S: Stream<Item = eyre::Result<DeliveredNotification>> + Unpin,
{
    tokio::pin!(work);
    loop {
        tokio::select! {
            result = &mut work => return result.map_err(DriveError::Work),
            notification = notifications.next() => {
                let delivery = notification
                    .ok_or_else(|| eyre::eyre!("checker notification stream closed while work was pending"))
                    .and_then(|result| result)
                    .map_err(DriveError::Notifications)?;
                progress
                    .accept_delivery(delivery)
                    .map_err(eyre::Report::new)
                    .map_err(DriveError::Notifications)?;
            }
        }
    }
}

#[cfg(test)]
/// Build a delivery for tests in this module and its siblings.
pub(super) fn delivery(number: u64, kind: NotificationKind) -> DeliveredNotification {
    DeliveredNotification {
        tip: BlockNumHash::new(number, B256::repeat_byte(number as u8)),
        kind,
    }
}

#[cfg(test)]
mod tests {
    use futures::{SinkExt as _, channel::mpsc, future, stream};

    use super::*;

    #[tokio::test]
    async fn busy_work_drains_notifications_and_tracks_latest_tip() {
        let (sender_guard, receiver) = mpsc::channel(1);
        let mut sender = sender_guard.clone();
        let (release, released) = futures::channel::oneshot::channel();
        let producer = async move {
            for number in 1..=4 {
                sender
                    .send(Ok(delivery(number, NotificationKind::Committed)))
                    .await
                    .unwrap();
            }
            release.send(()).unwrap();
        };
        let work = async move {
            released.await.unwrap();
            Ok::<_, eyre::Report>(())
        };
        let mut progress = RuntimeProgress::new(BlockNumHash::default());

        let ((), result) = tokio::join!(
            producer,
            drive_while_draining(work, receiver, &mut progress)
        );

        result.unwrap();
        assert!(progress.last_delivered_tip.number >= 3);
    }

    #[tokio::test]
    async fn busy_work_rejects_reorg_as_an_append_only_violation() {
        let notifications = stream::iter([Ok(delivery(7, NotificationKind::Reorged))]);
        let mut progress = RuntimeProgress::new(BlockNumHash::default());
        let error = drive_while_draining(
            future::pending::<Result<(), eyre::Report>>(),
            notifications,
            &mut progress,
        )
        .await
        .expect_err("a Zone reorg must disable the checker");

        assert!(matches!(error, DriveError::Notifications(_)));
        assert_eq!(progress.last_delivered_tip.number, 0);
    }

    #[test]
    fn every_non_commit_notification_violates_append_only_history() {
        for kind in [NotificationKind::Reorged, NotificationKind::Reverted] {
            let error = ensure_append_only(delivery(7, kind))
                .expect_err("a non-commit notification must disable the checker");
            assert!(error.to_string().contains("append-only invariant violated"));
        }
    }
}
