// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A client for the Walrus blob store.

use std::{
    env,
    fmt::{self, Display},
    io::Write,
    net::SocketAddr,
    num::{NonZeroU16, NonZeroU64},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use serde_with::{base64::Base64, serde_as, DisplayFromStr};
use tracing_subscriber::{
    util::{SubscriberInitExt, TryInitError},
    EnvFilter,
};
use walrus_core::{
    encoding::{encoded_blob_length_for_n_shards, EncodingConfig, Primary},
    metadata::VerifiedBlobMetadataWithId,
    BlobId,
};
use walrus_sdk::api::BlobStatus;
use walrus_service::{
    cli_utils::{
        error,
        format_event_id,
        get_contract_client,
        get_read_client,
        get_sui_read_client_from_rpc_node_or_wallet,
        load_configuration,
        load_wallet_context,
        mist_price_per_blob_size,
        print_walrus_info,
        success,
        HumanReadableBytes,
        HumanReadableMist,
    },
    client::{BlobStoreResult, Client},
    daemon::ClientDaemon,
};
use walrus_sui::client::{ContractClient, ReadClient};

#[derive(Parser, Debug, Clone, Deserialize)]
#[command(author, version, about = "Walrus client", long_about = None)]
#[clap(rename_all = "kebab-case")]
#[serde(rename_all = "camelCase")]
struct App {
    /// The path to the wallet configuration file.
    ///
    /// The Walrus configuration is taken from the following locations:
    ///
    /// 1. From this configuration parameter, if set.
    /// 2. From `./client_config.yaml`.
    /// 3. From `~/.walrus/client_config.yaml`.
    ///
    /// If an invalid path is specified through this option, an error is returned.
    // NB: Keep this in sync with `walrus_service::cli_utils`.
    #[clap(short, long, verbatim_doc_comment)]
    #[serde(default)]
    config: Option<PathBuf>,
    /// The path to the Sui wallet configuration file.
    ///
    /// The wallet configuration is taken from the following locations:
    ///
    /// 1. From this configuration parameter, if set.
    /// 2. From the path specified in the Walrus configuration, if set.
    /// 3. From `./sui_config.yaml`.
    /// 4. From `~/.sui/sui_config/client.yaml`.
    ///
    /// If an invalid path is specified through this option or in the configuration file, an error
    /// is returned.
    // NB: Keep this in sync with `walrus_service::cli_utils`.
    #[clap(short, long, verbatim_doc_comment)]
    #[serde(default)]
    wallet: Option<PathBuf>,
    /// The gas budget for transactions.
    #[clap(short, long, default_value_t = default::gas_budget())]
    #[serde(default = "default::gas_budget")]
    gas_budget: u64,
    /// Write output as JSON.
    ///
    /// This is always done in JSON mode.
    #[clap(long, action)]
    #[serde(default)]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}

#[serde_as]
#[derive(Subcommand, Debug, Clone, Deserialize)]
#[clap(rename_all = "kebab-case")]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
enum Commands {
    /// Store a new blob into Walrus.
    #[clap(alias("write"))]
    Store {
        /// The file containing the blob to be published to Walrus.
        file: PathBuf,
        /// The number of epochs ahead for which to store the blob.
        #[clap(short, long, default_value_t = default::epochs())]
        #[serde(default = "default::epochs")]
        epochs: u64,
        /// Do not check for the blob status before storing it.
        ///
        /// This will create a new blob even if the blob is already certified for a sufficient
        /// duration.
        #[clap(long, action)]
        #[serde(default)]
        force: bool,
    },
    /// Read a blob from Walrus, given the blob ID.
    Read {
        /// The blob ID to be read.
        #[serde_as(as = "DisplayFromStr")]
        blob_id: BlobId,
        /// The file path where to write the blob.
        ///
        /// If unset, prints the blob to stdout.
        #[clap(short, long)]
        #[serde(default)]
        out: Option<PathBuf>,
        #[clap(flatten)]
        #[serde(flatten)]
        rpc_arg: RpcArg,
    },
    /// Get the status of a blob.
    ///
    /// This queries multiple storage nodes representing more than a third of the shards for the
    /// blob status and return the "latest" status (in the life-cycle of a blob) that can be
    /// verified with an on-chain event.
    ///
    /// This does not take into account any transient states. For example, for invalid blobs, there
    /// is a short period in which some of the storage nodes are aware of the inconsistency before
    /// this is posted on chain. During this time period, this command would still return a
    /// "verified" status.
    BlobStatus {
        #[clap(flatten)]
        #[serde(flatten)]
        file_or_blob_id: FileOrBlobId,
        /// Timeout for status requests to storage nodes.
        #[clap(short, long, value_parser = humantime::parse_duration, default_value = "1s")]
        #[serde(default = "default::status_timeout")]
        timeout: Duration,
        #[clap(flatten)]
        #[serde(flatten)]
        rpc_arg: RpcArg,
    },
    /// Run a publisher service at the provided network address.
    ///
    /// This does not perform any type of access control and is thus not suited for a public
    /// deployment when real money is involved.
    Publisher {
        #[clap(flatten)]
        #[serde(flatten)]
        args: PublisherArgs,
    },
    /// Run an aggregator service at the provided network address.
    Aggregator {
        #[clap(flatten)]
        #[serde(flatten)]
        rpc_arg: RpcArg,
        #[clap(flatten)]
        #[serde(flatten)]
        daemon_args: DaemonArgs,
    },
    /// Run a client daemon at the provided network address, combining the functionality of an
    /// aggregator and a publisher.
    Daemon {
        #[clap(flatten)]
        #[serde(flatten)]
        args: PublisherArgs,
    },
    /// Print information about the Walrus storage system this client is connected to.
    Info {
        #[clap(flatten)]
        #[serde(flatten)]
        rpc_arg: RpcArg,
        /// Print extended information for developers.
        #[clap(long, action)]
        #[serde(default)]
        dev: bool,
    },
    /// Run the client by specifying the arguments in a JSON string; CLI options are ignored.
    Json {
        /// The JSON-encoded args for the Walrus CLI; if not present, the args are read from stdin.
        ///
        /// The JSON structure follows the CLI arguments, containing global options and a "command"
        /// object at the root level. The "command" object itself contains the command (e.g.,k
        /// "store", "read", "publisher", "blobStatus", ...) with an object containing the command
        /// options.
        ///
        /// Note that where CLI options are in "kebab-case", the respective JSON strings are in
        /// "camelCase".
        ///
        /// For example, to read a blob and write it to "some_output_file" using a specific
        /// configuration file, you can use the following JSON input:
        ///
        ///     {
        ///       "config": "path/to/client_config.yaml",
        ///       "command": {
        ///         "read": {
        ///           "blobId": "4BKcDC0Ih5RJ8R0tFMz3MZVNZV8b2goT6_JiEEwNHQo",
        ///           "out": "some_output_file"
        ///         }
        ///       }
        ///     }
        ///
        /// Important: If the "read" command does not have an "out" file specified, the output JSON
        /// string will contain the full bytes of the blob, encoded as a Base64 string.
        ///
        /// All commands except "info" are available.
        #[clap(verbatim_doc_comment)]
        command_string: Option<String>,
    },
    /// Encode the specified file to obtain its blob ID.
    BlobId {
        /// The file containing the blob for which to compute the blob ID.
        file: PathBuf,
        /// The number of shards for which to compute the blob ID.
        ///
        /// If not specified, the number of shards is read from chain.
        #[clap(short, long)]
        #[serde(default)]
        n_shards: Option<NonZeroU16>,
        #[clap(flatten)]
        #[serde(flatten)]
        rpc_arg: RpcArg,
    },
}

#[derive(Debug, Clone, Args, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublisherArgs {
    #[clap(flatten)]
    #[serde(flatten)]
    daemon_args: DaemonArgs,
    /// The maximum body size of PUT requests in KiB.
    #[clap(short, long = "max-body-size", default_value_t = default::max_body_size_kib())]
    #[serde(default = "default::max_body_size_kib")]
    pub max_body_size_kib: usize,
}

#[derive(Default, Debug, Clone, Args, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcArg {
    /// The URL of the Sui RPC node to use.
    ///
    /// If unset, the wallet configuration is applied (if set), or the fullnode at
    /// `fullnode.testnet.sui.io:443` is used.
    // NB: Keep this in sync with `walrus_service::cli_utils`.
    #[clap(short, long)]
    #[serde(default)]
    rpc_url: Option<String>,
}

