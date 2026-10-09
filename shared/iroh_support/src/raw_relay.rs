//! Supervised bounded relay for authorized raw workload streams.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, ensure};
use parking_lot::Mutex;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug)]
pub struct RawRelayLimits {
    pub max_bytes_per_direction: u64,
    pub idle_timeout: Duration,
    pub max_lifetime: Duration,
    pub authority_interval: Duration,
}

impl RawRelayLimits {
    pub fn validate(self) -> Result<Self> {
        ensure!(
            self.max_bytes_per_direction > 0,
            "raw relay byte limit is zero"
        );
        ensure!(
            !self.idle_timeout.is_zero(),
            "raw relay idle timeout is zero"
        );
        ensure!(!self.max_lifetime.is_zero(), "raw relay lifetime is zero");
        ensure!(
            !self.authority_interval.is_zero(),
            "raw relay authority interval is zero"
        );
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawRelayStats {
    pub left_to_right_bytes: u64,
    pub right_to_left_bytes: u64,
}

pub async fn supervise_raw_relay<LR, LW, RR, RW, A>(
    left_reader: LR,
    left_writer: LW,
    right_reader: RR,
    right_writer: RW,
    limits: RawRelayLimits,
    cancellation: CancellationToken,
    mut check_authority: A,
) -> Result<RawRelayStats>
where
    LR: AsyncRead + Unpin,
    LW: AsyncWrite + Unpin,
    RR: AsyncRead + Unpin,
    RW: AsyncWrite + Unpin,
    A: FnMut() -> Result<()>,
{
    let limits = limits.validate()?;
    check_authority().context("raw relay authority check")?;
    let child = cancellation.child_token();
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let left_to_right = copy_direction(
        left_reader,
        right_writer,
        limits.max_bytes_per_direction,
        last_activity.clone(),
        child.clone(),
    );
    let right_to_left = copy_direction(
        right_reader,
        left_writer,
        limits.max_bytes_per_direction,
        last_activity.clone(),
        child.clone(),
    );
    tokio::pin!(left_to_right);
    tokio::pin!(right_to_left);
    let mut left_done = false;
    let mut right_done = false;
    let mut left_result = None;
    let mut right_result = None;
    let lifetime = tokio::time::sleep(limits.max_lifetime);
    tokio::pin!(lifetime);
    let check_period = limits.idle_timeout.min(Duration::from_secs(1));
    let mut idle = tokio::time::interval_at(Instant::now() + check_period, check_period);
    idle.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut authority = tokio::time::interval_at(
        Instant::now() + limits.authority_interval,
        limits.authority_interval,
    );
    authority.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let terminal_error = loop {
        if left_done && right_done {
            break None;
        }
        tokio::select! {
            result = &mut left_to_right, if !left_done => {
                left_done = true;
                match result {
                    Ok(bytes) => left_result = Some(bytes),
                    Err(error) => break Some(error.context("left-to-right relay")),
                }
            }
            result = &mut right_to_left, if !right_done => {
                right_done = true;
                match result {
                    Ok(bytes) => right_result = Some(bytes),
                    Err(error) => break Some(error.context("right-to-left relay")),
                }
            }
            _ = cancellation.cancelled() => break Some(anyhow!("raw relay cancelled")),
            _ = &mut lifetime => break Some(anyhow!("raw relay lifetime exceeded")),
            _ = idle.tick() => {
                if Instant::now().saturating_duration_since(*last_activity.lock()) >= limits.idle_timeout {
                    break Some(anyhow!("raw relay idle timeout exceeded"));
                }
            }
            _ = authority.tick() => {
                if let Err(error) = check_authority() {
                    break Some(error.context("raw relay authority check"));
                }
            }
        }
    };

    child.cancel();
    if !left_done {
        left_result = left_to_right.await.ok();
    }
    if !right_done {
        right_result = right_to_left.await.ok();
    }
    if let Some(error) = terminal_error {
        return Err(error);
    }
    Ok(RawRelayStats {
        left_to_right_bytes: left_result.unwrap_or_default(),
        right_to_left_bytes: right_result.unwrap_or_default(),
    })
}

async fn copy_direction<R, W>(
    mut reader: R,
    mut writer: W,
    max_bytes: u64,
    last_activity: Arc<Mutex<Instant>>,
    cancellation: CancellationToken,
) -> Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut total = 0u64;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = tokio::select! {
            _ = cancellation.cancelled() => return Err(anyhow!("raw relay direction cancelled")),
            result = reader.read(&mut buffer) => result.context("read raw relay bytes")?,
        };
        if read == 0 {
            tokio::select! {
                _ = cancellation.cancelled() => return Err(anyhow!("raw relay direction cancelled")),
                result = writer.shutdown() => result.context("shutdown raw relay writer")?,
            }
            return Ok(total);
        }
        let next = total
            .checked_add(u64::try_from(read).context("convert raw relay byte count")?)
            .context("raw relay byte count overflow")?;
        ensure!(next <= max_bytes, "raw relay byte limit exceeded");
        tokio::select! {
            _ = cancellation.cancelled() => return Err(anyhow!("raw relay direction cancelled")),
            result = writer.write_all(&buffer[..read]) => result.context("write raw relay bytes")?,
        }
        total = next;
        *last_activity.lock() = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct PendingShutdown;

    impl AsyncWrite for PendingShutdown {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    #[tokio::test]
    async fn lifetime_cancels_a_writer_stuck_in_shutdown() {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            supervise_raw_relay(
                tokio::io::empty(),
                PendingShutdown,
                tokio::io::empty(),
                PendingShutdown,
                RawRelayLimits {
                    max_lifetime: Duration::from_millis(10),
                    ..limits()
                },
                CancellationToken::new(),
                || Ok(()),
            ),
        )
        .await
        .expect("relay cleanup must remain bounded after EOF");
        assert!(result.unwrap_err().to_string().contains("lifetime"));
    }

    fn limits() -> RawRelayLimits {
        RawRelayLimits {
            max_bytes_per_direction: 1024,
            idle_timeout: Duration::from_secs(1),
            max_lifetime: Duration::from_secs(2),
            authority_interval: Duration::from_millis(10),
        }
    }

    #[tokio::test]
    async fn duplex_bytes_flow_in_both_directions() {
        let (mut left_client, left_relay) = tokio::io::duplex(64);
        let (mut right_client, right_relay) = tokio::io::duplex(64);
        let (left_read, left_write) = tokio::io::split(left_relay);
        let (right_read, right_write) = tokio::io::split(right_relay);
        let relay = tokio::spawn(supervise_raw_relay(
            left_read,
            left_write,
            right_read,
            right_write,
            limits(),
            CancellationToken::new(),
            || Ok(()),
        ));
        left_client.write_all(b"right").await.unwrap();
        let mut right = [0u8; 5];
        right_client.read_exact(&mut right).await.unwrap();
        assert_eq!(&right, b"right");
        right_client.write_all(b"left").await.unwrap();
        let mut left = [0u8; 4];
        left_client.read_exact(&mut left).await.unwrap();
        assert_eq!(&left, b"left");
        left_client.shutdown().await.unwrap();
        right_client.shutdown().await.unwrap();
        let stats = relay.await.unwrap().unwrap();
        assert_eq!(stats.left_to_right_bytes, 5);
        assert_eq!(stats.right_to_left_bytes, 4);
    }

    #[tokio::test]
    async fn authority_failure_terminates_both_directions() {
        let (_left_client, left_relay) = tokio::io::duplex(64);
        let (_right_client, right_relay) = tokio::io::duplex(64);
        let (left_read, left_write) = tokio::io::split(left_relay);
        let (right_read, right_write) = tokio::io::split(right_relay);
        let checks = AtomicUsize::new(0);
        let result = supervise_raw_relay(
            left_read,
            left_write,
            right_read,
            right_write,
            limits(),
            CancellationToken::new(),
            || {
                if checks.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(())
                } else {
                    Err(anyhow!("authority expired"))
                }
            },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("authority"));
    }

