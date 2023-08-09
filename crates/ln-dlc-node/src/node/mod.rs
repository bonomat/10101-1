use crate::disk;
use crate::dlc_custom_signer::CustomKeysManager;
use crate::fee_rate_estimator::FeeRateEstimator;
use crate::ln::manage_spendable_outputs;
use crate::ln::TracingLogger;
use crate::ln_dlc_wallet::LnDlcWallet;
use crate::node::dlc_channel::sub_channel_manager_periodic_check;
use crate::node::peer_manager::alias_as_bytes;
use crate::node::peer_manager::broadcast_node_announcement;
use crate::on_chain_wallet::OnChainWallet;
use crate::seed::Bip39Seed;
use crate::ChainMonitor;
use crate::EventHandlerTrait;
use crate::NetworkGraph;
use crate::PeerManager;
use anyhow::Context;
use anyhow::Result;
use bitcoin::hashes::hex::ToHex;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use dlc_messages::message_handler::MessageHandler as DlcMessageHandler;
use dlc_sled_storage_provider::SledStorageProvider;
use futures::future::RemoteHandle;
use futures::FutureExt;
use lightning::chain::chainmonitor;
use lightning::chain::keysinterface::EntropySource;
use lightning::chain::keysinterface::KeysManager;
use lightning::chain::Confirm;
use lightning::ln::msgs::NetAddress;
use lightning::ln::peer_handler::MessageHandler;
use lightning::routing::gossip::P2PGossipSync;
use lightning::routing::router::DefaultRouter;
use lightning::routing::utxo::UtxoLookup;
use lightning::util::config::UserConfig;
use lightning_background_processor::process_events_async;
use lightning_background_processor::GossipSync;
use lightning_persister::FilesystemPersister;
use lightning_transaction_sync::EsploraSyncClient;
use p2pd_oracle_client::P2PDOracleClient;
use serde::Deserialize;
use serde::Serialize;
use serde_with::serde_as;
use serde_with::DurationSeconds;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use tokio::sync::RwLock;
use tokio::task::spawn_blocking;

mod channel_manager;
mod connection;
pub(crate) mod dlc_channel;
mod dlc_manager;
pub(crate) mod invoice;
mod ln_channel;
mod oracle;
pub mod peer_manager;
mod storage;
mod sub_channel_manager;
mod wallet;

pub use self::dlc_manager::DlcManager;
pub use crate::node::oracle::OracleInfo;
use crate::shadow::Shadow;
pub use ::dlc_manager as rust_dlc_manager;
pub use channel_manager::ChannelManager;
pub use dlc_channel::dlc_message_name;
pub use dlc_channel::sub_channel_message_name;
pub use invoice::HTLCStatus;
use lightning::routing::scoring::ProbabilisticScorer;
pub use storage::InMemoryStore;
pub use storage::Storage;
pub use sub_channel_manager::SubChannelManager;
pub use wallet::PaymentDetails;

/// The interval at which the [`lightning::ln::msgs::NodeAnnouncement`] is broadcast.
///
/// According to the LDK team, a value of up to 1 hour should be fine.
const BROADCAST_NODE_ANNOUNCEMENT_INTERVAL: Duration = Duration::from_secs(3600);

/// The interval at which spendable outputs generated by LDK are considered for spending.
const MANAGE_SPENDABLE_OUTPUTS_INTERVAL: Duration = Duration::from_secs(30 * 60);

type Scorer = ProbabilisticScorer<Arc<NetworkGraph>, Arc<TracingLogger>>;

type NodeGossipSync =
    P2PGossipSync<Arc<NetworkGraph>, Arc<dyn UtxoLookup + Send + Sync>, Arc<TracingLogger>>;

type NodeEsploraClient = EsploraSyncClient<Arc<TracingLogger>>;

type RequestedScid = u64;
type FakeChannelPaymentRequests = Arc<parking_lot::Mutex<HashMap<RequestedScid, PublicKey>>>;

/// An LN-DLC node.
pub struct Node<S> {
    pub settings: Arc<RwLock<LnDlcNodeSettings>>,
    pub network: Network,

    pub(crate) wallet: Arc<LnDlcWallet>,

    pub peer_manager: Arc<PeerManager>,
    pub channel_manager: Arc<ChannelManager>,
    chain_monitor: Arc<ChainMonitor>,
    pub(crate) keys_manager: Arc<CustomKeysManager>,
    pub network_graph: Arc<NetworkGraph>,
    pub fee_rate_estimator: Arc<FeeRateEstimator>,

    logger: Arc<TracingLogger>,

    pub info: NodeInfo,
    pub(crate) fake_channel_payments: FakeChannelPaymentRequests,