#[derive(Debug, Clone, Args, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonArgs {
    /// The address to which to bind the service.
    #[clap(short, long, default_value_t = default::bind_address())]
    #[serde(default = "default::bind_address")]
    bind_address: SocketAddr,
    /// Path to a blocklist file containing a list (in YAML syntax) of blocked blob IDs.
    #[clap(long)]
    #[serde(default)]
    blocklist: Option<PathBuf>,
}

#[serde_as]
#[derive(Debug, Clone, Args, Deserialize)]
#[serde(rename_all = "camelCase")]
#[group(required = true, multiple = false)]
struct FileOrBlobId {
    /// The file containing the blob to be checked.
    #[clap(short, long)]
    #[serde(default)]
    file: Option<PathBuf>,
    /// The blob ID to be checked.
    #[clap(short, long)]
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[serde(default)]
    blob_id: Option<BlobId>,
}

impl FileOrBlobId {
    fn get_or_compute_blob_id(self, encoding_config: &EncodingConfig) -> Result<BlobId> {
        match self {
            FileOrBlobId {
                blob_id: Some(blob_id),
                ..
            } => Ok(blob_id),
            FileOrBlobId {
                file: Some(file), ..
            } => {
                tracing::debug!(
                    file = %file.display(),
                    "checking status of blob read from the filesystem"
                );
                Ok(*encoding_config
                    .get_blob_encoder(&std::fs::read(&file)?)?
                    .compute_metadata()
                    .blob_id())
            }
            // This case is required for JSON mode where we don't have the clap checking.
            _ => Err(anyhow!("either the file or blob ID must be defined")),
        }
    }
}

