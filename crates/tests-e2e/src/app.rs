use crate::test_subscriber::TestSubscriber;
use crate::test_subscriber::ThreadSafeSenders;
use crate::wait_until;
use native::api;
use native::api::DlcChannel;
use tempfile::TempDir;
use tokio::task::block_in_place;

pub struct AppHandle {
    pub rx: TestSubscriber,
    _app_dir: TempDir,
    _seed_dir: TempDir,
    _handle: tokio::task::JoinHandle<()>,
    _tx: ThreadSafeSenders,
}

impl AppHandle {
    pub fn stop(&self) {
        self._handle.abort()
    }
}

pub async fn run_app(seed_phrase: Option<Vec<String>>) -> AppHandle {
    let app_dir = TempDir::new().unwrap();
    let seed_dir = TempDir::new().unwrap();
    let _app_handle = {
        let as_string = |dir: &TempDir| dir.path().to_str().unwrap().to_string();

        let app_dir = as_string(&app_dir);
        let seed_dir = as_string(&seed_dir);

        native::api::set_config(test_config(), app_dir, seed_dir.clone()).unwrap();

        if let Some(seed_phrase) = seed_phrase {
            tokio::task::spawn_blocking({
                let seed_dir = seed_dir.clone();
                move || {
                    api::restore_from_seed_phrase(
                        seed_phrase.join(" "),
                        format!("{seed_dir}/regtest/seed"),
                    )
                    .unwrap();
                }
            })
            .await
            .unwrap();
        }

        tokio::task::spawn_blocking(move || native::api::run_in_test(seed_dir).unwrap())
    };

    let (rx, tx) = TestSubscriber::new().await;
    let app = AppHandle {
        _app_dir: app_dir,
        _seed_dir: seed_dir,
        _handle: _app_handle,
        rx,
        _tx: tx.clone(),
    };

    native::event::subscribe(tx);

    wait_until!(app.rx.init_msg() == Some("10101 is ready.".to_string()));
    wait_until!(app.rx.wallet_info().is_some()); // wait for initial wallet sync
    app
}

/// Refresh the app's wallet information.
///
/// To call this make sure that you are either outside of a runtime or in a multi-threaded runtime
/// (i.e. use `flavor = "multi_thread"` in a `tokio::test`).
pub fn refresh_wallet_info() {
    // We must `block_in_place` because calling `refresh_wallet_info` starts a new runtime and that
    // cannot happen within another runtime.
    block_in_place(move || api::refresh_wallet_info().unwrap());
}

/// Run periodic checks on the DLC channels, including syncing them with the blockchain.
///
/// To call this make sure that you are either outside of a runtime or in a multi-threaded runtime
/// (i.e. use `flavor = "multi_thread"` in a `tokio::test`).
pub fn sync_dlc_channels() {
    // We must `block_in_place` because calling `sync_dlc_channels` starts a new runtime and that
    // cannot happen within another runtime.
    block_in_place(move || api::sync_dlc_channels().unwrap());
}

/// Force close DLC channel.
///
/// To call this make sure that you are either outside of a runtime or in a multi-threaded runtime
/// (i.e. use `flavor = "multi_thread"` in a `tokio::test`).
pub fn force_close_dlc_channel() {
    // We must `block_in_place` because calling `force_close_channel` starts a new runtime and that
    // cannot happen within another runtime.
    block_in_place(move || api::force_close_channel().unwrap());
}

/// Get the ID of the currently open DLC channel, if there is one.
///
/// To call this make sure that you are either outside of a runtime or in a multi-threaded runtime
/// (i.e. use `flavor = "multi_thread"` in a `tokio::test`).
pub fn get_dlc_channel_id() -> Option<String> {
    block_in_place(move || api::get_dlc_channel_id().unwrap())
}

pub fn get_dlc_channels() -> Vec<DlcChannel> {
    block_in_place(move || api::list_dlc_channels().unwrap())
}

// Values mostly taken from `environment.dart`
fn test_config() -> native::config::api::Config {
    native::config::api::Config {
        coordinator_pubkey: "02dd6abec97f9a748bf76ad502b004ce05d1b2d1f43a9e76bd7d85e767ffb022c9"
            .to_string(),
        esplora_endpoint: "http://127.0.0.1:3000".to_string(),
        host: "127.0.0.1".to_string(),
        p2p_port: 9045,
        http_port: 8000,
        network: "regtest".to_string(),
        oracle_endpoint: "http://127.0.0.1:8081".to_string(),
        oracle_pubkey: "16f88cf7d21e6c0f46bcbc983a4e3b19726c6c98858cc31c83551a88fde171c0"
            .to_string(),
        health_check_interval_secs: 1, // We want to measure health more often in tests
        rgs_server_url: None,
    }
}