    pub dlc_manager: Arc<DlcManager>,
    pub sub_channel_manager: Arc<SubChannelManager>,
    oracle: Arc<P2PDOracleClient>,
    pub dlc_message_handler: Arc<DlcMessageHandler>,
    pub(crate) storage: Arc<S>,
    pub ldk_config: Arc<parking_lot::RwLock<UserConfig>>,

    // fields below are needed only to start the node
    listen_address: SocketAddr,
    gossip_sync: Arc<NodeGossipSync>,
    persister: Arc<FilesystemPersister>,
    alias: String,
    announcement_addresses: Vec<NetAddress>,
    scorer: Arc<Mutex<Scorer>>,
    esplora_server_url: String,
    esplora_client: Arc<NodeEsploraClient>,
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
#[derive(Debug, Clone, Deserialize, Serialize)]
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

    /// When handling the [`Event::HTLCIntercepted`], we may need to
    /// create a new channel with the recipient of the HTLC. If the
    /// payment is small enough (< 1000 sats), opening the channel will
    /// fail unless we provide more outbound liquidity.
    ///
    /// This value defines the maximum channel amount between the coordinator and a user that opens
    /// a channel through an interceptable invoice. Channels that exceed this amount will be
    /// rejected.
    pub max_app_channel_size_sats: u64,
}

impl Default for LnDlcNodeSettings {
    fn default() -> Self {
        Self {
            off_chain_sync_interval: Duration::from_secs(5),
            on_chain_sync_interval: Duration::from_secs(300),
            fee_rate_sync_interval: Duration::from_secs(20),
            dlc_manager_periodic_check_interval: Duration::from_secs(30),
            sub_channel_manager_periodic_check_interval: Duration::from_secs(30),
            forwarding_fee_proportional_millionths: 50,
            shadow_sync_interval: Duration::from_secs(600),
            bdk_client_stop_gap: 20,
            bdk_client_concurrency: 4,
            // 200_000 is an arbitrary number we are feeling comfortable with
            max_app_channel_size_sats: 200_000,
        }
    }
}

