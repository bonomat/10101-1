use crate::channel::UserChannelId;
use crate::dlc_custom_signer::CustomKeysManager;
use crate::fee_rate_estimator::FeeRateEstimator;
use crate::ln::manage_spendable_outputs;
use crate::ln::GossipSource;
use crate::ln::Probes;
use crate::ln::TracingLogger;
use crate::ln_dlc_wallet::LnDlcWallet;
use crate::node::peer_manager::alias_as_bytes;
use crate::node::peer_manager::broadcast_node_announcement;
use crate::node::sub_channel::sub_channel_manager_periodic_check;
use crate::on_chain_wallet::OnChainWallet;
use crate::seed::Bip39Seed;
use crate::shadow::Shadow;
use crate::storage::TenTenOneStorage;
use crate::ChainMonitor;
use crate::EventHandlerTrait;
use crate::NetworkGraph;
use crate::P2pGossipSync;
use crate::PeerManager;
use crate::RapidGossipSync;
use crate::WalletSettings;
use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use bdk::FeeRate;
use bitcoin::hashes::hex::ToHex;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Amount;
use bitcoin::Network;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use dlc_messages::message_handler::MessageHandler as DlcMessageHandler;
use futures::future::RemoteHandle;
use futures::FutureExt;
use lightning::chain::chaininterface::ConfirmationTarget;
use lightning::chain::chainmonitor;
use lightning::chain::Confirm;
use lightning::ln::msgs::RoutingMessageHandler;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::ln::peer_handler::MessageHandler;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::ProbabilisticScorer;
use lightning::routing::scoring::ProbabilisticScoringFeeParameters;
use lightning::routing::utxo::UtxoLookup;
use lightning::sign::EntropySource;
use lightning::sign::KeysManager;
use lightning::util::config::UserConfig;
use lightning_background_processor::process_events_async;
use lightning_transaction_sync::EsploraSyncClient;
use ln_dlc_storage::DlcStorageProvider;
use p2pd_oracle_client::P2PDOracleClient;
use serde::Deserialize;
use serde::Serialize;
use serde_with::serde_as;
use serde_with::DurationSeconds;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use tokio::sync::RwLock;
use tokio::task::spawn_blocking;

mod channel_manager;
mod connection;
mod dlc_manager;
mod ln_channel;
mod oracle;
mod storage;
mod sub_channel_manager;
mod wallet;

pub(crate) mod invoice;
pub(crate) mod sub_channel;

pub mod dlc_channel;
pub mod event;
pub mod peer_manager;

pub use crate::node::connection::TenTenOneOnionMessageHandler;
pub use crate::node::dlc_manager::signed_channel_state_name;
pub use crate::node::dlc_manager::DlcManager;
use crate::node::event::NodeEventHandler;
pub use crate::node::oracle::OracleInfo;
pub use ::dlc_manager as rust_dlc_manager;
pub use channel_manager::ChannelManager;
pub use invoice::HTLCStatus;
use lightning::ln::msgs::SocketAddress;
use lightning::util::persist::KVStore;
use lightning::util::persist::NETWORK_GRAPH_PERSISTENCE_KEY;
use lightning::util::persist::NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE;
use lightning::util::persist::NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE;
use lightning::util::ser::ReadableArgs;
pub use storage::InMemoryStore;
pub use storage::Storage;
pub use sub_channel::dlc_message_name;
pub use sub_channel::send_sub_channel_message;
pub use sub_channel::sub_channel_message_name;
pub use sub_channel_manager::SubChannelManager;
pub use wallet::PaymentDetails;

/// The interval at which the [`lightning::ln::msgs::NodeAnnouncement`] is broadcast.
///
/// According to the LDK team, a value of up to 1 hour should be fine.
const BROADCAST_NODE_ANNOUNCEMENT_INTERVAL: Duration = Duration::from_secs(3600);

/// The interval at which spendable outputs generated by LDK are considered for spending.
const MANAGE_SPENDABLE_OUTPUTS_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// The time in between RGS sync attempts.
///
/// Value taken from `ldk-node` project.
const RGS_SYNC_INTERVAL: Duration = Duration::from_secs(60 * 60);

type Scorer = ProbabilisticScorer<Arc<NetworkGraph>, Arc<TracingLogger>>;

type NodeEsploraClient = EsploraSyncClient<Arc<TracingLogger>>;

