use async_trait::async_trait;
use futures::{
    StreamExt,
    future::{Either, FutureExt, select},
    pin_mut,
};
use hopr_db_api::peers::HoprDbPeersOperations;
#[cfg(all(feature = "prometheus", not(test)))]
use hopr_metrics::{histogram_start_measure, metrics::SimpleHistogram};
use hopr_primitive_types::traits::SaturatingSub;
use libp2p_identity::PeerId;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use serde_with::{DurationSeconds, serde_as};
use tracing::{debug, info};
use validator::Validate;

#[cfg(all(feature = "prometheus", not(test)))]
lazy_static::lazy_static! {
    static ref METRIC_TIME_TO_HEARTBEAT: SimpleHistogram =
        SimpleHistogram::new(
            "hopr_heartbeat_round_time_sec",
            "Measures total time in seconds it takes to probe all other nodes",
            vec![0.5, 1.0, 2.5, 5.0, 10.0, 15.0, 30.0, 60.0],
        ).unwrap();
}

use hopr_platform::time::native::current_time;

use crate::{
    constants::{
        DEFAULT_HEARTBEAT_INTERVAL, DEFAULT_HEARTBEAT_INTERVAL_VARIANCE, DEFAULT_HEARTBEAT_THRESHOLD,
        DEFAULT_MAX_PARALLEL_PINGS,
    },
    network::Network,
    ping::Pinging,
};

/// Configuration for the Heartbeat mechanism
#[serde_as]
#[derive(Debug, Clone, Copy, PartialEq, smart_default::SmartDefault, Validate, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatConfig {
    /// Maximum number of parallel probes performed by the heartbeat mechanism
    #[validate(range(min = 0))]
    #[default(default_max_parallel_pings())]
    #[serde(default = "default_max_parallel_pings")]
    pub max_parallel_probes: usize,

    /// Round-to-round variance to complicate network sync in seconds
    #[serde_as(as = "DurationSeconds<u64>")]
    #[serde(default = "default_heartbeat_variance")]
    #[default(default_heartbeat_variance())]
    pub variance: std::time::Duration,
    /// Interval in which the heartbeat is triggered in seconds
    #[serde_as(as = "DurationSeconds<u64>")]
    #[serde(default = "default_heartbeat_interval")]
    #[default(default_heartbeat_interval())]
    pub interval: std::time::Duration,
    /// The time interval for which to consider peer heartbeat renewal in seconds
    #[serde_as(as = "DurationSeconds<u64>")]
    #[serde(default = "default_heartbeat_threshold")]
    #[default(default_heartbeat_threshold())]
    pub threshold: std::time::Duration,
}

#[inline]
fn default_max_parallel_pings() -> usize {
    DEFAULT_MAX_PARALLEL_PINGS
}

#[inline]
fn default_heartbeat_interval() -> std::time::Duration {
    DEFAULT_HEARTBEAT_INTERVAL
}

#[inline]
fn default_heartbeat_threshold() -> std::time::Duration {
    DEFAULT_HEARTBEAT_THRESHOLD
}

#[inline]
fn default_heartbeat_variance() -> std::time::Duration {
    DEFAULT_HEARTBEAT_INTERVAL_VARIANCE
}

use std::sync::Arc;

use tracing::error;

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait HeartbeatExternalApi {
    /// Get all peers considered by the `Network` to be pingable.
    ///
    /// After a duration of non-pinging based specified by the configurable threshold.
    async fn get_peers(&self, from_timestamp: std::time::SystemTime) -> Vec<PeerId>;
}

/// Implementor of the heartbeat external API.
///
/// Heartbeat requires functionality from external components in order to obtain
/// the triggers for its functionality. This class implements the basic API by
/// aggregating all necessary heartbeat resources without leaking them into the
/// `Heartbeat` object and keeping both the adaptor and the heartbeat object
/// OCP and SRP compliant.
#[derive(Debug, Clone)]
pub struct HeartbeatExternalInteractions<T>
where
    T: HoprDbPeersOperations + Sync + Send + std::fmt::Debug,
{
    network: Arc<Network<T>>,
}

impl<T> HeartbeatExternalInteractions<T>
where
    T: HoprDbPeersOperations + Sync + Send + std::fmt::Debug,
{
    pub fn new(network: Arc<Network<T>>) -> Self {
        Self { network }
    }
}

#[async_trait]
impl<T> HeartbeatExternalApi for HeartbeatExternalInteractions<T>
where
    T: HoprDbPeersOperations + Sync + Send + std::fmt::Debug,
{
    /// Get all peers considered by the `Network` to be pingable.
    ///
    /// After a duration of non-pinging based specified by the configurable threshold.
    async fn get_peers(&self, from_timestamp: std::time::SystemTime) -> Vec<PeerId> {
        self.network
            .find_peers_to_ping(from_timestamp)
            .await
            .unwrap_or_else(|e| {
                error!(error = %e, "Failed to generate peers for the heartbeat procedure");
                vec![]
            })
    }
}

pub type AsyncSleepFn =
    Box<dyn Fn(std::time::Duration) -> std::pin::Pin<Box<dyn futures::Future<Output = ()> + Send>> + Send>;

/// Heartbeat mechanism providing the regular trigger and processing for the heartbeat protocol.
///
/// This object provides a single public method that can be polled. Once triggered, it will never
/// return and will only terminate with an unresolvable error or a panic.
pub struct Heartbeat<T: Pinging, API: HeartbeatExternalApi> {
    config: HeartbeatConfig,
    pinger: T,
    external_api: API,
    sleep_fn: AsyncSleepFn,
}

impl<T: Pinging, API: HeartbeatExternalApi> std::fmt::Debug for Heartbeat<T, API> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Heartbeat").field("config", &self.config).finish()
    }
}

