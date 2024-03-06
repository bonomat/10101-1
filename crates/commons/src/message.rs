use crate::order::Order;
use crate::signature::Signature;
use crate::trade::FilledWith;
use crate::LiquidityOption;
use anyhow::Result;
use bitcoin::address::NetworkUnchecked;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Address;
use bitcoin::Amount;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde::Serialize;
use std::fmt::Display;
use tokio_tungstenite_wasm as tungstenite;
use uuid::Uuid;

pub type ChannelId = [u8; 32];
pub type DlcChannelId = [u8; 32];

#[derive(Serialize, Clone, Deserialize, Debug)]
pub enum Message {
    AllOrders(Vec<Order>),
    LimitOrderFilledMatches {
        trader_id: PublicKey,
        matches: Vec<(Uuid, Decimal)>,
    },
    NewOrder(Order),
    DeleteOrder(Uuid),
    Update(Order),
    InvalidAuthentication(String),
    Authenticated(LspConfig),
    Match(FilledWith),
    AsyncMatch {
        order: Order,
        filled_with: FilledWith,
    },
    Rollover(Option<String>),
    /// Message used to collaboratively revert DLC channels.
    DlcChannelCollaborativeRevert {
        channel_id: DlcChannelId,
        coordinator_address: Address<NetworkUnchecked>,
        #[serde(with = "bitcoin::amount::serde::as_sat")]
        coordinator_amount: Amount,
        #[serde(with = "bitcoin::amount::serde::as_sat")]
        trader_amount: Amount,
        #[serde(with = "rust_decimal::serde::float")]
        execution_price: Decimal,
    },
    TradeError {
        order_id: Uuid,
        error: String,
    },
}

#[derive(Serialize, Clone, Deserialize, Debug)]
pub struct LspConfig {
    /// The fee rate to be used for the DLC contracts in sats/vbyte
    pub contract_tx_fee_rate: u64,
    // The liquidity options for onboarding
    pub liquidity_options: Vec<LiquidityOption>,
}

#[derive(Serialize, Clone, Deserialize, Debug)]
pub enum OrderbookRequest {
    Authenticate {
        fcm_token: Option<String>,
        version: Option<String>,
        signature: Signature,
    },
    LimitOrderFilledMatches {
        trader_id: PublicKey,
    },
}

impl TryFrom<OrderbookRequest> for tungstenite::Message {
    type Error = anyhow::Error;

    fn try_from(request: OrderbookRequest) -> Result<Self> {
        let msg = serde_json::to_string(&request)?;
        Ok(tungstenite::Message::Text(msg))
    }
}

impl Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Message::AllOrders(_) => {
                write!(f, "AllOrders")
            }
            Message::LimitOrderFilledMatches { .. } => {
                write!(f, "LimitOrderFilledMatches")
            }
            Message::NewOrder(_) => {
                write!(f, "NewOrder")
            }
            Message::DeleteOrder(_) => {
                write!(f, "DeleteOrder")
            }
            Message::Update(_) => {
                write!(f, "Update")
            }
            Message::InvalidAuthentication(_) => {
                write!(f, "InvalidAuthentication")
            }
            Message::Authenticated(_) => {
                write!(f, "Authenticated")
            }
            Message::Match(_) => {
                write!(f, "Match")
            }
            Message::AsyncMatch { .. } => {
                write!(f, "AsyncMatch")
            }
            Message::Rollover(_) => {
                write!(f, "Rollover")
            }
            Message::DlcChannelCollaborativeRevert { .. } => {
                write!(f, "DlcChannelCollaborativeRevert")
            }
            Message::TradeError { .. } => {
                write!(f, "TradeError")
            }
        }
    }
}