impl<S> Node<S>
where
    S: Storage + Send + Sync + 'static,
{
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
        node_storage: Arc<S>,
        announcement_address: SocketAddr,
        listen_address: SocketAddr,
        announcement_addresses: Vec<NetAddress>,
        esplora_server_url: String,
        seed: Bip39Seed,
        ephemeral_randomness: [u8; 32],
        settings: LnDlcNodeSettings,
        oracle_client: P2PDOracleClient,
    ) -> Result<Self>
    where
        SC: Fn(&Path, Arc<NetworkGraph>, Arc<TracingLogger>) -> Scorer,
    {
        let time_since_unix_epoch = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;

        let logger = Arc::new(TracingLogger {
            alias: alias.to_string(),
        });

        let ldk_config = Arc::new(parking_lot::RwLock::new(ldk_config));

        if !data_dir.exists() {
            std::fs::create_dir_all(data_dir)
                .context(format!("Could not create data dir ({data_dir:?})"))?;
        }

        let ldk_data_dir = data_dir.to_string_lossy().to_string();
        let persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

        let dlc_storage = Arc::new(SledStorageProvider::new(
            data_dir.to_str().expect("data_dir"),
        )?);

        let on_chain_dir = data_dir.join("on_chain");
        let on_chain_wallet =
            OnChainWallet::new(on_chain_dir.as_path(), network, seed.wallet_seed())?;

        let esplora_client = Arc::new(EsploraSyncClient::new(
            esplora_server_url.clone(),
            logger.clone(),
        ));

        let fee_rate_estimator = Arc::new(FeeRateEstimator::new(esplora_server_url.clone()));
        let ln_dlc_wallet = {
            Arc::new(LnDlcWallet::new(
                esplora_client.clone(),
                on_chain_wallet.inner,
                fee_rate_estimator.clone(),
                dlc_storage.clone(),
                seed.clone(),
                settings.bdk_client_stop_gap,
                settings.bdk_client_concurrency,
                node_storage.clone(),
            ))
        };

        let settings = Arc::new(RwLock::new(settings));

        let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
            Some(esplora_client.clone()),
            ln_dlc_wallet.clone(),
            logger.clone(),
            fee_rate_estimator.clone(),
            persister.clone(),
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

        let network_graph_path = format!("{ldk_data_dir}/network_graph");
        let network_graph = Arc::new(disk::read_network(
            Path::new(&network_graph_path),
            network,
            logger.clone(),
        ));

        let scorer_path = data_dir.join("scorer");
        let scorer = Arc::new(Mutex::new(read_scorer(
            scorer_path.as_path(),
            network_graph.clone(),
            logger.clone(),
        )));

        let router = Arc::new(DefaultRouter::new(
            network_graph.clone(),
            logger.clone(),
            keys_manager.get_secure_random_bytes(),
            scorer.clone(),
        ));

        let channel_manager = channel_manager::build(
            &ldk_data_dir,
            keys_manager.clone(),
            ln_dlc_wallet.clone(),
            fee_rate_estimator.clone(),
            esplora_client.clone(),
            logger.clone(),
            chain_monitor.clone(),
            *ldk_config.read(),
            network,
            persister.clone(),
            router,
        )?;

        let channel_manager = Arc::new(channel_manager);

        let gossip_sync = Arc::new(P2PGossipSync::new(
            network_graph.clone(),
            None::<Arc<dyn UtxoLookup + Send + Sync>>,
            logger.clone(),
        ));

        let oracle_client = Arc::new(oracle_client);

        let dlc_manager = dlc_manager::build(
            data_dir,
            ln_dlc_wallet.clone(),
            dlc_storage,
            oracle_client.clone(),
            fee_rate_estimator.clone(),
        )?;
        let dlc_manager = Arc::new(dlc_manager);

        let sub_channel_manager =
            sub_channel_manager::build(channel_manager.clone(), dlc_manager.clone())?;

        let dlc_message_handler = Arc::new(DlcMessageHandler::new());

        let lightning_msg_handler = MessageHandler {
            chan_handler: sub_channel_manager.clone(),
            route_handler: gossip_sync.clone(),
            // Hooking the dlc_message_handler here to intercept the `peer_disconnected` event and
            // clear all pending unprocessed message from the disconnected peer.
            onion_message_handler: dlc_message_handler.clone(),
        };

        let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(
            lightning_msg_handler,
            time_since_unix_epoch.as_secs() as u32,
            &ephemeral_randomness,
            logger.clone(),
            dlc_message_handler.clone(),
            keys_manager.clone(),
        ));

        let fake_channel_payments: FakeChannelPaymentRequests =
            Arc::new(parking_lot::Mutex::new(HashMap::new()));

        let node_info = NodeInfo {
            pubkey: channel_manager.get_our_node_id(),
            address: announcement_address,
        };

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
            oracle: oracle_client,
            dlc_message_handler,
            dlc_manager,
            storage: node_storage,
            fee_rate_estimator,
            ldk_config,
            network_graph,
            settings,
            listen_address,
            gossip_sync,
            persister,
            alias: alias.to_string(),
            announcement_addresses,
            scorer,
            esplora_server_url,
            esplora_client,
        })
    }

    /// Starts the background handles - if the returned handles are dropped, the
    /// background tasks are stopped.
    // TODO: Consider having handles for *all* the tasks & threads for a clean shutdown.
    pub fn start(&self, event_handler: impl EventHandlerTrait + 'static) -> Result<RunningNode> {
        let mut handles = vec![spawn_connection_management(
            self.peer_manager.clone(),
            self.listen_address,
        )];

        std::thread::spawn(sync_on_chain_wallet_periodically(
            self.settings.clone(),
            self.wallet.clone(),
        ));

        std::thread::spawn(shadow_sync_periodically(
            self.settings.clone(),
            self.storage.clone(),
            self.wallet.clone(),
            self.channel_manager.clone(),
        ));

        tokio::spawn(lightning_wallet_sync(
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
            self.persister.clone(),
            event_handler,
            self.gossip_sync.clone(),
            self.scorer.clone(),
        ));

        handles.push(spawn_broadcast_node_annoucements(
            &self.alias,
            self.announcement_addresses.clone(),
            self.peer_manager.clone(),
        )?);

        handles.push(manage_sub_channels(
            self.sub_channel_manager.clone(),
            self.dlc_message_handler.clone(),
            self.settings.clone(),
        ));

        tokio::spawn(manage_spendable_outputs_task(
            self.esplora_server_url.clone(),
            self.storage.clone(),
            self.wallet.clone(),
            self.fee_rate_estimator.clone(),
            self.keys_manager.clone(),
        ));

        std::thread::spawn(monitor_for_deadlocks());

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
fn spawn_background_processor(
    peer_manager: Arc<PeerManager>,
    channel_manager: Arc<ChannelManager>,
    chain_monitor: Arc<ChainMonitor>,
    logger: Arc<TracingLogger>,
    persister: Arc<FilesystemPersister>,
    event_handler: impl EventHandlerTrait + 'static,
    gossip_sync: Arc<NodeGossipSync>,
    scorer: Arc<Mutex<Scorer>>,
) -> RemoteHandle<()> {
    tracing::info!("Starting background processor");
    let (fut, remote_handle) = async move {
        if let Err(e) = process_events_async(
            persister,
            |e| event_handler.handle_event(e),
            chain_monitor,
            channel_manager,
            GossipSync::p2p(gossip_sync),
            peer_manager,
            logger,
            Some(scorer),
            |d| {
                Box::pin(async move {
                    tokio::time::sleep(d).await;
                    false
                })
            },
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

/// Parking lot mutexes have the ability to mark deadlocks.
///
/// Take advantage of this behaviour and log deadlocks when they occur.
fn monitor_for_deadlocks() -> impl Fn() {
    move || loop {
        let deadlocks = parking_lot::deadlock::check_deadlock();

        for (i, threads) in deadlocks.iter().enumerate() {
            tracing::error!(%i, "Deadlock detected");
            for t in threads {
                tracing::error!(thread_id = %t.thread_id());
                tracing::error!("{:#?}", t.backtrace());
            }
        }

        std::thread::sleep(Duration::from_secs(10));
    }
}

async fn lightning_wallet_sync(
    channel_manager: Arc<ChannelManager>,
    chain_monitor: Arc<ChainMonitor>,
    settings: Arc<RwLock<LnDlcNodeSettings>>,
    esplora_client: Arc<EsploraSyncClient<Arc<TracingLogger>>>,
) {
    loop {
        let now = Instant::now();
        let confirmables = vec![
            &*channel_manager as &(dyn Confirm + Sync + Send),
            &*chain_monitor as &(dyn Confirm + Sync + Send),
        ];
        match esplora_client.sync(confirmables) {
            Ok(()) => tracing::info!(
                "Background sync of Lightning wallet finished in {}ms.",
                now.elapsed().as_millis()
            ),
            Err(e) => {
                tracing::error!("Background sync of Lightning wallet failed: {e:#}")
            }
        }

        let interval = {
            let guard = settings.read().await;
            guard.off_chain_sync_interval
        };
        tokio::time::sleep(interval).await;
    }
}

fn shadow_sync_periodically<S: Storage + Sync + Send + 'static>(
    settings: Arc<RwLock<LnDlcNodeSettings>>,
    node_storage: Arc<S>,
    ln_dlc_wallet: Arc<LnDlcWallet>,
    channel_manager: Arc<ChannelManager>,
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

fn sync_on_chain_wallet_periodically(
    settings: Arc<RwLock<LnDlcNodeSettings>>,
    ln_dlc_wallet: Arc<LnDlcWallet>,
) -> impl Fn() {
    let handle = tokio::runtime::Handle::current();
    move || loop {
        if let Err(e) = ln_dlc_wallet.inner().sync() {
            tracing::error!("Failed on-chain sync: {e:#}");
        }

        if let Err(e) = ln_dlc_wallet.update_address_cache() {
            tracing::warn!("Failed to update address cache: {e:#}");
        }

        let interval = handle.block_on(async {
            let guard = settings.read().await;
            guard.on_chain_sync_interval
        });

        std::thread::sleep(interval);
    }
}

fn spawn_connection_management(
    peer_manager: Arc<PeerManager>,
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

fn spawn_broadcast_node_annoucements(
    alias: &str,
    announcement_addresses: Vec<NetAddress>,
    peer_manager: Arc<PeerManager>,
) -> Result<RemoteHandle<()>> {
    let alias = alias_as_bytes(alias)?;
    let (fut, remote_handle) = async move {
        let mut interval = tokio::time::interval(BROADCAST_NODE_ANNOUNCEMENT_INTERVAL);
        loop {
            broadcast_node_announcement(&peer_manager, alias, announcement_addresses.clone());

            interval.tick().await;
        }
    }
    .remote_handle();
    tokio::spawn(fut);
    Ok(remote_handle)
}

async fn manage_spendable_outputs_task<S: Storage + Send + Sync + 'static>(
    esplora_server_url: String,
    node_storage: Arc<S>,
    ln_dlc_wallet: Arc<LnDlcWallet>,
    fee_rate_estimator: Arc<FeeRateEstimator>,
    keys_manager: Arc<CustomKeysManager>,
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
fn manage_sub_channels(
    sub_channel_manager: Arc<SubChannelManager>,
    dlc_message_handler: Arc<DlcMessageHandler>,
    settings: Arc<RwLock<LnDlcNodeSettings>>,
) -> RemoteHandle<()> {
    let (fut, remote_handle) = {
        async move {
            loop {
                if let Err(e) = sub_channel_manager_periodic_check(
                    sub_channel_manager.clone(),
                    &dlc_message_handler,
                )
                .await
                {
                    tracing::error!("Failed to process pending DLC actions: {e:#}");
                };

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

impl Display for NodeInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        format!("{}@{}", self.pubkey, self.address).fmt(f)
    }
}