mod default {
    use std::{net::SocketAddr, time::Duration};

    pub(crate) fn gas_budget() -> u64 {
        500_000_000
    }

    pub(crate) fn epochs() -> u64 {
        1
    }

    pub(crate) fn max_body_size_kib() -> usize {
        10_240
    }

    pub(crate) fn status_timeout() -> Duration {
        Duration::from_secs(1)
    }

    pub(crate) fn bind_address() -> SocketAddr {
        "127.0.0.1:31415"
            .parse()
            .expect("this is a correct socket address")
    }
}

impl PublisherArgs {
    fn max_body_size(&self) -> usize {
        self.max_body_size_kib << 10
    }

    fn format_max_body_size(&self) -> String {
        format!(
            "{}",
            HumanReadableBytes(
                self.max_body_size()
                    .try_into()
                    .expect("should fit into a `u64`")
            )
        )
    }

    fn print_debug_message(&self, message: &str) {
        tracing::debug!(
            bind_address = %self.daemon_args.bind_address,
            max_body_size = self.format_max_body_size(),
            message
        );
    }
}

/// Helper function to get the correct string depending on the output mode.
fn output_string<T: Display + Serialize>(output: &T, json: bool) -> Result<String> {
    if json {
        Ok(serde_json::to_string(&output)?)
    } else {
        Ok(output.to_string())
    }
}

/// The output of the `store` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreOutput {
    pub result: BlobStoreResult,
    pub n_shards: NonZeroU16,
    pub price_per_unit_size: u64,
}

impl Display for StoreOutput {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.result {
            BlobStoreResult::AlreadyCertified {
                blob_id,
                event,
                end_epoch,
            } => {
                write!(
                    f,
                    "{} Blob was previously certified within Walrus for a sufficient period.\n\
                    Blob ID: {}\nCertification event ID: {}\nEnd epoch (exclusive): {}",
                    success(),
                    blob_id,
                    format_event_id(event),
                    end_epoch,
                )
            }
            BlobStoreResult::NewlyCreated(blob) => {
                let unencoded_length = blob.size;
                let encoded_size =
                    encoded_blob_length_for_n_shards(self.n_shards, unencoded_length)
                        .expect("must be valid as the store succeeded");
                let price = mist_price_per_blob_size(
                    unencoded_length,
                    self.n_shards,
                    self.price_per_unit_size,
                )
                .expect("must be valid as the store succeeded");
                write!(
                    f,
                    "{} Blob stored successfully.\n\
                    Blob ID: {}\n\
                    Unencoded size: {}\n\
                    Encoded size (including metadata): {}\n\
                    Sui object ID: {}\n\
                    Cost (excluding gas): {}",
                    success(),
                    blob.blob_id,
                    HumanReadableBytes(unencoded_length),
                    HumanReadableBytes(encoded_size),
                    blob.id,
                    HumanReadableMist(price),
                )
            }
            BlobStoreResult::MarkedInvalid { blob_id, event } => {
                write!(
                    f,
                    "{} Blob was marked as invalid.\nBlob ID: {}\nInvalidation event ID: {}",
                    error(),
                    blob_id,
                    format_event_id(event),
                )
            }
        }
    }
}

/// The output of the `read` command.
#[serde_as]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadOutput {
    #[serde(skip_serializing_if = "std::option::Option::is_none")]
    out: Option<PathBuf>,
    #[serde_as(as = "DisplayFromStr")]
    blob_id: BlobId,
    // When serializing to JSON, the blob is encoded as Base64 string.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde_as(as = "Base64")]
    blob: Vec<u8>,
}

