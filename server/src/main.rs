//! `swiftcache` — a remote compilation cache server for the Swift/Clang
//! toolchain, speaking LLVM's remote caching gRPC protocol.
//!
//! Two topologies:
//!
//! * `swiftcache serve --socket …` — single machine, compiler talks straight
//!   to the daemon over a unix socket.
//! * `swiftcache serve --tcp …` on a shared host plus
//!   `swiftcache proxy --socket … --upstream …` on each developer machine —
//!   the toolchain plugin only accepts a unix socket path, so the proxy is the
//!   local hop to a genuinely remote cache.

mod blob;
mod hash;
mod peer;
mod proto;
mod proxy;
mod service;
mod store;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use tokio::net::{TcpListener, UnixListener};
use tokio_stream::wrappers::{TcpListenerStream, UnixListenerStream};
use tonic::transport::{Channel, Server};
use tracing::info;

use crate::blob::BlobDir;
use crate::proto::cas::casdb_service_client::CasdbServiceClient;
use crate::proto::cas::casdb_service_server::CasdbServiceServer;
use crate::proto::kv::key_value_db_client::KeyValueDbClient;
use crate::proto::kv::key_value_db_server::KeyValueDbServer;
use crate::store::Store;

/// How long the traffic has to stop before the current burst of RPCs counts as
/// a finished build and gets its own summary.
const SESSION_IDLE: Duration = Duration::from_secs(2);
const SESSION_POLL: Duration = Duration::from_millis(250);

/// Compilation artifacts (object files, PCMs, swiftmodules) routinely exceed
/// gRPC's 4 MiB default.
const DEFAULT_MAX_MESSAGE_SIZE: usize = 512 * 1024 * 1024;

#[derive(Parser)]
#[command(name = "swiftcache", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the cache itself, backed by on-disk storage.
    Serve(ServeArgs),
    /// Run a unix-socket front end that forwards to a remote `serve` instance.
    Proxy(ProxyArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Directory holding the CAS objects and action cache.
    #[arg(long, env = "SWIFTCACHE_STORE")]
    store: PathBuf,

    /// Unix socket to listen on. This is the path you pass to the toolchain as
    /// `remote-service-path`.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// TCP address to listen on, for use with `swiftcache proxy`.
    #[arg(long)]
    tcp: Option<SocketAddr>,

    /// Materialise `write_to_disk` responses here instead of returning them
    /// inline. Only valid when the client shares this filesystem.
    #[arg(long)]
    blob_dir: Option<PathBuf>,

    #[arg(long, default_value_t = DEFAULT_MAX_MESSAGE_SIZE)]
    max_message_size: usize,
}

#[derive(Args)]
struct ProxyArgs {
    /// Unix socket to listen on, passed to the toolchain as
    /// `remote-service-path`.
    #[arg(long)]
    socket: PathBuf,

    /// Upstream `swiftcache serve --tcp` endpoint, e.g. `http://cache:50051`.
    #[arg(long, env = "SWIFTCACHE_UPSTREAM")]
    upstream: String,

    /// Materialise `write_to_disk` responses here instead of returning them
    /// inline.
    #[arg(long)]
    blob_dir: Option<PathBuf>,

    #[arg(long, default_value_t = DEFAULT_MAX_MESSAGE_SIZE)]
    max_message_size: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SWIFTCACHE_LOG")
                .unwrap_or_else(|_| "swiftcache=info".into()),
        )
        .with_target(false)
        .init();

    match Cli::parse().command {
        Command::Serve(args) => serve(args).await,
        Command::Proxy(args) => run_proxy(args).await,
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    if args.socket.is_none() && args.tcp.is_none() {
        bail!("pass at least one of --socket or --tcp");
    }
    let store = Arc::new(Store::open(&args.store).await?);
    info!(store = %args.store.display(), "swiftcache serve");

    let blob_dir = match &args.blob_dir {
        Some(d) => Some(Arc::new(BlobDir::new(d).await?)),
        None => None,
    };

    let make_services = || {
        let cas = service::CasService {
            store: store.clone(),
            blob_dir: blob_dir.clone(),
        };
        let kv = service::KvService {
            store: store.clone(),
        };
        (
            CasdbServiceServer::new(cas)
                .max_decoding_message_size(args.max_message_size)
                .max_encoding_message_size(args.max_message_size),
            KeyValueDbServer::new(kv)
                .max_decoding_message_size(args.max_message_size)
                .max_encoding_message_size(args.max_message_size),
        )
    };

    let mut tasks = tokio::task::JoinSet::new();
    if let Some(path) = args.socket.clone() {
        let incoming = unix_listener(&path)?;
        let (cas, kv) = make_services();
        info!(socket = %path.display(), "listening");
        tasks.spawn(async move {
            Server::builder()
                .add_service(cas)
                .add_service(kv)
                .serve_with_incoming(incoming)
                .await
                .context("unix server")
        });
    }
    if let Some(addr) = args.tcp {
        let listener = TcpListener::bind(addr).await?;
        let (cas, kv) = make_services();
        info!(tcp = %listener.local_addr()?, "listening");
        tasks.spawn(async move {
            Server::builder()
                .add_service(cas)
                .add_service(kv)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .context("tcp server")
        });
    }

    tokio::spawn(session_reporter(store.clone()));

    wait_for_shutdown(tasks, args.socket.as_deref()).await?;
    info!("total: {}", store.stats.summary());
    Ok(())
}