type RequestedScid = u64;
// TODO(holzeis): Move to coordinator
type FakeChannelPaymentRequests = Arc<parking_lot::Mutex<HashMap<RequestedScid, LiquidityRequest>>>;

#[derive(Clone, Debug)]
pub struct LiquidityRequest {
    pub user_channel_id: UserChannelId,
    pub liquidity_option_id: i32,
    pub trader_id: PublicKey,
    pub trade_up_to_sats: u64,
    pub max_deposit_sats: u64,
    pub coordinator_leverage: f32,
    pub fee_sats: u64,
}

/// An LN-DLC node.
pub struct Node<S: TenTenOneStorage, N: Storage> {
    pub settings: Arc<RwLock<LnDlcNodeSettings>>,
    pub network: Network,

    pub(crate) wallet: Arc<LnDlcWallet<S, N>>,

    pub peer_manager: Arc<PeerManager<S, N>>,
    pub channel_manager: Arc<ChannelManager<S, N>>,
    pub chain_monitor: Arc<ChainMonitor<S, N>>,
    pub keys_manager: Arc<CustomKeysManager<S, N>>,
    pub network_graph: Arc<NetworkGraph>,
    pub fee_rate_estimator: Arc<FeeRateEstimator>,

    pub logger: Arc<TracingLogger>,

    pub info: NodeInfo,
    pub(crate) fake_channel_payments: FakeChannelPaymentRequests,

    pub dlc_manager: Arc<DlcManager<S, N>>,
    pub sub_channel_manager: Arc<SubChannelManager<S, N>>,

    /// All oracles clients the node is aware of.
    oracles: Vec<Arc<P2PDOracleClient>>,
    pub dlc_message_handler: Arc<DlcMessageHandler>,
    pub ldk_config: Arc<parking_lot::RwLock<UserConfig>>,

    /// The oracle pubkey used for proposing dlc channels
    pub oracle_pubkey: XOnlyPublicKey,

    pub event_handler: Arc<NodeEventHandler>,

    // storage
    // TODO(holzeis): The node storage should get extracted to the corresponding application
    // layers.
    pub node_storage: Arc<N>,
    pub ln_storage: Arc<S>,
    pub dlc_storage: Arc<DlcStorageProvider<S>>,

    // fields below are needed only to start the node
    listen_address: SocketAddr,
    gossip_source: Arc<GossipSource>,
    pub(crate) alias: String,
    pub(crate) announcement_addresses: Vec<SocketAddress>,
    pub scorer: Arc<std::sync::RwLock<Scorer>>,
    esplora_server_url: String,
    esplora_client: Arc<NodeEsploraClient>,
    pub pending_channel_opening_fee_rates: Arc<parking_lot::Mutex<HashMap<PublicKey, FeeRate>>>,
    pub probes: Probes,
}

/// An on-chain network fee for a transaction
pub enum Fee {
    /// A fee given by the transaction's priority
    Priority(ConfirmationTarget),
    /// A fix defined sats/vbyte
    FeeRate(FeeRate),
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct NodeInfo {
    pub pubkey: PublicKey,
    pub address: SocketAddr,
}

/// Node is running until this struct is dropped
pub struct RunningNode {
    _handles: Vec<RemoteHandle<()>>,
}

#[serde_as]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct LnDlcNodeSettings {
    /// How often we sync the LDK wallet
    #[serde_as(as = "DurationSeconds")]
    pub off_chain_sync_interval: Duration,
    /// How often we sync the BDK wallet
    #[serde_as(as = "DurationSeconds")]
    pub on_chain_sync_interval: Duration,
    /// How often we update the fee rate
    #[serde_as(as = "DurationSeconds")]
    pub fee_rate_sync_interval: Duration,
    /// How often we run the [`DlcManager`]'s periodic check.
    #[serde_as(as = "DurationSeconds")]
    pub dlc_manager_periodic_check_interval: Duration,
    /// How often we run the [`SubChannelManager`]'s periodic check.
    #[serde_as(as = "DurationSeconds")]
    pub sub_channel_manager_periodic_check_interval: Duration,
    /// How often we sync the shadow states
    #[serde_as(as = "DurationSeconds")]
    pub shadow_sync_interval: Duration,

    /// Amount (in millionths of a satoshi) charged per satoshi for payments forwarded outbound
    /// over a channel.
    pub forwarding_fee_proportional_millionths: u32,