impl ReadOutput {
    fn new(out: Option<PathBuf>, blob_id: BlobId, orig_blob: Vec<u8>) -> Self {
        // Avoid serializing the blob if there is an output file.
        let blob = if out.is_some() { vec![] } else { orig_blob };
        Self { out, blob_id, blob }
    }
}

impl Display for ReadOutput {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.out {
            Some(path) => {
                write!(
                    f,
                    "{} Blob {} reconstructed from Walrus and written to {}.",
                    success(),
                    self.blob_id,
                    path.display()
                )
            }
            // The full blob has been written to stdout.
            None => write!(f, ""),
        }
    }
}

/// The output of the `blob-id` command.
#[serde_as]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlobIdOutput {
    #[serde_as(as = "DisplayFromStr")]
    blob_id: BlobId,
    file: PathBuf,
    unencoded_length: NonZeroU64,
}

impl Display for BlobIdOutput {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} Blob encoded successfully: {}.\n\
                Unencoded size: {}\nBlob ID: {}",
            success(),
            self.file.display(),
            self.unencoded_length,
            self.blob_id,
        )
    }
}

impl BlobIdOutput {
    fn new(file: &Path, metadata: &VerifiedBlobMetadataWithId) -> Self {
        Self {
            blob_id: *metadata.blob_id(),
            file: file.to_owned(),
            unencoded_length: metadata.metadata().unencoded_length,
        }
    }
}

/// The output of the `blob-status` command.
#[serde_as]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlobStatusOutput {
    #[serde_as(as = "DisplayFromStr")]
    blob_id: BlobId,
    #[serde(skip_serializing_if = "std::option::Option::is_none")]
    file: Option<PathBuf>,
    status: BlobStatus,
}

impl Display for BlobStatusOutput {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let blob_str = if let Some(file) = self.file.clone() {
            format!("{} (file: {})", self.blob_id, file.display())
        } else {
            format!("{}", self.blob_id)
        };
        match self.status {
            BlobStatus::Nonexistent => write!(f, "Blob ID {blob_str} is not stored on Walrus."),
            BlobStatus::Existent {
                status,
                end_epoch,
                status_event,
            } => write!(
                f,
                "Status for blob ID {blob_str}: {}\n\
                    End epoch: {}\n\
                    Related event: {}",
                status.to_string().bold(),
                end_epoch,
                format_event_id(&status_event),
            ),
        }
    }
}

async fn client() -> Result<()> {
    init_tracing_subscriber()?;
    let mut app = App::parse();

    while let Commands::Json { command_string } = app.command {
        tracing::info!("running in JSON mode");
        let command_string = match command_string {
            Some(s) => s,
            None => {
                tracing::debug!("reading JSON input from stdin");
                std::io::read_to_string(std::io::stdin())?
            }
        };
        tracing::debug!(
            command = command_string.replace('\n', ""),
            "running JSON command"
        );
        app = serde_json::from_str(&command_string)?;
        app.json = true;
    }
    run_app(app).await
}

