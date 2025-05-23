//! ## Auto Funding Strategy
//! This strategy listens for channel state change events to check whether a channel has dropped below `min_stake_threshold` HOPR.
//! If this happens, the strategy issues a **fund channel** transaction to re-stake the channel with `funding_amount` HOPR.
//!
//! For details on default parameters see [AutoFundingStrategyConfig].
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::fmt::{Debug, Display, Formatter};
use tracing::info;
use validator::Validate;

use hopr_chain_actions::channels::ChannelActions;
use hopr_internal_types::prelude::*;
use hopr_primitive_types::prelude::*;

use crate::errors::StrategyError::CriteriaNotSatisfied;
use crate::strategy::SingularStrategy;
use crate::Strategy;

#[cfg(all(feature = "prometheus", not(test)))]
use hopr_metrics::metrics::SimpleCounter;

#[cfg(all(feature = "prometheus", not(test)))]
lazy_static::lazy_static! {
    static ref METRIC_COUNT_AUTO_FUNDINGS: SimpleCounter =
        SimpleCounter::new("hopr_strategy_auto_funding_funding_count", "Count of initiated automatic fundings").unwrap();
}

/// Configuration for `AutoFundingStrategy`
#[serde_as]
#[derive(Debug, Clone, Copy, PartialEq, Eq, smart_default::SmartDefault, Validate, Serialize, Deserialize)]
pub struct AutoFundingStrategyConfig {
    /// Minimum stake that a channel's balance must not go below.
    ///
    /// Default is 1 wxHOPR
    #[serde_as(as = "DisplayFromStr")]
    #[default(Balance::new_from_str("1000000000000000000", BalanceType::HOPR))]
    pub min_stake_threshold: Balance,

    /// Funding amount.
    ///
    /// Defaults to 10 wxHOPR.
    #[serde_as(as = "DisplayFromStr")]
    #[default(Balance::new_from_str("10000000000000000000", BalanceType::HOPR))]
    pub funding_amount: Balance,
}

/// The `AutoFundingStrategy` automatically funds channel that
/// dropped it's staked balance below the configured threshold.
pub struct AutoFundingStrategy<A: ChannelActions> {
    hopr_chain_actions: A,
    cfg: AutoFundingStrategyConfig,
}

impl<A: ChannelActions> AutoFundingStrategy<A> {
    pub fn new(cfg: AutoFundingStrategyConfig, hopr_chain_actions: A) -> Self {
        Self {
            cfg,
            hopr_chain_actions,
        }
    }
}

impl<A: ChannelActions> Debug for AutoFundingStrategy<A> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", Strategy::AutoFunding(self.cfg))
    }
}

impl<A: ChannelActions> Display for AutoFundingStrategy<A> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", Strategy::AutoFunding(self.cfg))
    }
}

