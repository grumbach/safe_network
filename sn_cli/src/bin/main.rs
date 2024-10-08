// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#[macro_use]
extern crate tracing;

mod subcommands;

use subcommands::{
    files::files_cmds,
    folders::folders_cmds,
    register::register_cmds,
    wallet::{
        hot_wallet::{wallet_cmds, wallet_cmds_without_client, WalletCmds},
        wo_wallet::{wo_wallet_cmds, wo_wallet_cmds_without_client, WatchOnlyWalletCmds},
    },
    Opt, SubCmd,
};

use bls::SecretKey;
use clap::Parser;
use color_eyre::Result;
use indicatif::ProgressBar;
use sn_client::transfers::bls_secret_from_hex;
use sn_client::{Client, ClientEvent, ClientEventsBroadcaster, ClientEventsReceiver};
#[cfg(feature = "metrics")]
use sn_logging::{metrics::init_metrics, Level, LogBuilder, LogFormat};
use sn_protocol::version::IDENTIFY_PROTOCOL_STR;
use std::{io, path::PathBuf, time::Duration};
use tokio::{sync::broadcast::error::RecvError, task::JoinHandle};

const CLIENT_KEY: &str = "clientkey";

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let opt = Opt::parse();

    if opt.version {
        println!(
            "{}",
            sn_build_info::version_string(
                "Autonomi CLI",
                env!("CARGO_PKG_VERSION"),
                Some(&IDENTIFY_PROTOCOL_STR)
            )
        );
        return Ok(());
    }

    if opt.crate_version {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if opt.protocol_version {
        println!("{}", *IDENTIFY_PROTOCOL_STR);
        return Ok(());
    }

    #[cfg(not(feature = "nightly"))]
    if opt.package_version {
        println!("{}", sn_build_info::package_version());
        return Ok(());
    }

    let logging_targets = vec![
        // TODO: Reset to nice and clean defaults once we have a better idea of what we want
        ("sn_networking".to_string(), Level::INFO),
        ("safe".to_string(), Level::TRACE),
        ("sn_build_info".to_string(), Level::TRACE),
        ("autonomi".to_string(), Level::TRACE),
        ("sn_client".to_string(), Level::TRACE),
        ("sn_logging".to_string(), Level::TRACE),
        ("sn_peers_acquisition".to_string(), Level::TRACE),
        ("sn_protocol".to_string(), Level::TRACE),
        ("sn_registers".to_string(), Level::TRACE),
        ("sn_transfers".to_string(), Level::TRACE),
    ];
    let mut log_builder = LogBuilder::new(logging_targets);
    log_builder.output_dest(opt.log_output_dest);
    log_builder.format(opt.log_format.unwrap_or(LogFormat::Default));
    let _log_handles = log_builder.initialize()?;

    #[cfg(feature = "metrics")]
    tokio::spawn(init_metrics(std::process::id()));

    // Log the full command that was run
    info!("\"{}\"", std::env::args().collect::<Vec<_>>().join(" "));

    debug!(
        "safe client built with git version: {}",
        sn_build_info::git_info()
    );
    println!(
        "safe client built with git version: {}",
        sn_build_info::git_info()
    );

    let client_data_dir_path = get_client_data_dir_path()?;
    // Perform actions that do not require us connecting to the network and return early
    if let Some(SubCmd::Wallet(cmds)) = &opt.cmd {
        if let WalletCmds::Address { .. }
        | WalletCmds::Balance { .. }
        | WalletCmds::Create { .. }
        | WalletCmds::Sign { .. }
        | WalletCmds::Status { .. }
        | WalletCmds::Encrypt { .. } = cmds
        {
            wallet_cmds_without_client(cmds, &client_data_dir_path).await?;
            return Ok(());
        }
    }

    if let Some(SubCmd::WatchOnlyWallet(cmds)) = &opt.cmd {
        if let WatchOnlyWalletCmds::Addresses
        | WatchOnlyWalletCmds::Balance { .. }
        | WatchOnlyWalletCmds::Deposit { .. }
        | WatchOnlyWalletCmds::Create { .. }
        | WatchOnlyWalletCmds::Transaction { .. } = cmds
        {
            wo_wallet_cmds_without_client(cmds, &client_data_dir_path).await?;
            return Ok(());
        }
    }

    println!("Instantiating a SAFE client...");
    let secret_key = get_client_secret_key(&client_data_dir_path)?;

    let bootstrap_peers = opt.peers.get_peers().await?;

    println!(
        "Connecting to the network with {} peers",
        bootstrap_peers.len(),
    );

    let bootstrap_peers = if bootstrap_peers.is_empty() {
        // empty vec is returned if `local-discovery` flag is provided
        None
    } else {
        Some(bootstrap_peers)
    };

    // get the broadcaster as we want to have our own progress bar.
    let broadcaster = ClientEventsBroadcaster::default();
    let (progress_bar, progress_bar_handler) =
        spawn_connection_progress_bar(broadcaster.subscribe());

    let result = Client::new(
        secret_key,
        bootstrap_peers,
        opt.connection_timeout,
        Some(broadcaster),
    )
    .await;
    let client = match result {
        Ok(client) => client,
        Err(err) => {
            // clean up progress bar
            progress_bar.finish_with_message("Could not connect to the network");
            return Err(err.into());
        }
    };
    progress_bar_handler.await?;

    let should_verify_store = !opt.no_verify;

    // PowerShell seems having issue to showing the unwrapped error
    // Hence capture the result and print it out explicity.
    let result = match opt.cmd {
        Some(SubCmd::Wallet(cmds)) => {
            wallet_cmds(cmds, &client, &client_data_dir_path, should_verify_store).await
        }
        Some(SubCmd::WatchOnlyWallet(cmds)) => {
            wo_wallet_cmds(cmds, &client, &client_data_dir_path, should_verify_store).await
        }
        Some(SubCmd::Files(cmds)) => {
            files_cmds(cmds, &client, &client_data_dir_path, should_verify_store).await
        }
        Some(SubCmd::Folders(cmds)) => {
            folders_cmds(cmds, &client, &client_data_dir_path, should_verify_store).await
        }
        Some(SubCmd::Register(cmds)) => {
            register_cmds(cmds, &client, &client_data_dir_path, should_verify_store).await
        }
        None => {
            println!("Use --help to see available commands");
            return Ok(());
        }
    };
    println!("Completed with {result:?}");

    Ok(())
}