    /// The 'stop gap' parameter used by BDK's wallet sync. This seems to configure the threshold
    /// number of blocks after which BDK stops looking for scripts belonging to the wallet.
    /// Note: This constant and value was copied from ldk_node
    /// XXX: Requires restart of the node to take effect
    pub bdk_client_stop_gap: usize,
    /// The number of concurrent requests made against the API provider.
    /// Note: This constant and value was copied from ldk_node
    /// XXX: Requires restart of the node to take effect
    pub bdk_client_concurrency: u8,

    /// XXX: Requires restart of the node to take effect
    pub gossip_source_config: GossipSourceConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub enum GossipSourceConfig {
    P2pNetwork,
    RapidGossipSync { server_url: String },
}

impl<S: TenTenOneStorage + 'static, N: Storage + Sync + Send + 'static> Node<S, N> {
    pub async fn update_settings(&self, new_settings: LnDlcNodeSettings) {
        tracing::info!(?new_settings, "Updating LnDlcNode settings");
        *self.settings.write().await = new_settings;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new<SC>(
        // Supplied configuration of LDK node.
        ldk_config: UserConfig,
        read_scorer: SC,
        alias: &str,
        network: Network,
        data_dir: &Path,
        storage: S,
        node_storage: Arc<N>,
        announcement_address: SocketAddr,
        listen_address: SocketAddr,
        announcement_addresses: Vec<SocketAddress>,
        esplora_server_url: String,
        seed: Bip39Seed,
        ephemeral_randomness: [u8; 32],
        settings: LnDlcNodeSettings,
        wallet_settings: WalletSettings,
        oracle_clients: Vec<P2PDOracleClient>,
        oracle_pubkey: XOnlyPublicKey,
        node_event_handler: Arc<NodeEventHandler>,
    ) -> Result<Self>
    where
        SC: Fn(&Path, Arc<NetworkGraph>, Arc<TracingLogger>) -> Scorer,
    {
        let time_since_unix_epoch = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;

        let logger = Arc::new(TracingLogger {
            alias: alias.to_string(),
        });

        let ldk_config = Arc::new(parking_lot::RwLock::new(ldk_config));

        let on_chain_dir = data_dir.join("on_chain");
        let on_chain_wallet =
            OnChainWallet::new(on_chain_dir.as_path(), network, seed.wallet_seed())?;

        let esplora_client = Arc::new(EsploraSyncClient::new(
            esplora_server_url.clone(),
            logger.clone(),
        ));

        let dlc_storage = Arc::new(DlcStorageProvider::new(storage.clone()));
        let ln_storage = Arc::new(storage);

        let fee_rate_estimator = Arc::new(FeeRateEstimator::new(network));
        let ln_dlc_wallet = {
            Arc::new(LnDlcWallet::new(
                esplora_client.clone(),
                on_chain_wallet.inner,
                fee_rate_estimator.clone(),
                dlc_storage.clone(),
                node_storage.clone(),
                settings.bdk_client_stop_gap,
                settings.bdk_client_concurrency,
                wallet_settings,
            ))
        };

        let chain_monitor: Arc<ChainMonitor<S, N>> = Arc::new(chainmonitor::ChainMonitor::new(
            Some(esplora_client.clone()),
            ln_dlc_wallet.clone(),
            logger.clone(),
            fee_rate_estimator.clone(),
            ln_storage.clone(),
        ));

        let keys_manager = {
            Arc::new(CustomKeysManager::new(
                KeysManager::new(
                    &seed.lightning_seed(),
                    time_since_unix_epoch.as_secs(),
                    time_since_unix_epoch.subsec_nanos(),
                ),
                ln_dlc_wallet.clone(),
            ))
        };

        let network_graph = match KVStore::read(
            ln_storage.as_ref(),
            NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
            NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
            NETWORK_GRAPH_PERSISTENCE_KEY,
        ) {
            Ok(network_graph) => {
                let network_graph = match NetworkGraph::read(
                    &mut BufReader::new(network_graph.as_slice()),
                    logger.clone(),
                ) {
                    Ok(network_graph) => network_graph,
                    Err(e) => {
                        tracing::warn!("Failed to read network graph. {e:#}");
                        NetworkGraph::new(network, logger.clone())
                    }
                };
                Arc::new(network_graph)
            }
            Err(e) => {
                tracing::info!("Couldn't find network graph. {e:#}");
                tracing::info!("Creating new network graph");
                Arc::new(NetworkGraph::new(network, logger.clone()))
            }
        };

        let scorer_path = data_dir.join("scorer");
        let scorer = Arc::new(std::sync::RwLock::new(read_scorer(
            scorer_path.as_path(),
            network_graph.clone(),
            logger.clone(),
        )));

        let scoring_fee_params = ProbabilisticScoringFeeParameters::default();
        let router = Arc::new(DefaultRouter::new(
            network_graph.clone(),
            logger.clone(),
            keys_manager.get_secure_random_bytes(),
            scorer.clone(),
            scoring_fee_params,
        ));

        let channel_manager = channel_manager::build(
            keys_manager.clone(),
            ln_dlc_wallet.clone(),
            fee_rate_estimator.clone(),
            esplora_client.clone(),
            logger.clone(),
            chain_monitor.clone(),
            *ldk_config.read(),
            network,
            ln_storage.clone(),
            router,
        )?;

        let channel_manager = Arc::new(channel_manager);

        let gossip_source = match &settings.gossip_source_config {
            GossipSourceConfig::P2pNetwork => {
                let gossip_sync = Arc::new(P2pGossipSync::new(
                    network_graph.clone(),
                    None::<Arc<dyn UtxoLookup + Send + Sync>>,
                    logger.clone(),
                ));

                GossipSource::P2pNetwork { gossip_sync }
            }
            GossipSourceConfig::RapidGossipSync { server_url } => {
                let gossip_sync =
                    Arc::new(RapidGossipSync::new(network_graph.clone(), logger.clone()));

                GossipSource::RapidGossipSync {
                    gossip_sync,
                    server_url: server_url.clone(),
                }
            }
        };

        let oracle_clients: Vec<Arc<P2PDOracleClient>> =
            oracle_clients.into_iter().map(Arc::new).collect();

        let dlc_manager = dlc_manager::build(
            data_dir,
            ln_dlc_wallet.clone(),
            dlc_storage.clone(),
            oracle_clients.clone(),
            fee_rate_estimator.clone(),
        )?;
        let dlc_manager = Arc::new(dlc_manager);

        let sub_channel_manager = sub_channel_manager::build(
            channel_manager.clone(),
            dlc_manager.clone(),
            chain_monitor.clone(),
            keys_manager.clone(),
        )?;

        let dlc_message_handler = Arc::new(DlcMessageHandler::new());

        let route_handler = match &gossip_source {
            GossipSource::P2pNetwork { gossip_sync } => {
                gossip_sync.clone() as Arc<dyn RoutingMessageHandler + Sync + Send>
            }
            GossipSource::RapidGossipSync { .. } => {
                Arc::new(IgnoringMessageHandler {}) as Arc<dyn RoutingMessageHandler + Sync + Send>
            }
        };

        let onion_message_handler = Arc::new(TenTenOneOnionMessageHandler::new(
            node_event_handler.clone(),
        ));

        let lightning_msg_handler = MessageHandler {
            chan_handler: sub_channel_manager.clone(),
            route_handler,
            onion_message_handler,
            custom_message_handler: dlc_message_handler.clone(),
        };

        let peer_manager: Arc<PeerManager<S, N>> = Arc::new(PeerManager::new(
            lightning_msg_handler,
            time_since_unix_epoch.as_secs() as u32,
            &ephemeral_randomness,
            logger.clone(),
            keys_manager.clone(),
        ));

        let fake_channel_payments: FakeChannelPaymentRequests =
            Arc::new(parking_lot::Mutex::new(HashMap::new()));

        let node_info = NodeInfo {
            pubkey: channel_manager.get_our_node_id(),
            address: announcement_address,
        };

        let gossip_source = Arc::new(gossip_source);

        let settings = Arc::new(RwLock::new(settings));

        Ok(Self {
            network,
            wallet: ln_dlc_wallet,
            peer_manager,
            keys_manager,
            chain_monitor,
            logger,
            channel_manager: channel_manager.clone(),
            info: node_info,
            fake_channel_payments,
            sub_channel_manager,
            oracles: oracle_clients,
            dlc_message_handler,
            dlc_manager,
            ln_storage,
            dlc_storage,
            node_storage,
            fee_rate_estimator,
            ldk_config,
            network_graph,
            settings,
            listen_address,
            gossip_source,
            alias: alias.to_string(),
            announcement_addresses,
            scorer,
            esplora_server_url,
            esplora_client,
            pending_channel_opening_fee_rates: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            oracle_pubkey,
            probes: Probes::default(),
            event_handler: node_event_handler,
        })
    }

    /// Starts the background handles - if the returned handles are dropped, the
    /// background tasks are stopped.
    // TODO: Consider having handles for *all* the tasks & threads for a clean shutdown.
    pub fn start(
        &self,
        event_handler: impl EventHandlerTrait + 'static,
        mobile_interruptable_platform: bool,
    ) -> Result<RunningNode> {
        let mut handles = vec![spawn_connection_management(
            self.peer_manager.clone(),
            self.listen_address,
        )];

        std::thread::spawn(shadow_sync_periodically(
            self.settings.clone(),
            self.node_storage.clone(),
            self.wallet.clone(),
            self.channel_manager.clone(),
        ));

        tokio::spawn(periodic_lightning_wallet_sync(
            self.channel_manager.clone(),
            self.chain_monitor.clone(),
            self.settings.clone(),
            self.esplora_client.clone(),
        ));

        tokio::spawn(update_fee_rate_estimates(
            self.settings.clone(),
            self.fee_rate_estimator.clone(),
        ));

        handles.push(spawn_background_processor(
            self.peer_manager.clone(),
            self.channel_manager.clone(),
            self.chain_monitor.clone(),
            self.logger.clone(),
            self.ln_storage.clone(),
            event_handler,
            self.gossip_source.clone(),
            self.scorer.clone(),
            mobile_interruptable_platform,
        ));

        handles.push(spawn_broadcast_node_annoucements(
            &self.alias,
            self.announcement_addresses.clone(),
            self.peer_manager.clone(),
            self.channel_manager.clone(),
        )?);

        handles.push(manage_sub_channels(
            self.sub_channel_manager.clone(),
            self.dlc_message_handler.clone(),
            self.peer_manager.clone(),
            self.settings.clone(),
        ));

        handles.push(manage_dlc_manager(
            self.dlc_manager.clone(),
            self.settings.clone(),
        ));

        handles.push(spawn_keep_rgs_snapshot_up_to_date(
            self.gossip_source.clone(),
        ));

        tokio::spawn(manage_spendable_outputs_task(
            self.esplora_server_url.clone(),
            self.node_storage.clone(),
            self.wallet.clone(),
            self.fee_rate_estimator.clone(),
            self.keys_manager.clone(),
        ));

        tracing::info!("Lightning node started with node ID {}", self.info);

        Ok(RunningNode { _handles: handles })
    }

    pub fn update_ldk_settings(&self, ldk_config: UserConfig) {
        tracing::debug!("Updating LDK settings");
        *self.ldk_config.write() = ldk_config;

        tracing::info!(?ldk_config, "Updated LDK settings");

        for channel in self.list_channels() {
            let channel_id = channel.channel_id;
            let peer_id = channel.counterparty.node_id;
            if let Err(e) = self.channel_manager.update_channel_config(
                &peer_id,
                &[channel_id],
                &ldk_config.channel_config,
            ) {
                tracing::error!(
                    channel_id = %channel_id.to_hex(),
                    %peer_id,
                    "Failed to apply new channel configuration: {e:?}"
                );
            }
        }
    }

    pub async fn sub_channel_manager_periodic_check(&self) -> Result<()> {
        sub_channel_manager_periodic_check(
            self.sub_channel_manager.clone(),
            &self.dlc_message_handler,
            &self.peer_manager,
        )
        .await
    }

    /// Returns a closure which triggers an on-chain sync and subsequently updates the address
    /// cache, at an interval.
    ///
    /// The task will loop at an interval determined by the node's [`LnDlcNodeSettings`].
    ///
    /// Suitable for daemons such as the coordinator and the maker.
    pub fn sync_on_chain_wallet_periodically(&self) -> impl Fn() {
        let handle = tokio::runtime::Handle::current();
        let settings = self.settings.clone();
        let ln_dlc_wallet = self.wallet.clone();
        move || loop {
            if let Err(e) = ln_dlc_wallet.sync_and_update_address_cache() {
                tracing::error!("Failed on-chain sync: {e:#}");
            }

            let interval = handle.block_on(async {
                let guard = settings.read().await;
                guard.on_chain_sync_interval
            });

            std::thread::sleep(interval);
        }
    }

    pub fn sync_on_chain_wallet(&self) -> Result<()> {
        self.wallet.sync_and_update_address_cache()
    }

    pub fn sync_lightning_wallet(&self) -> Result<()> {
        lightning_wallet_sync(
            &self.channel_manager,
            &self.chain_monitor,
            &self.esplora_client,
        )
    }

    /// Calculate the fee for sending the given `amount_sats` to the given `address` on-chain with
    /// the given `fee`.
    pub fn calculate_fee(
        &self,
        address: &bitcoin::Address,
        amount_sats: u64,
        fee: ConfirmationTarget,
    ) -> Result<Amount> {
        self.wallet
            .ldk_wallet()
            .calculate_fee(address, amount_sats, fee)
    }

    /// Send the given `amount_sats` sats to the given `address` on-chain.
    pub fn send_to_address(
        &self,
        address: &bitcoin::Address,
        amount_sats: u64,
        fee: Fee,
    ) -> Result<Txid> {
        self.wallet
            .ldk_wallet()
            .send_to_address(address, amount_sats, fee)
    }
}

async fn update_fee_rate_estimates(
    settings: Arc<RwLock<LnDlcNodeSettings>>,
    fee_rate_estimator: Arc<FeeRateEstimator>,
) {
    loop {
        if let Err(err) = fee_rate_estimator.update().await {
            tracing::error!("Failed to update fee rate estimates: {err:#}");
        }

        let interval = {
            let guard = settings.read().await;
            guard.fee_rate_sync_interval
        };
        tokio::time::sleep(interval).await;
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_background_processor<S: TenTenOneStorage + 'static, N: Storage + Sync + Send + 'static>(
    peer_manager: Arc<PeerManager<S, N>>,
    channel_manager: Arc<ChannelManager<S, N>>,
    chain_monitor: Arc<ChainMonitor<S, N>>,
    logger: Arc<TracingLogger>,
    persister: Arc<S>,
    event_handler: impl EventHandlerTrait + 'static,
    gossip_source: Arc<GossipSource>,
    scorer: Arc<std::sync::RwLock<Scorer>>,
    mobile_interruptable_platform: bool,
) -> RemoteHandle<()> {
    tracing::info!("Starting background processor");
    let (fut, remote_handle) = async move {
        if let Err(e) = process_events_async(
            persister,
            |e| event_handler.handle_event(e),
            chain_monitor,
            channel_manager,
            gossip_source.as_gossip_sync(),
            peer_manager,
            logger,
            Some(scorer),
            |d| {
                Box::pin(async move {
                    tokio::time::sleep(d).await;
                    false
                })
            },
            mobile_interruptable_platform,
        )
        .await
        {
            tracing::error!("Error running background processor: {e}");
        }
    }
    .remote_handle();
    tokio::spawn(fut);
    remote_handle
}

async fn periodic_lightning_wallet_sync<S: TenTenOneStorage, N: Storage + Sync + Send>(
    channel_manager: Arc<ChannelManager<S, N>>,
    chain_monitor: Arc<ChainMonitor<S, N>>,
    settings: Arc<RwLock<LnDlcNodeSettings>>,
    esplora_client: Arc<EsploraSyncClient<Arc<TracingLogger>>>,
) {
    loop {
        if let Err(e) = lightning_wallet_sync(&channel_manager, &chain_monitor, &esplora_client) {
            tracing::error!("Background sync of Lightning wallet failed: {e:#}")
        }

        let interval = {
            let guard = settings.read().await;
            guard.off_chain_sync_interval
        };
        tokio::time::sleep(interval).await;
    }
}

fn lightning_wallet_sync<S: TenTenOneStorage, N: Storage + Sync + Send>(
    channel_manager: &ChannelManager<S, N>,
    chain_monitor: &ChainMonitor<S, N>,
    esplora_client: &EsploraSyncClient<Arc<TracingLogger>>,
) -> Result<()> {
    let now = Instant::now();
    let confirmables = vec![
        channel_manager as &(dyn Confirm + Sync + Send),
        chain_monitor as &(dyn Confirm + Sync + Send),
    ];
    esplora_client
        .sync(confirmables)
        .context("Lightning wallet sync failed")?;

    tracing::trace!(
        "Lightning wallet sync finished in {}ms.",
        now.elapsed().as_millis()
    );

    Ok(())
}

fn shadow_sync_periodically<S: TenTenOneStorage, N: Storage>(
    settings: Arc<RwLock<LnDlcNodeSettings>>,
    node_storage: Arc<N>,
    ln_dlc_wallet: Arc<LnDlcWallet<S, N>>,
    channel_manager: Arc<ChannelManager<S, N>>,
) -> impl Fn() {
    let handle = tokio::runtime::Handle::current();
    let shadow = Shadow::new(node_storage, ln_dlc_wallet, channel_manager);
    move || loop {
        if let Err(e) = shadow.sync_channels() {
            tracing::error!("Failed to sync channel shadows. Error: {e:#}");
        }

        if let Err(e) = shadow.sync_transactions() {
            tracing::error!("Failed to sync transaction shadows. Error: {e:#}");
        }

        let interval = handle.block_on(async {
            let guard = settings.read().await;
            guard.shadow_sync_interval
        });

        std::thread::sleep(interval);
    }
}

fn spawn_connection_management<
    S: TenTenOneStorage + 'static,
    N: Storage + Send + Sync + 'static,
>(
    peer_manager: Arc<PeerManager<S, N>>,
    listen_address: SocketAddr,
) -> RemoteHandle<()> {
    let (fut, remote_handle) = async move {
        let mut connection_handles = Vec::new();

        let listener = tokio::net::TcpListener::bind(listen_address)
            .await
            .expect("Failed to bind to listen port");
        loop {
            let peer_manager = peer_manager.clone();
            let (tcp_stream, addr) = match listener.accept().await {
                Ok(ret) => ret,
                Err(e) => {
                    tracing::error!("Failed to accept incoming connection: {e:#}");
                    continue;
                }
            };

            tracing::debug!(%addr, "Received inbound connection");

            let (fut, connection_handle) = async move {
                lightning_net_tokio::setup_inbound(
                    peer_manager.clone(),
                    tcp_stream.into_std().expect("Stream conversion to succeed"),
                )
                .await;
            }
            .remote_handle();

            connection_handles.push(connection_handle);

            tokio::spawn(fut);
        }
    }
    .remote_handle();

    tokio::spawn(fut);

    tracing::info!("Listening on {listen_address}");

    remote_handle
}

fn spawn_broadcast_node_annoucements<
    S: TenTenOneStorage + 'static,
    N: Storage + Sync + Send + 'static,
>(
    alias: &str,
    announcement_addresses: Vec<SocketAddress>,
    peer_manager: Arc<PeerManager<S, N>>,
    channel_manager: Arc<ChannelManager<S, N>>,
) -> Result<RemoteHandle<()>> {
    let alias = alias_as_bytes(alias)?;
    let (fut, remote_handle) = async move {
        let mut interval = tokio::time::interval(BROADCAST_NODE_ANNOUNCEMENT_INTERVAL);
        loop {
            if channel_manager.list_channels().iter().any(|c| c.is_public) {
                // Other nodes will ignore our node announcement if we don't have at least one
                // public channel, hence, we should only broadcast our node
                // announcement if we have at least one channel.
                broadcast_node_announcement(&peer_manager, alias, announcement_addresses.clone());
            }

            interval.tick().await;
        }
    }
    .remote_handle();
    tokio::spawn(fut);
    Ok(remote_handle)
}

async fn manage_spendable_outputs_task<
    S: TenTenOneStorage + 'static,
    N: Storage + Sync + Send + 'static,
>(
    esplora_server_url: String,
    node_storage: Arc<N>,
    ln_dlc_wallet: Arc<LnDlcWallet<S, N>>,
    fee_rate_estimator: Arc<FeeRateEstimator>,
    keys_manager: Arc<CustomKeysManager<S, N>>,
) {
    let client = Arc::new(esplora_client::BlockingClient::from_agent(
        esplora_server_url,
        ureq::agent(),
    ));
    loop {
        if let Err(e) = spawn_blocking({
            let client = client.clone();
            let node_storage = node_storage.clone();
            let ln_dlc_wallet = ln_dlc_wallet.clone();
            let fee_rate_estimator = fee_rate_estimator.clone();
            let keys_manager = keys_manager.clone();
            move || {
                manage_spendable_outputs(
                    node_storage,
                    client,
                    ln_dlc_wallet,
                    fee_rate_estimator,
                    keys_manager,
                )
            }
        })
        .await
        .expect("task to complete")
        {
            tracing::error!("Failed to deal with spendable outputs: {e:#}");
        };

        tokio::time::sleep(MANAGE_SPENDABLE_OUTPUTS_INTERVAL).await;
    }
}

/// Spawn a task that manages subchannels
fn manage_sub_channels<S: TenTenOneStorage + 'static, N: Storage + Sync + Send + 'static>(
    sub_channel_manager: Arc<SubChannelManager<S, N>>,
    dlc_message_handler: Arc<DlcMessageHandler>,
    peer_manager: Arc<PeerManager<S, N>>,
    settings: Arc<RwLock<LnDlcNodeSettings>>,
) -> RemoteHandle<()> {
    let (fut, remote_handle) = {
        async move {
            loop {
                tracing::trace!("Started periodic check");
                let now = Instant::now();
                if let Err(e) = sub_channel_manager_periodic_check(
                    sub_channel_manager.clone(),
                    &dlc_message_handler,
                    &peer_manager,
                )
                .await
                {
                    tracing::error!("Failed to process pending DLC actions: {e:#}");
                };

                tracing::trace!(
                    duration = now.elapsed().as_millis(),
                    "Finished periodic check"
                );

                let interval = {
                    let guard = settings.read().await;
                    guard.sub_channel_manager_periodic_check_interval
                };
                tokio::time::sleep(interval).await;
            }
        }
    }
    .remote_handle();

    tokio::spawn(fut);

    remote_handle
}

/// Spawn a task that manages dlc manager
fn manage_dlc_manager<S: TenTenOneStorage + 'static, N: Storage + Sync + Send + 'static>(
    dlc_manager: Arc<DlcManager<S, N>>,
    settings: Arc<RwLock<LnDlcNodeSettings>>,
) -> RemoteHandle<()> {
    let (fut, remote_handle) = {
        async move {
            loop {
                tracing::trace!("Started periodic dlc manager check");
                let now = Instant::now();

                if let Err(e) = dlc_manager.periodic_chain_monitor() {
                    tracing::error!("Failed to perform periodic chain monitor check: {e:#}");
                };

                tracing::trace!(
                    duration = now.elapsed().as_millis(),
                    "Finished periodic check"
                );

                let interval = {
                    let guard = settings.read().await;
                    guard.dlc_manager_periodic_check_interval
                };
                tokio::time::sleep(interval).await;
            }
        }
    }
    .remote_handle();

    tokio::spawn(fut);

    remote_handle
}

fn spawn_keep_rgs_snapshot_up_to_date(gossip_source: Arc<GossipSource>) -> RemoteHandle<()> {
    let (fut, remote_handle) = async move {
        if let GossipSource::RapidGossipSync {
            gossip_sync,
            server_url,
        } = gossip_source.as_ref()
        {
            tracing::info!("Keeping RGS snapshot up to date");

            let mut latest_sync_timestamp = gossip_sync
                .network_graph()
                .get_last_rapid_gossip_sync_timestamp()
                .unwrap_or_default();

            loop {
                match update_rgs_snapshot(gossip_sync.clone(), server_url, latest_sync_timestamp)
                    .await
                {
                    Ok(timestamp) => latest_sync_timestamp = timestamp,
                    Err(e) => {
                        tracing::error!("Failed to update RGS snapshot: {e:#}");
                    }
                }

                tokio::time::sleep(RGS_SYNC_INTERVAL).await;
            }
        }
    }
    .remote_handle();

    tokio::spawn(fut);

    remote_handle
}

async fn update_rgs_snapshot(
    gossip_sync: Arc<RapidGossipSync>,
    rgs_server_url: &str,
    latest_sync_timestamp: u32,
) -> Result<u32> {
    tracing::info!(%rgs_server_url, %latest_sync_timestamp, "Requesting RGS gossip update");

    let query_url = format!("{}/{}", rgs_server_url, latest_sync_timestamp);
    let response = reqwest::get(query_url)
        .await
        .context("Failed to retrieve RGS gossip update")?
        .error_for_status()
        .context("Failed to retrieve RGS gossip update")?;

    let update_data = response
        .bytes()
        .await
        .context("Failed to get RGS gossip update response bytes")?;

    let new_latest_sync_timestamp = gossip_sync
        .update_network_graph(&update_data)
        .map_err(|e| anyhow!("Failed to update network graph: {e:?}"))?;

    tracing::info!(%new_latest_sync_timestamp, "Updated network graph");

    Ok(new_latest_sync_timestamp)
}

impl Display for NodeInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        format!("{}@{}", self.pubkey, self.address).fmt(f)
    }
}
