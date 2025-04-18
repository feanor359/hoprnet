use async_lock::RwLock;
use futures::channel::mpsc::Sender;
use std::sync::{Arc, OnceLock};
use tracing::trace;

use hopr_chain_types::chain_events::NetworkRegistryStatus;
use hopr_crypto_types::types::OffchainPublicKey;
use hopr_db_sql::HoprDbAllOperations;
use hopr_internal_types::protocol::ApplicationData;
use hopr_network_types::prelude::RoutingOptions;
use hopr_path::{path::TransportPath, selectors::PathSelector};
use hopr_primitive_types::primitives::Address;
use hopr_transport_identity::PeerId;
use hopr_transport_protocol::msg::processor::{MsgSender, SendMsgInput};
use hopr_transport_session::{
    errors::{SessionManagerError, TransportSessionError},
    traits::SendMsg,
};

#[cfg(all(feature = "prometheus", not(test)))]
lazy_static::lazy_static! {
    static ref METRIC_PATH_LENGTH: hopr_metrics::metrics::SimpleHistogram = hopr_metrics::metrics::SimpleHistogram::new(
        "hopr_path_length",
        "Distribution of number of hops of sent messages",
        vec![0.0, 1.0, 2.0, 3.0, 4.0]
    ).unwrap();
}

use crate::{constants::RESERVED_SESSION_TAG_UPPER_LIMIT, errors::HoprTransportError};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PeerEligibility {
    Eligible,
    Ineligible,
}

impl From<NetworkRegistryStatus> for PeerEligibility {
    fn from(value: NetworkRegistryStatus) -> Self {
        match value {
            NetworkRegistryStatus::Allowed => Self::Eligible,
            NetworkRegistryStatus::Denied => Self::Ineligible,
        }
    }
}

/// Ticket statistics data exposed by the ticket mechanism.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct TicketStatistics {
    pub winning_count: u128,
    pub unredeemed_value: hopr_primitive_types::primitives::Balance,
    pub redeemed_value: hopr_primitive_types::primitives::Balance,
    pub neglected_value: hopr_primitive_types::primitives::Balance,
    pub rejected_value: hopr_primitive_types::primitives::Balance,
}

#[derive(Clone)]
pub(crate) struct PathPlanner<T, S> {
    db: T,
    channel_graph: Arc<RwLock<hopr_path::channel_graph::ChannelGraph>>,
    selector: S,
    me: Address,
}

impl<T, S> PathPlanner<T, S>
where
    T: HoprDbAllOperations + std::fmt::Debug + Send + Sync + 'static,
    S: PathSelector + Send + Sync,
{
    pub(crate) fn new(
        db: T,
        selector: S,
        channel_graph: Arc<RwLock<hopr_path::channel_graph::ChannelGraph>>,
        me: Address,
    ) -> Self {
        Self {
            db,
            channel_graph,
            me,
            selector,
        }
    }

    pub(crate) fn channel_graph(&self) -> Arc<RwLock<hopr_path::channel_graph::ChannelGraph>> {
        self.channel_graph.clone()
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub(crate) async fn resolve_path(
        &self,
        destination: PeerId,
        options: RoutingOptions,
    ) -> crate::errors::Result<TransportPath> {
        let path = match options {
            RoutingOptions::IntermediatePath(path) => {
                let complete_path = Vec::from_iter(path.into_iter().chain([destination]));
                trace!(full_path = ?complete_path, "resolved a specific path");

                let cg = self.channel_graph.read().await;

                TransportPath::resolve(complete_path, &self.db, &cg)
                    .await
                    .map(|(p, _)| p)?
            }
            RoutingOptions::Hops(hops) if u32::from(hops) == 0 => {
                trace!(hops = 0, %destination, "resolved zero-hop path");
                TransportPath::direct(destination)
            }
            RoutingOptions::Hops(hops) => {
                trace!(%hops, "resolved path using hop count");

                let pk = OffchainPublicKey::try_from(destination)?;

                if let Some(chain_key) = self
                    .db
                    .translate_key(None, pk)
                    .await
                    .map_err(hopr_db_sql::api::errors::DbError::from)?
                {
                    let target_chain_key: Address = chain_key.try_into()?;
                    let cp = self
                        .selector
                        .select_path(self.me, target_chain_key, hops.into(), hops.into())
                        .await?;

                    let full_path = cp.into_path(&self.db, target_chain_key).await?;
                    trace!(%full_path, "resolved automatic path");

                    full_path
                } else {
                    return Err(HoprTransportError::Api(
                        "send msg: unknown destination peer id encountered".to_owned(),
                    ));
                }
            }
        };

        #[cfg(all(feature = "prometheus", not(test)))]
        {
            use hopr_path::path::Path;
            hopr_metrics::SimpleHistogram::observe(&METRIC_PATH_LENGTH, (path.hops().len() - 1) as f64);
        }

        Ok(path)
    }
}

#[derive(Clone)]
pub(crate) struct MessageSender<T, S> {
    pub process_packet_send: Arc<OnceLock<MsgSender<Sender<SendMsgInput>>>>,
    pub resolver: PathPlanner<T, S>,
}

impl<T, S> MessageSender<T, S>
where
    T: HoprDbAllOperations + std::fmt::Debug + Send + Sync + 'static,
    S: PathSelector + Send + Sync,
{
    pub fn new(
        process_packet_send: Arc<OnceLock<MsgSender<Sender<SendMsgInput>>>>,
        resolver: PathPlanner<T, S>,
    ) -> Self {
        Self {
            process_packet_send,
            resolver,
        }
    }
}

#[async_trait::async_trait]
impl<T, S> SendMsg for MessageSender<T, S>
where
    T: HoprDbAllOperations + std::fmt::Debug + Send + Sync + 'static,
    S: PathSelector + Send + Sync,
{
    #[tracing::instrument(level = "debug", skip(self, data))]
    async fn send_message(
        &self,
        data: ApplicationData,
        destination: PeerId,
        options: RoutingOptions,
    ) -> std::result::Result<(), TransportSessionError> {
        data.application_tag
            .is_some_and(|application_tag| application_tag < RESERVED_SESSION_TAG_UPPER_LIMIT)
            .then_some(())
            .ok_or(TransportSessionError::Tag)?;

        let path = self
            .resolver
            .resolve_path(destination, options)
            .await
            .map_err(|_| TransportSessionError::Path)?;

        self.process_packet_send
            .get()
            .ok_or_else(|| SessionManagerError::NotStarted)?
            .send_packet(data, path)
            .await
            .map_err(|_| TransportSessionError::Closed)?
            .consume_and_wait(crate::constants::PACKET_QUEUE_TIMEOUT_MILLISECONDS)
            .await
            .map_err(|_e| TransportSessionError::Timeout)?;

        trace!("Packet sent to the outgoing queue");

        Ok(())
    }
}