/// Helper to subscribe to the client events broadcaster and spin up a progress bar that terminates when the
/// client successfully connects to the network or if it errors out.
fn spawn_connection_progress_bar(mut rx: ClientEventsReceiver) -> (ProgressBar, JoinHandle<()>) {
    // Network connection progress bar
    let progress_bar = ProgressBar::new_spinner();
    let progress_bar_clone = progress_bar.clone();
    progress_bar.enable_steady_tick(Duration::from_millis(120));
    progress_bar.set_message("Connecting to The SAFE Network...");
    let new_style = progress_bar.style().tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈🔗");
    progress_bar.set_style(new_style);

    progress_bar.set_message("Connecting to The SAFE Network...");

    let handle = tokio::spawn(async move {
        let mut peers_connected = 0;
        loop {
            match rx.recv().await {
                Ok(ClientEvent::ConnectedToNetwork) => {
                    progress_bar.finish_with_message("Connected to the Network");
                    break;
                }
                Ok(ClientEvent::PeerAdded {
                    max_peers_to_connect,
                }) => {
                    peers_connected += 1;
                    progress_bar.set_message(format!(
                        "{peers_connected}/{max_peers_to_connect} initial peers found.",
                    ));
                }
                Err(RecvError::Lagged(_)) => {
                    // Even if the receiver is lagged, we would still get the ConnectedToNetwork during each new
                    // connection. Thus it would be okay to skip this error.
                }
                Err(RecvError::Closed) => {
                    progress_bar.finish_with_message("Could not connect to the network");
                    break;
                }
                _ => {}
            }
        }
    });
    (progress_bar_clone, handle)
}

fn get_client_secret_key(root_dir: &PathBuf) -> Result<SecretKey> {
    // create the root directory if it doesn't exist
    std::fs::create_dir_all(root_dir)?;
    let key_path = root_dir.join(CLIENT_KEY);
    let secret_key = if key_path.is_file() {
        info!("Client key found. Loading from file...");
        let secret_hex_bytes = std::fs::read(key_path)?;
        bls_secret_from_hex(secret_hex_bytes)?
    } else {
        info!("No key found. Generating a new client key...");
        let secret_key = SecretKey::random();
        std::fs::write(key_path, hex::encode(secret_key.to_bytes()))?;
        secret_key
    };
    Ok(secret_key)
}

