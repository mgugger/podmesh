use super::*;
use std::task::Poll;

/// Poll the actual capacity wait rather than timing an entire podctl apply.
/// Apply also performs crypto, deployment, persistence, and DNS peer bootstrap;
/// its wall-clock duration cannot tell us whether selection slept to expiry.
#[tokio::test]
async fn capacity_wait_returns_as_soon_as_enough_offers_arrive() {
    let (offers, receiver) = tokio::sync::watch::channel(0usize);
    let cancellation = CancellationToken::new();
    let wait = wait_for_offers(receiver, 3, Duration::from_secs(3600), &cancellation);
    tokio::pin!(wait);

    assert!(futures::poll!(wait.as_mut()).is_pending());
    for count in [1, 2] {
        offers.send(count).unwrap();
        assert!(futures::poll!(wait.as_mut()).is_pending());
    }
    offers.send(3).unwrap();
    assert!(matches!(futures::poll!(wait.as_mut()), Poll::Ready(Ok(()))));
}

#[tokio::test]
async fn capacity_wait_checks_offers_that_arrived_before_it_started() {
    let (_offers, receiver) = tokio::sync::watch::channel(3usize);
    let cancellation = CancellationToken::new();
    let wait = wait_for_offers(receiver, 3, Duration::from_secs(3600), &cancellation);
    tokio::pin!(wait);

    assert!(matches!(futures::poll!(wait.as_mut()), Poll::Ready(Ok(()))));
}

#[tokio::test]
async fn capacity_wait_is_cancelled_during_shutdown() {
    let (_offers, receiver) = tokio::sync::watch::channel(0usize);
    let cancellation = CancellationToken::new();
    let wait = wait_for_offers(receiver, 3, Duration::from_secs(3600), &cancellation);
    tokio::pin!(wait);

    assert!(futures::poll!(wait.as_mut()).is_pending());
    cancellation.cancel();
    assert!(matches!(futures::poll!(wait.as_mut()), Poll::Ready(Err(_))));
}