/// Reports one summary per burst of traffic instead of a running total.
///
/// A build run is a dense burst of RPCs followed by silence, so "counters
/// stopped moving for `SESSION_IDLE`" delimits it well enough without the
/// protocol carrying any notion of a build. Each report covers only its own
/// burst — the numbers are a delta, not the all-time totals.
async fn session_reporter(store: Arc<Store>) {
    let mut baseline = store.stats.snapshot();
    let mut last = baseline;
    let mut started = Instant::now();
    let mut last_change = Instant::now();
    let mut active = false;
    let mut session = 0u64;

    loop {
        tokio::time::sleep(SESSION_POLL).await;
        let now = store.stats.snapshot();
        if now != last {
            if !active {
                active = true;
                session += 1;
                baseline = last;
                started = Instant::now();
                info!("session #{session} started");
            }
            last = now;
            last_change = Instant::now();
        } else if active && last_change.elapsed() >= SESSION_IDLE {
            active = false;
            let took = started.elapsed() - SESSION_IDLE;
            let delta = now.delta(&baseline);
            let peers = store.stats.take_peers();
            let mut who = peers
                .iter()
                .take(4)
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            if peers.len() > 4 {
                who.push_str(&format!(", +{} more", peers.len() - 4));
            }
            info!(
                "session #{session} done in {:.1}s | {} | {} clients: {who}",
                took.as_secs_f64(),
                delta.summary(),
                peers.len(),
            );
        }
    }
}

async fn run_proxy(args: ProxyArgs) -> Result<()> {
    let channel = Channel::from_shared(args.upstream.clone())
        .with_context(|| format!("invalid --upstream {}", args.upstream))?
        .connect()
        .await
        .with_context(|| format!("connecting to {}", args.upstream))?;
    info!(upstream = %args.upstream, "swiftcache proxy");

    let blob_dir = match &args.blob_dir {
        Some(d) => Some(Arc::new(BlobDir::new(d).await?)),
        None => None,
    };

    let cas = proxy::CasProxy {
        upstream: CasdbServiceClient::new(channel.clone())
            .max_decoding_message_size(args.max_message_size)
            .max_encoding_message_size(args.max_message_size),
        blob_dir,
    };
    let kv = proxy::KvProxy {
        upstream: KeyValueDbClient::new(channel)
            .max_decoding_message_size(args.max_message_size)
            .max_encoding_message_size(args.max_message_size),
    };

    let max = args.max_message_size;
    let incoming = unix_listener(&args.socket)?;
    info!(socket = %args.socket.display(), "listening");
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        Server::builder()
            .add_service(
                CasdbServiceServer::new(cas)
                    .max_decoding_message_size(max)
                    .max_encoding_message_size(max),
            )
            .add_service(
                KeyValueDbServer::new(kv)
                    .max_decoding_message_size(max)
                    .max_encoding_message_size(max),
            )
            .serve_with_incoming(incoming)
            .await
            .context("unix server")
    });

    wait_for_shutdown(tasks, Some(&args.socket)).await
}

fn unix_listener(path: &std::path::Path) -> Result<UnixListenerStream> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // A leftover socket file from a previous run would make bind() fail with
    // EADDRINUSE even though nothing is listening.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing stale {}", path.display())),
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    Ok(UnixListenerStream::new(listener))
}

async fn wait_for_shutdown(
    mut tasks: tokio::task::JoinSet<Result<()>>,
    socket: Option<&std::path::Path>,
) -> Result<()> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // A listener task only ever finishes by failing, so the first completion
    // is the error worth reporting.
    let outcome = tokio::select! {
        _ = tokio::signal::ctrl_c() => { info!("shutting down (SIGINT)"); Ok(()) }
        _ = sigterm.recv() => { info!("shutting down (SIGTERM)"); Ok(()) }
        Some(joined) = tasks.join_next() => joined.context("listener task panicked")?,
    };
    if let Some(path) = socket {
        let _ = std::fs::remove_file(path);
    }
    outcome
}
