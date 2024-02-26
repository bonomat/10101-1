use crate::bitcoin_conversion::to_network_29;
use crate::bitcoin_conversion::to_secp_pk_30;
use crate::blockchain::Blockchain;
use crate::channel::UserChannelId;
use crate::dlc_custom_signer::CustomKeysManager;
use crate::dlc_wallet::DlcWallet;
use crate::fee_rate_estimator::FeeRateEstimator;
use crate::ln::manage_spendable_outputs;
use crate::ln::GossipSource;
use crate::ln::TracingLogger;
use crate::node::event::NodeEventHandler;
use crate::node::sub_channel::sub_channel_manager_periodic_check;
use crate::on_chain_wallet::BdkStorage;
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
use anyhow::Context;
use anyhow::Result;
use bdk::FeeRate;
use bdk_esplora::esplora_client;
use bitcoin::address::NetworkUnchecked;
use bitcoin::secp256k1::PublicKey;
use bitcoin::secp256k1::XOnlyPublicKey;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::Network;
use bitcoin::Txid;
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
use lightning::routing::scoring::ProbabilisticScoringDecayParameters;
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
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
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
mod oracle;
mod storage;
mod sub_channel_manager;
mod wallet;

pub(crate) mod sub_channel;

pub mod dlc_channel;
pub mod event;
pub mod peer_manager;

pub use ::dlc_manager as rust_dlc_manager;
pub use channel_manager::ChannelManager;
pub use connection::TenTenOneOnionMessageHandler;
pub use dlc_manager::signed_channel_state_name;
pub use dlc_manager::DlcManager;
pub use oracle::OracleInfo;
pub use storage::InMemoryStore;
pub use storage::Storage;
pub use sub_channel::dlc_message_name;
pub use sub_channel_manager::SubChannelManager;

/// The interval at which spendable outputs generated by LDK are considered for spending.
const MANAGE_SPENDABLE_OUTPUTS_INTERVAL: Duration = Duration::from_secs(30 * 60);

type Scorer = ProbabilisticScorer<Arc<NetworkGraph>, Arc<TracingLogger>>;

type NodeEsploraClient = EsploraSyncClient<Arc<TracingLogger>>;

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
pub struct Node<D: BdkStorage, S: TenTenOneStorage, N: Storage> {
    pub settings: Arc<RwLock<LnDlcNodeSettings>>,
    pub network: Network,

    pub(crate) wallet: Arc<OnChainWallet<D>>,
    pub blockchain: Arc<Blockchain<N>>,

    // Making this public is only necessary because of the collaborative revert protocol.
    pub dlc_wallet: Arc<DlcWallet<D, S, N>>,

    pub peer_manager: Arc<PeerManager<D, S, N>>,
    pub channel_manager: Arc<ChannelManager<D, S, N>>,
    pub chain_monitor: Arc<ChainMonitor<S, N>>,
    pub keys_manager: Arc<CustomKeysManager<D>>,
    pub network_graph: Arc<NetworkGraph>,
    pub fee_rate_estimator: Arc<FeeRateEstimator>,

    pub logger: Arc<TracingLogger>,

    pub info: NodeInfo,