impl<T: Pinging, API: HeartbeatExternalApi> Heartbeat<T, API> {
    pub fn new(config: HeartbeatConfig, pinger: T, external_api: API, sleep_fn: AsyncSleepFn) -> Self {
        Self {
            config,
            pinger,
            external_api,
            sleep_fn,
        }
    }

    #[tracing::instrument(level = "info", skip(self), fields(from_timestamp = tracing::field::debug(current_time())))]
    async fn perform_heartbeat_round(&mut self) {
        let start = current_time();
        let from_timestamp = start.checked_sub(self.config.threshold).unwrap_or(start);

        let mut peers = self.external_api.get_peers(from_timestamp).await;

        // shuffle the peers to make sure that the order is different each heartbeat round
        let mut rng = hopr_crypto_random::rng();
        peers.shuffle(&mut rng);

        #[cfg(all(feature = "prometheus", not(test)))]
        let heartbeat_round_timer = histogram_start_measure!(METRIC_TIME_TO_HEARTBEAT);

        // random timeout to avoid network sync:
        let this_round_planned_duration = std::time::Duration::from_millis({
            let interval_ms = self.config.interval.as_millis() as u64;
            let variance_ms = self.config.variance.as_millis() as u64;

            hopr_crypto_random::random_integer(
                interval_ms,
                Some(interval_ms.checked_add(variance_ms.max(1u64)).unwrap_or(u64::MAX)),
            )
        });

        let peers_contacted = peers.len();
        debug!(peers = tracing::field::debug(&peers), "Heartbeat round start");
        let timeout = (self.sleep_fn)(this_round_planned_duration).fuse();
        let ping_stream = self.pinger.ping(peers);

        pin_mut!(timeout, ping_stream);

        match select(timeout, ping_stream.collect::<Vec<_>>().fuse()).await {
            Either::Left(_) => debug!("Heartbeat round interrupted by timeout"),
            Either::Right((v, _)) => {
                // We intentionally ignore any ping errors here
                let this_round_actual_duration = current_time().saturating_sub(start);
                let time_to_wait_for_next_round =
                    this_round_planned_duration.saturating_sub(this_round_actual_duration);

                let ping_ok = v.iter().filter(|v| v.is_ok()).count();
                info!(
                    round_duration_ms = tracing::field::debug(this_round_actual_duration.as_millis()),
                    time_til_next_round_ms = tracing::field::debug(time_to_wait_for_next_round.as_millis()),
                    peers_contacted,
                    ping_ok,
                    ping_fail = peers_contacted - ping_ok,
                    "Heartbeat round finished"
                );

                (self.sleep_fn)(time_to_wait_for_next_round).await
            }
        };

        #[cfg(all(feature = "prometheus", not(test)))]
        METRIC_TIME_TO_HEARTBEAT.record_measure(heartbeat_round_timer);
    }

    /// Heartbeat loop responsible for periodically requesting peers to ping around from the
    /// external API interface.
    ///
    /// The loop runs indefinitely, until the program is explicitly terminated.
    ///
    /// This feature should be joined with other internal loops and awaited after all
    /// components have been initialized.
    pub async fn heartbeat_loop(&mut self) {
        loop {
            self.perform_heartbeat_round().await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::Stream;
    use tokio::time::sleep;

    use super::*;

    fn simple_heartbeat_config() -> HeartbeatConfig {
        HeartbeatConfig {
            variance: std::time::Duration::from_millis(0u64),
            interval: std::time::Duration::from_millis(5u64),
            threshold: std::time::Duration::from_millis(0u64),
            max_parallel_probes: 14,
        }
    }

    pub struct DelayingPinger {
        pub delay: std::time::Duration,
    }

    impl Pinging for DelayingPinger {
        fn ping(&self, _peers: Vec<PeerId>) -> impl Stream<Item = crate::errors::Result<Duration>> {
            futures::stream::once(async {
                sleep(self.delay).await;
                Ok(Duration::from_millis(1))
            })
        }
    }

    #[tokio::test]
    async fn test_heartbeat_should_loop_multiple_times() {
        let config = simple_heartbeat_config();

        let ping_delay = config.interval / 2;
        let expected_loop_count = 2u32;

        let mut mock = MockHeartbeatExternalApi::new();
        mock.expect_get_peers()
            .times(expected_loop_count as usize..)
            .return_const(vec![PeerId::random(), PeerId::random()]);

        let mut heartbeat = Heartbeat::new(
            config,
            DelayingPinger { delay: ping_delay },
            mock,
            Box::new(|dur| Box::pin(sleep(dur))),
        );

        futures::select!(
            _ = heartbeat.heartbeat_loop().fuse() => {},
            _ = sleep(config.interval * expected_loop_count).fuse() => {},
        );
    }

    #[tokio::test]
    async fn test_heartbeat_should_interrupt_long_running_heartbeats() {
        let config = HeartbeatConfig {
            interval: std::time::Duration::from_millis(5u64),
            ..simple_heartbeat_config()
        };

        let ping_delay = 2 * config.interval + config.interval / 2;
        let expected_loop_count = 2;

        let mut mock = MockHeartbeatExternalApi::new();
        mock.expect_get_peers()
            .times(expected_loop_count..)
            .return_const(vec![PeerId::random(), PeerId::random()]);

        let mut heartbeat = Heartbeat::new(
            config,
            DelayingPinger { delay: ping_delay },
            mock,
            Box::new(|dur| Box::pin(sleep(dur))),
        );

        let tolerance = std::time::Duration::from_millis(2);
        futures::select!(
            _ = heartbeat.heartbeat_loop().fuse() => {},
            _ = sleep(config.interval * (expected_loop_count as u32) + tolerance).fuse() => {},
        );
    }
}