fn get_client_data_dir_path() -> Result<PathBuf> {
    let mut home_dirs = dirs_next::data_dir().expect("Data directory is obtainable");
    home_dirs.push("safe");
    home_dirs.push("client");
    std::fs::create_dir_all(home_dirs.as_path())?;
    Ok(home_dirs)
}

fn get_stdin_response(prompt: &str) -> String {
    println!("{prompt}");
    let mut buffer = String::new();
    let stdin = io::stdin();
    if stdin.read_line(&mut buffer).is_err() {
        // consider if error should process::exit(1) here
        return "".to_string();
    };
    // Remove leading and trailing whitespace
    buffer.trim().to_owned()
}

fn get_stdin_password_response(prompt: &str) -> String {
    rpassword::prompt_password(prompt)
        .map(|v| v.trim().to_owned())
        .unwrap_or("".to_string())
}

#[cfg(test)]
mod tests {
    use crate::subcommands::wallet::hot_wallet::{wallet_cmds_without_client, WalletCmds};
    use crate::subcommands::wallet::WalletApiHelper;
    use bls::SecretKey;
    use color_eyre::Result;
    use sn_client::acc_packet::{load_or_create_mnemonic, secret_key_from_mnemonic};
    use sn_client::transfers::HotWallet;
    use std::path::Path;

    fn create_wallet(root_dir: &Path, derivation_passphrase: Option<String>) -> Result<HotWallet> {
        let mnemonic = load_or_create_mnemonic(root_dir)?;
        let secret_key = secret_key_from_mnemonic(mnemonic, derivation_passphrase)?;
        let wallet = HotWallet::create_from_key(root_dir, secret_key, None)?;
        Ok(wallet)
    }

    #[tokio::test]
    async fn test_wallet_address_command() {
        let tmp_dir = tempfile::tempdir().expect("Could not create temp dir");
        let root_dir = tmp_dir.path().to_path_buf();

        // Create wallet
        let _wallet = create_wallet(&root_dir, None).expect("Could not create wallet");

        let cmds = WalletCmds::Address;

        let result = wallet_cmds_without_client(&cmds, &root_dir).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_wallet_address_command_should_fail_with_no_existing_wallet() {
        let tmp_dir = tempfile::tempdir().expect("Could not create temp dir");
        let client_data_dir = tmp_dir.path().to_path_buf();

        let cmds = WalletCmds::Address;

        // Runs command without a wallet being present, thus should fail
        let result = wallet_cmds_without_client(&cmds, &client_data_dir).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_wallet_create_command() {
        let tmp_dir = tempfile::tempdir().expect("Could not create temp dir");
        let root_dir = tmp_dir.path().to_path_buf();

        let cmds = WalletCmds::Create {
            no_replace: false,
            no_password: true,
            key: None,
            derivation_passphrase: None,
            password: None,
        };

        // Run command and hopefully create a wallet
        let result = wallet_cmds_without_client(&cmds, &root_dir).await;
        assert!(result.is_ok());

        // Check if valid wallet exists
        let result = WalletApiHelper::load_from(&root_dir);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_wallet_create_command_with_hex_key() {
        let tmp_dir = tempfile::tempdir().expect("Could not create temp dir");
        let root_dir = tmp_dir.path().to_path_buf();

        let secret_key = SecretKey::random();
        let secret_key_hex = secret_key.to_hex();

        let cmds = WalletCmds::Create {
            no_replace: false,
            no_password: true,
            key: Some(secret_key_hex),
            derivation_passphrase: None,
            password: None,
        };

        // Run command and hopefully create a wallet
        let result = wallet_cmds_without_client(&cmds, &root_dir).await;
        assert!(result.is_ok());

        // Check if valid wallet exists
        let result = WalletApiHelper::load_from(&root_dir);
        assert!(result.is_ok());

        if let WalletApiHelper::HotWallet(wallet) = result.expect("No valid wallet found") {
            // Compare public addresses (secret keys are the same if the public addresses are)
            assert_eq!(wallet.address().to_hex(), secret_key.public_key().to_hex());
        } else {
            panic!("Did not expect a watch only wallet");
        }
    }
}