#[async_trait]
impl<A: ChannelActions + Send + Sync> SingularStrategy for AutoFundingStrategy<A> {
    async fn on_own_channel_changed(
        &self,
        channel: &ChannelEntry,
        direction: ChannelDirection,
        change: ChannelChange,
    ) -> crate::errors::Result<()> {
        // Can only auto-fund outgoing channels
        if direction != ChannelDirection::Outgoing {
            return Ok(());
        }

        if let ChannelChange::CurrentBalance { right: new, .. } = change {
            if new.lt(&self.cfg.min_stake_threshold) && channel.status == ChannelStatus::Open {
                info!(%channel, balance = %channel.balance, threshold = %self.cfg.min_stake_threshold,
                    "stake on channel is below threshold",
                );

                #[cfg(all(feature = "prometheus", not(test)))]
                METRIC_COUNT_AUTO_FUNDINGS.increment();

                let rx = self
                    .hopr_chain_actions
                    .fund_channel(channel.get_id(), self.cfg.funding_amount)
                    .await?;
                std::mem::drop(rx); // The Receiver is not intentionally awaited here and the oneshot Sender can fail safely
                info!(%channel, amount = %self.cfg.funding_amount, "issued re-staking of channel", );
            }
            Ok(())
        } else {
            Err(CriteriaNotSatisfied)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::auto_funding::{AutoFundingStrategy, AutoFundingStrategyConfig};
    use crate::strategy::SingularStrategy;
    use async_trait::async_trait;
    use futures::{future::ok, FutureExt};
    use hex_literal::hex;
    use hopr_chain_actions::action_queue::{ActionConfirmation, PendingAction};
    use hopr_chain_actions::channels::ChannelActions;
    use hopr_chain_types::actions::Action;
    use hopr_chain_types::chain_events::ChainEventType;
    use hopr_crypto_random::random_bytes;
    use hopr_crypto_types::types::Hash;
    use hopr_internal_types::prelude::*;
    use hopr_primitive_types::prelude::*;
    use mockall::mock;

    lazy_static::lazy_static! {
        static ref ALICE: Address = hex!("18f8ae833c85c51fbeba29cef9fbfb53b3bad950").into();
        static ref BOB: Address = hex!("44f23fa14130ca540b37251309700b6c281d972e").into();
        static ref CHRIS: Address = hex!("b6021e0860dd9d96c9ff0a73e2e5ba3a466ba234").into();
        static ref DAVE: Address = hex!("68499f50ff68d523385dc60686069935d17d762a").into();
    }

    mock! {
        ChannelAct { }
        #[async_trait]
        impl ChannelActions for ChannelAct {
            async fn open_channel(&self, destination: Address, amount: Balance) -> hopr_chain_actions::errors::Result<PendingAction>;
            async fn fund_channel(&self, channel_id: Hash, amount: Balance) -> hopr_chain_actions::errors::Result<PendingAction>;
            async fn close_channel(
                &self,
                counterparty: Address,
                direction: ChannelDirection,
                redeem_before_close: bool,
            ) -> hopr_chain_actions::errors::Result<PendingAction>;
        }
    }

    fn mock_action_confirmation(channel: ChannelEntry, balance: Balance) -> ActionConfirmation {
        let random_hash = Hash::from(random_bytes::<{ Hash::SIZE }>());
        ActionConfirmation {
            tx_hash: random_hash,
            event: Some(ChainEventType::ChannelBalanceIncreased(channel, balance)),
            action: Action::FundChannel(channel, balance),
        }
    }

    #[async_std::test]
    async fn test_auto_funding_strategy() -> anyhow::Result<()> {
        let stake_limit = Balance::new(7_u32, BalanceType::HOPR);
        let fund_amount = Balance::new(5_u32, BalanceType::HOPR);

        let c1 = ChannelEntry::new(
            *ALICE,
            *BOB,
            Balance::new(10_u32, BalanceType::HOPR),
            0_u32.into(),
            ChannelStatus::Open,
            0_u32.into(),
        );

        let c2 = ChannelEntry::new(
            *BOB,
            *CHRIS,
            Balance::new(5_u32, BalanceType::HOPR),
            0_u32.into(),
            ChannelStatus::Open,
            0_u32.into(),
        );

        let c3 = ChannelEntry::new(
            *CHRIS,
            *DAVE,
            Balance::new(5_u32, BalanceType::HOPR),
            0_u32.into(),
            ChannelStatus::PendingToClose(std::time::SystemTime::now()),
            0_u32.into(),
        );

        let mut actions = MockChannelAct::new();
        let fund_amount_c = fund_amount.clone();
        actions
            .expect_fund_channel()
            .times(1)
            .withf(move |h, balance| c2.get_id().eq(h) && fund_amount_c.eq(&balance))
            .return_once(move |_, _| Ok(ok(mock_action_confirmation(c2, fund_amount)).boxed()));

        let cfg = AutoFundingStrategyConfig {
            min_stake_threshold: stake_limit,
            funding_amount: fund_amount,
        };

        let afs = AutoFundingStrategy::new(cfg, actions);
        afs.on_own_channel_changed(
            &c1,
            ChannelDirection::Outgoing,
            ChannelChange::CurrentBalance {
                left: Balance::zero(BalanceType::HOPR),
                right: c1.balance,
            },
        )
        .await?;

        afs.on_own_channel_changed(
            &c2,
            ChannelDirection::Outgoing,
            ChannelChange::CurrentBalance {
                left: Balance::zero(BalanceType::HOPR),
                right: c2.balance,
            },
        )
        .await?;

        afs.on_own_channel_changed(
            &c3,
            ChannelDirection::Outgoing,
            ChannelChange::CurrentBalance {
                left: Balance::zero(BalanceType::HOPR),
                right: c3.balance,
            },
        )
        .await?;

        Ok(())
    }
}