fn init_tracing_subscriber() -> Result<(), TryInitError> {
    // Use INFO level by default.
    let directive = format!(
        "info,{}",
        env::var(EnvFilter::DEFAULT_ENV).unwrap_or_default()
    );
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::new(directive))
        .finish()
        .try_init()
}
async fn run_app(app: App) -> Result<()> {
    let config = load_configuration(&app.config);
    tracing::debug!(?app, ?config, "initializing the client");
    let wallet_path = app.wallet.clone().or(config
        .as_ref()
        .ok()
        .and_then(|conf| conf.wallet_config.clone()));
    let wallet = load_wallet_context(&wallet_path);

    match app.command {
        Commands::Store {
            file,
            epochs,
            force,
        } => {
            let client = get_contract_client(config?, wallet, app.gas_budget, &None).await?;

            tracing::info!(
                file = %file.display(),
                "Storing blob read from the filesystem"
            );
            let result = client
                .reserve_and_store_blob(&std::fs::read(file)?, epochs, force)
                .await?;
            println!(
                "{}",
                output_string(
                    &StoreOutput {
                        result,
                        n_shards: client.encoding_config().n_shards(),
                        price_per_unit_size: client
                            .sui_client()
                            .read_client()
                            .price_per_unit_size()
                            .await
                            .map_err(|error| {
                                tracing::warn!(%error, "could not retrieve storage price");
                            })
                            .unwrap_or_default()
                    },
                    app.json
                )?
            );
        }
        Commands::Read {
            blob_id,
            out,
            rpc_arg: RpcArg { rpc_url },
        } => {
            let client =
                get_read_client(config?, rpc_url, wallet, wallet_path.is_none(), &None).await?;
            let blob = client.read_blob::<Primary>(&blob_id).await?;
            match out.as_ref() {
                Some(path) => std::fs::write(path, &blob)?,
                None => {
                    if !app.json {
                        std::io::stdout().write_all(&blob)?
                    }
                }
            }
            println!(
                "{}",
                output_string(&ReadOutput::new(out, blob_id, blob), app.json)?
            );
        }
        Commands::BlobStatus {
            file_or_blob_id,
            timeout,
            rpc_arg: RpcArg { rpc_url },
        } => {
            tracing::debug!(?file_or_blob_id, "getting blob status");
            let config = config?;
            let sui_read_client = get_sui_read_client_from_rpc_node_or_wallet(
                &config,
                rpc_url,
                wallet,
                wallet_path.is_none(),
            )
            .await?;
            let client = Client::new_read_client(config, &sui_read_client).await?;
            let file = file_or_blob_id.file.clone();
            let blob_id = file_or_blob_id.get_or_compute_blob_id(client.encoding_config())?;

            let status = client
                .get_verified_blob_status(&blob_id, &sui_read_client, timeout)
                .await?;
            println!(
                "{}",
                output_string(
                    &BlobStatusOutput {
                        blob_id,
                        file,
                        status
                    },
                    app.json
                )?
            );
        }
        Commands::Publisher { args } => {
            args.print_debug_message("attempting to run the Walrus publisher");
            let client =
                get_contract_client(config?, wallet, app.gas_budget, &args.daemon_args.blocklist)
                    .await?;
            let publisher = ClientDaemon::new(client, args.daemon_args.bind_address)
                .with_publisher(args.max_body_size());
            publisher.run().await?;
        }
        Commands::Aggregator {
            rpc_arg: RpcArg { rpc_url },
            daemon_args:
                DaemonArgs {
                    bind_address,
                    blocklist,
                },
        } => {
            tracing::debug!(?rpc_url, "attempting to run the Walrus aggregator");
            let client =
                get_read_client(config?, rpc_url, wallet, wallet_path.is_none(), &blocklist)
                    .await?;
            let aggregator = ClientDaemon::new(client, bind_address).with_aggregator();
            aggregator.run().await?;
        }
        Commands::Daemon { args } => {
            args.print_debug_message("attempting to run the Walrus daemon");
            let client =
                get_contract_client(config?, wallet, app.gas_budget, &args.daemon_args.blocklist)
                    .await?;
            let publisher = ClientDaemon::new(client, args.daemon_args.bind_address)
                .with_aggregator()
                .with_publisher(args.max_body_size());
            publisher.run().await?;
        }
        Commands::Info {
            rpc_arg: RpcArg { rpc_url },
            dev,
        } => {
            if app.json {
                // TODO: Implement the info command for JSON as well. (#465)
                return Err(anyhow!("the info command is not available in JSON mode"));
            }
            let config = config?;
            let sui_read_client = get_sui_read_client_from_rpc_node_or_wallet(
                &config,
                rpc_url,
                wallet,
                wallet_path.is_none(),
            )
            .await?;
            print_walrus_info(&sui_read_client, dev).await?;
        }
        Commands::Json { .. } => {
            unreachable!("we unpack JSON commands until we obtain a different command")
        }
        Commands::BlobId {
            file,
            n_shards,
            rpc_arg: RpcArg { rpc_url },
        } => {
            let n_shards = if let Some(n) = n_shards {
                n
            } else {
                let config = config?;
                tracing::debug!("reading `n_shards` from chain");
                let sui_read_client = get_sui_read_client_from_rpc_node_or_wallet(
                    &config,
                    rpc_url,
                    wallet,
                    wallet_path.is_none(),
                )
                .await?;
                sui_read_client.current_committee().await?.n_shards()
            };

            tracing::debug!(%n_shards, "encoding the blob");
            let metadata = EncodingConfig::new(n_shards)
                .get_blob_encoder(&std::fs::read(&file)?)?
                .compute_metadata();

            println!(
                "{}",
                output_string(&BlobIdOutput::new(&file, &metadata), app.json)?
            );
        }
    }
    Ok(())
}

/// The CLI entrypoint.
#[tokio::main]
pub async fn main() -> ExitCode {
    if let Err(e) = client().await {
        // Print any error in a (relatively) user-friendly way.
        eprintln!("{} {:#}", error(), e);
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
