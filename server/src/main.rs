use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use swiftcache::{
    journal::Journal,
    management::{self, LogReload},
    runtime::{Config, RunningService, DEFAULT_MAX_MESSAGE_SIZE},
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};

#[derive(Parser)]
#[command(
    name = "swiftcache",
    version,
    about = "Swift / Clang remote compilation cache and desktop service manager"
)]
struct Cli {
    /// Persist complete transfer/event history here (defaults to the application records directory).
    #[arg(long, global = true)]
    records: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Run the cache, backed by on-disk storage.
    Serve(ServeArgs),
    /// Forward a local Unix socket to a remote TCP cache host.
    Proxy(ProxyArgs),
    /// Run the independent local desktop management process.
    Daemon(DaemonArgs),
}
#[derive(Args)]
struct ServeArgs {
    #[arg(long, env = "SWIFTCACHE_STORE")]
    store: PathBuf,
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long)]
    tcp: Option<SocketAddr>,
    #[arg(long)]
    blob_dir: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_MAX_MESSAGE_SIZE)]
    max_message_size: usize,
}
#[derive(Args)]
struct ProxyArgs {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long, env = "SWIFTCACHE_UPSTREAM")]
    upstream: String,
    /// Local disk-backed read-through cache; never shared with another service.
    #[arg(long, env = "SWIFTCACHE_STORE")]
    store: PathBuf,
    #[arg(long)]
    blob_dir: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_MAX_MESSAGE_SIZE)]
    max_message_size: usize,
}
#[derive(Args)]
struct DaemonArgs {
    /// App settings, daemon descriptor and independent history directory.
    #[arg(long, env = "SWIFTCACHE_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Management API address; loopback only, with a per-process bearer token.
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
}
fn path_string(path: PathBuf) -> String {
    path.to_string_lossy().into_owned()
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let data = match &cli.command {
        Command::Daemon(args) => args
            .data_dir
            .clone()
            .map(Ok)
            .unwrap_or_else(management::default_data_dir)?,
        _ => management::default_data_dir()?,
    };
    let data = management::prepare_data_dir(&data)?;
    let records = cli.records.unwrap_or_else(|| data.join("records"));
    let journal = Arc::new(Journal::open(&records)?);
    let filter =
        tracing_subscriber::EnvFilter::try_from_env("SWIFTCACHE_LOG").unwrap_or_else(|_| "swiftcache=info".into());
    let (filter, handle) = tracing_subscriber::reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(
            journal
                .layer()
                .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
                    meta.target().starts_with("swiftcache")
                })),
        )
        .with(tracing_subscriber::fmt::layer().with_target(false).with_filter(filter))
        .init();
    let reload: LogReload = Arc::new(move |text| {
        handle.reload(tracing_subscriber::EnvFilter::try_new(text)?)?;
        Ok(())
    });
    let mut config = Config::defaults(&data);
    config.log_level = std::env::var("SWIFTCACHE_LOG").unwrap_or_else(|_| "swiftcache=info".into());
    let result = match cli.command {
        Command::Daemon(args) => management::daemon(data, records, args.listen, journal.clone(), reload).await,
        Command::Serve(args) => {
            config.store = path_string(args.store);
            config.socket = args.socket.map(path_string);
            config.tcp = args.tcp.map(|addr| addr.to_string());
            config.blob_dir = args.blob_dir.map(path_string);
            config.max_message_size = args.max_message_size;
            run_cli(config, journal.clone()).await
        }
        Command::Proxy(args) => {
            config.mode = "proxy".into();
            config.socket = Some(path_string(args.socket));
            config.tcp = None;
            config.store = path_string(args.store);
            config.upstream = args.upstream;
            config.blob_dir = args.blob_dir.map(path_string);
            config.max_message_size = args.max_message_size;
            run_cli(config, journal.clone()).await
        }
    };
    // Log lines are queued without waiting; commit them before the process exits.
    journal.flush()?;
    result
}
async fn run_cli(config: Config, journal: Arc<Journal>) -> Result<()> {
    let service = RunningService::start(&config, journal.clone(), None).await?;
    service.wait_for_signal().await?;
    tracing::info!("total: {}", serde_json::to_string(&journal.summary()?)?);
    Ok(())
}