    #[tokio::test]
    async fn byte_limit_terminates_the_relay() {
        let (mut left_client, left_relay) = tokio::io::duplex(64);
        let (_right_client, right_relay) = tokio::io::duplex(64);
        let (left_read, left_write) = tokio::io::split(left_relay);
        let (right_read, right_write) = tokio::io::split(right_relay);
        let mut bounded = limits();
        bounded.max_bytes_per_direction = 3;
        let relay = tokio::spawn(supervise_raw_relay(
            left_read,
            left_write,
            right_read,
            right_write,
            bounded,
            CancellationToken::new(),
            || Ok(()),
        ));
        left_client.write_all(b"four").await.unwrap();
        let error = relay.await.unwrap().unwrap_err();
        assert!(format!("{error:#}").contains("byte limit"));
    }

    #[tokio::test]
    async fn idle_timeout_cancels_blocked_directions() {
        let (_left_client, left_relay) = tokio::io::duplex(64);
        let (_right_client, right_relay) = tokio::io::duplex(64);
        let (left_read, left_write) = tokio::io::split(left_relay);
        let (right_read, right_write) = tokio::io::split(right_relay);
        let mut bounded = limits();
        bounded.idle_timeout = Duration::from_millis(10);
        bounded.max_lifetime = Duration::from_secs(1);
        bounded.authority_interval = Duration::from_secs(1);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            supervise_raw_relay(
                left_read,
                left_write,
                right_read,
                right_write,
                bounded,
                CancellationToken::new(),
                || Ok(()),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(result.to_string().contains("idle"));
    }

    #[tokio::test]
    async fn lifetime_timeout_cancels_blocked_directions() {
        let (_left_client, left_relay) = tokio::io::duplex(64);
        let (_right_client, right_relay) = tokio::io::duplex(64);
        let (left_read, left_write) = tokio::io::split(left_relay);
        let (right_read, right_write) = tokio::io::split(right_relay);
        let mut bounded = limits();
        bounded.idle_timeout = Duration::from_secs(1);
        bounded.max_lifetime = Duration::from_millis(10);
        bounded.authority_interval = Duration::from_secs(1);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            supervise_raw_relay(
                left_read,
                left_write,
                right_read,
                right_write,
                bounded,
                CancellationToken::new(),
                || Ok(()),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(result.to_string().contains("lifetime"));
    }

    #[tokio::test]
    async fn external_cancellation_stops_the_relay() {
        let (_left_client, left_relay) = tokio::io::duplex(64);
        let (_right_client, right_relay) = tokio::io::duplex(64);
        let (left_read, left_write) = tokio::io::split(left_relay);
        let (right_read, right_write) = tokio::io::split(right_relay);
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            supervise_raw_relay(
                left_read,
                left_write,
                right_read,
                right_write,
                limits(),
                cancellation,
                || Ok(()),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(format!("{result:#}").contains("cancelled"));
    }
}