    pub dlc_manager: Arc<DlcManager<D, S, N>>,
    pub sub_channel_manager: Arc<SubChannelManager<D, S, N>>,

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
    #[allow(dead_code)]
    listen_address: SocketAddr, // Irrelevant when using websockets
    gossip_source: Arc<GossipSource>,
    pub scorer: Arc<std::sync::RwLock<Scorer>>,
    electrs_server_url: String,
    esplora_client: Arc<NodeEsploraClient>,
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
    pub is_ws: bool,
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

impl<D: BdkStorage, S: TenTenOneStorage + 'static, N: Storage + Sync + Send + 'static>
    Node<D, S, N>
{
    pub async fn update_settings(&self, new_settings: LnDlcNodeSettings) {
        tracing::info!(?new_settings, "Updating LnDlcNode settings");
        *self.settings.write().await = new_settings;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        // Supplied configuration of LDK node.
        ldk_config: UserConfig,
        alias: &str,
        network: Network,
        data_dir: &Path,
        storage: S,
        node_storage: Arc<N>,
        wallet_storage: D,
        announcement_address: SocketAddr,
        listen_address: SocketAddr,
        electrs_server_url: String,
        seed: Bip39Seed,
        ephemeral_randomness: [u8; 32],
        settings: LnDlcNodeSettings,
        oracle_clients: Vec<P2PDOracleClient>,
        oracle_pubkey: XOnlyPublicKey,
        node_event_handler: Arc<NodeEventHandler>,
    ) -> Result<Self> {
        let time_since_unix_epoch = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;

        let logger = Arc::new(TracingLogger {
            alias: alias.to_string(),
        });

        let ldk_config = Arc::new(parking_lot::RwLock::new(ldk_config));

        let fee_rate_estimator = Arc::new(FeeRateEstimator::new(network));

        let on_chain_wallet = OnChainWallet::new(
            network,
            seed.wallet_seed(),
            wallet_storage,
            fee_rate_estimator.clone(),
        )?;
        let on_chain_wallet = Arc::new(on_chain_wallet);

        let blockchain = Blockchain::new(electrs_server_url.clone(), node_storage.clone())?;
        let blockchain = Arc::new(blockchain);

        let esplora_client = Arc::new(EsploraSyncClient::new(
            electrs_server_url.clone(),
            logger.clone(),
        ));

        let dlc_storage = Arc::new(DlcStorageProvider::new(storage.clone()));
        let ln_storage = Arc::new(storage);

        let chain_monitor: Arc<ChainMonitor<S, N>> = Arc::new(chainmonitor::ChainMonitor::new(
            Some(esplora_client.clone()),
            blockchain.clone(),
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
                on_chain_wallet.clone(),
            ))
        };

        let network_graph = Arc::new(NetworkGraph::new(to_network_29(network), logger.clone()));

        let scorer = ProbabilisticScorer::new(
            ProbabilisticScoringDecayParameters::default(),
            network_graph.clone(),
            logger.clone(),
        );
        let scorer = std::sync::RwLock::new(scorer);
        let scorer = Arc::new(scorer);

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
            blockchain.clone(),
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

        let dlc_wallet = DlcWallet::new(
            on_chain_wallet.clone(),
            dlc_storage.clone(),
            blockchain.clone(),
        );
        let dlc_wallet = Arc::new(dlc_wallet);

        let dlc_manager = dlc_manager::build(
            data_dir,
            dlc_wallet.clone(),
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

        let peer_manager: Arc<PeerManager<D, S, N>> = Arc::new(PeerManager::new(
            lightning_msg_handler,
            time_since_unix_epoch.as_secs() as u32,
            &ephemeral_randomness,
            logger.clone(),
            keys_manager.clone(),
        ));

        let node_info = NodeInfo {
            pubkey: to_secp_pk_30(channel_manager.get_our_node_id()),
            address: announcement_address,
            is_ws: false,
        };

        let gossip_source = Arc::new(gossip_source);

        let settings = Arc::new(RwLock::new(settings));

        Ok(Self {
            network,
            wallet: on_chain_wallet,
            blockchain,
            dlc_wallet,
            peer_manager,
            keys_manager,
            chain_monitor,
            logger,
            channel_manager: channel_manager.clone(),
            info: node_info,
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
            scorer,
            electrs_server_url,
            esplora_client,
            oracle_pubkey,
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
        #[cfg(feature = "ln_net_tcp")]
        let mut handles = vec![spawn_connection_management(
            self.peer_manager.clone(),
            self.listen_address,
        )];

        #[cfg(not(feature = "ln_net_tcp"))]
        let mut handles = Vec::new();

        std::thread::spawn(shadow_sync_periodically(
            self.settings.clone(),
            self.node_storage.clone(),
            self.wallet.clone(),
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

        handles.push(manage_sub_channels(self.sub_channel_manager.clone()));

        handles.push(manage_dlc_manager(
            self.dlc_manager.clone(),
            self.settings.clone(),
        ));

        tokio::spawn(manage_spendable_outputs_task::<D, N>(
            self.electrs_server_url.clone(),
            self.node_storage.clone(),
            self.wallet.clone(),
            self.blockchain.clone(),
            self.fee_rate_estimator.clone(),
            self.keys_manager.clone(),
        ));

        tracing::info!("Lightning node started with node ID {}", self.info);

        Ok(RunningNode { _handles: handles })
    }

    pub async fn sub_channel_manager_periodic_check(&self) -> Result<()> {
        sub_channel_manager_periodic_check(self.sub_channel_manager.clone()).await
    }

    pub fn sync_lightning_wallet(&self) -> Result<()> {
        lightning_wallet_sync(
            &self.channel_manager,
            &self.chain_monitor,
            &self.esplora_client,
        )
    }

    /// Estimate the fee for sending the given `amount_sats` to the given `address` on-chain with
    /// the given `fee`.
    pub fn estimate_fee(
        &self,
        address: Address<NetworkUnchecked>,
        amount_sats: u64,
        fee: ConfirmationTarget,
    ) -> Result<Amount> {
        let address = address.require_network(self.network)?;

        self.wallet.estimate_fee(&address, amount_sats, fee)
    }

    /// Send the given `amount_sats` sats to the given unchecked, on-chain `address`.
    pub async fn send_to_address(
        &self,
        address: Address<NetworkUnchecked>,
        amount_sats: u64,
        fee: Fee,
    ) -> Result<Txid> {
        let address = address.require_network(self.network)?;

        let tx = spawn_blocking({
            let wallet = self.wallet.clone();
            move || {
                let tx = wallet.build_on_chain_payment_tx(&address, amount_sats, fee)?;

                anyhow::Ok(tx)
            }
        })
        .await
        .expect("task to complete")?;

        let txid = self.blockchain.broadcast_transaction_blocking(&tx)?;

        Ok(txid)
    }

    pub fn list_peers(&self) -> Vec<PublicKey> {
        self.peer_manager
            .get_peer_node_ids()
            .into_iter()
            .map(|(peer, _)| to_secp_pk_30(peer))
            .collect()
    }

    pub fn sign_message(&self, data: String) -> Result<String> {
        let secret = self.keys_manager.get_node_secret_key();
        let signature = lightning::util::message_signing::sign(data.as_bytes(), &secret)?;
        Ok(signature)
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
fn spawn_background_processor<
    D: BdkStorage,
    S: TenTenOneStorage + 'static,
    N: Storage + Sync + Send + 'static,
>(
    peer_manager: Arc<PeerManager<D, S, N>>,
    channel_manager: Arc<ChannelManager<D, S, N>>,
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

async fn periodic_lightning_wallet_sync<
    D: BdkStorage,
    S: TenTenOneStorage,
    N: Storage + Sync + Send,
>(
    channel_manager: Arc<ChannelManager<D, S, N>>,
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

fn lightning_wallet_sync<D: BdkStorage, S: TenTenOneStorage, N: Storage + Sync + Send>(
    channel_manager: &ChannelManager<D, S, N>,
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

fn shadow_sync_periodically<D: BdkStorage, N: Storage>(
    settings: Arc<RwLock<LnDlcNodeSettings>>,
    node_storage: Arc<N>,
    wallet: Arc<OnChainWallet<D>>,
) -> impl Fn() {
    let handle = tokio::runtime::Handle::current();
    let shadow = Shadow::new(node_storage, wallet);
    move || loop {
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

#[cfg(feature = "ln_net_tcp")]
fn spawn_connection_management<
    D: BdkStorage,
    S: TenTenOneStorage + 'static,
    N: Storage + Send + Sync + 'static,
>(
    peer_manager: Arc<PeerManager<D, S, N>>,
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
                crate::networking::tcp::setup_inbound(
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

async fn manage_spendable_outputs_task<D: BdkStorage, N: Storage + Sync + Send + 'static>(
    electrs_server_url: String,
    node_storage: Arc<N>,
    wallet: Arc<OnChainWallet<D>>,
    blockchain: Arc<Blockchain<N>>,
    fee_rate_estimator: Arc<FeeRateEstimator>,
    keys_manager: Arc<CustomKeysManager<D>>,
) {
    let client = Arc::new(esplora_client::BlockingClient::from_agent(
        electrs_server_url,
        ureq::agent(),
    ));
    loop {
        if let Err(e) = spawn_blocking({
            let client = client.clone();
            let node_storage = node_storage.clone();
            let ln_dlc_wallet = wallet.clone();
            let blockchain = blockchain.clone();
            let fee_rate_estimator = fee_rate_estimator.clone();
            let keys_manager = keys_manager.clone();
            move || {
                manage_spendable_outputs(
                    node_storage,
                    client,
                    ln_dlc_wallet,
                    blockchain,
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
fn manage_sub_channels<
    D: BdkStorage,
    S: TenTenOneStorage + 'static,
    N: Storage + Send + Sync + 'static,
>(
    sub_channel_manager: Arc<SubChannelManager<D, S, N>>,
) -> RemoteHandle<()> {
    let (fut, remote_handle) = {
        async move {
            loop {
                tracing::trace!("Started periodic check");
                let now = Instant::now();
                if let Err(e) =
                    sub_channel_manager_periodic_check(sub_channel_manager.clone()).await
                {
                    tracing::error!("Failed to process pending DLC actions: {e:#}");
                };

                tracing::trace!(
                    duration = now.elapsed().as_millis(),
                    "Finished periodic check"
                );

                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
    .remote_handle();

    tokio::spawn(fut);

    remote_handle
}

/// Spawn a task that manages dlc manager
fn manage_dlc_manager<
    D: BdkStorage,
    S: TenTenOneStorage + 'static,
    N: Storage + Send + Sync + 'static,
>(
    dlc_manager: Arc<DlcManager<D, S, N>>,
    settings: Arc<RwLock<LnDlcNodeSettings>>,
) -> RemoteHandle<()> {
    let (fut, remote_handle) = {
        async move {
            loop {
                tracing::trace!("Started DLC manager periodic chain monitor task");
                let now = Instant::now();

                if let Err(e) = dlc_manager.periodic_chain_monitor() {
                    tracing::error!("Failed to run DLC manager periodic chain monitor task: {e:#}");
                };

                tracing::trace!(
                    duration = now.elapsed().as_millis(),
                    "Finished DLC manager periodic chain monitor task"
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

impl Display for NodeInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let scheme = if self.is_ws { "ws" } else { "tcp" };

        format!("{scheme}://{}@{}", self.pubkey, self.address).fmt(f)
    }
}
