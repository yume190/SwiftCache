//! Transport lifecycle shared by the CLI and the independently running desktop daemon.
use std::{net::SocketAddr, path::{Path, PathBuf}, sync::Arc, time::{Duration, Instant}};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};
use tracing::Instrument;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Server};
use tracing::info;
use crate::{blob::BlobDir, journal::{Journal, Recorder}, proto::{cas::{casdb_service_client::CasdbServiceClient, casdb_service_server::CasdbServiceServer}, kv::{key_value_db_client::KeyValueDbClient, key_value_db_server::KeyValueDbServer}}, proxy, service, store::Store};

pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub name: String,
    pub mode: String,
    pub store: String,
    pub socket: Option<String>,
    pub tcp: Option<String>,
    pub upstream: String,
    pub blob_dir: Option<String>,
    pub max_message_size: usize,
    pub log_level: String,
    #[serde(default)]
    pub local_cache_paths: Vec<String>,
}

impl Config {
    pub fn defaults(data: &Path) -> Self {
        Self {
            name: String::new(),
            mode: "serve".into(), store: data.join("cache/store").to_string_lossy().into_owned(),
            socket: None, tcp: Some("127.0.0.1:50051".into()), upstream: "http://cache-host:50051".into(),
            blob_dir: None, max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
            log_level: "swiftcache=info".into(), local_cache_paths: Vec::new(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        if self.max_message_size == 0 { bail!("訊息大小上限必須大於 0"); }
        tracing_subscriber::EnvFilter::try_new(&self.log_level).context("無效的 log 層級")?;
        if self.socket.as_ref().is_some_and(|s| s.trim().is_empty()) { bail!("Socket 路徑不可為空白"); }
        if self.blob_dir.as_ref().is_some_and(|s| s.trim().is_empty()) { bail!("Blob 目錄不可為空白"); }
        match self.mode.as_str() {
            "serve" => {
                if self.store.trim().is_empty() { bail!("請設定快取目錄"); }
                if self.socket.is_none() && self.tcp.is_none() { bail!("至少啟用 TCP 或 Unix socket"); }
                if let Some(tcp) = &self.tcp { tcp.parse::<SocketAddr>().context("無效的 TCP 位址，請使用 IP:port")?; }
            }
            "proxy" => {
                if self.store.trim().is_empty() { bail!("請設定 Proxy 快取目錄"); }
                if self.socket.is_none() { bail!("Proxy 需要本機 Unix socket"); }
                Channel::from_shared(self.upstream.clone()).context("無效的 upstream URL")?;
            }
            _ => bail!("未知的服務模式"),
        }
        #[cfg(not(unix))]
        if self.socket.is_some() || self.mode == "proxy" { bail!("此平台不支援 Unix socket / Proxy；請使用 TCP Serve"); }
        Ok(())
    }
}

pub struct RunningService {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<Result<()>>,
    pub started_at: i64,
}

impl RunningService {
    /// `service_id` attributes this service's transfers, events and log lines
    /// in the journal; `None` for CLI runs.
    pub async fn start(config: &Config, journal: Arc<Journal>, service_id: Option<&str>) -> Result<Self> {
        let span = match service_id {
            Some(id) => tracing::info_span!("service", service_id = id),
            None => tracing::Span::none(),
        };
        Self::start_in(config, journal.recorder(service_id), span.clone()).instrument(span).await
    }

    async fn start_in(config: &Config, recorder: Recorder, span: tracing::Span) -> Result<Self> {
        config.validate()?;
        let (shutdown, receiver) = watch::channel(false);
        let mut tasks = tokio::task::JoinSet::new();
        let mut reporter = None;
        let max = config.max_message_size;
        let blob_dir = match &config.blob_dir {
            Some(dir) => Some(Arc::new(BlobDir::new(Path::new(dir)).await?)), None => None,
        };
        // tonic serves each request on its own task; this span carries the service id into them.
        let request_span = span.clone();
        let trace = move |_: &tonic::codegen::http::Request<()>| request_span.clone();
        if config.mode == "serve" {
            let store = Arc::new(Store::open(Path::new(&config.store)).await?);
            let services = || (
                CasdbServiceServer::new(service::CasService { store: store.clone(), blob_dir: blob_dir.clone(), journal: recorder.clone() }).max_decoding_message_size(max).max_encoding_message_size(max),
                KeyValueDbServer::new(service::KvService { store: store.clone(), journal: recorder.clone() }).max_decoding_message_size(max).max_encoding_message_size(max),
            );
            // Bind every requested transport before returning success.
            let tcp = match &config.tcp { Some(addr) => Some(TcpListener::bind(addr).await.with_context(|| format!("綁定 TCP {addr}"))?), None => None };
            #[cfg(unix)]
            if let Some(path) = &config.socket {
                let incoming = unix_listener(Path::new(path)).await?;
                let (cas, kv) = services();
                let rx = receiver.clone();
                let trace = trace.clone();
                tasks.spawn(async move { Server::builder().trace_fn(trace).add_service(cas).add_service(kv).serve_with_incoming_shutdown(incoming, stopped(rx)).await.context("unix server") }.instrument(span.clone()));
                info!(socket = %path, "listening");
            }
            if let Some(listener) = tcp {
                info!(tcp = %listener.local_addr()?, "listening");
                let (cas, kv) = services();
                let rx = receiver.clone();
                let trace = trace.clone();
                tasks.spawn(async move { Server::builder().trace_fn(trace).add_service(cas).add_service(kv).serve_with_incoming_shutdown(TcpListenerStream::new(listener), stopped(rx)).await.context("tcp server") }.instrument(span.clone()));
            }
            reporter = Some(tokio::spawn(session_reporter(store).instrument(span.clone())));
            info!(store = %config.store, "swiftcache serve started");
        } else {
            #[cfg(unix)] {
                // Lazy: a warm local cache must serve even while the upstream is unreachable; misses and writes surface the connection error.
                let channel = Channel::from_shared(config.upstream.clone()).with_context(|| format!("無效的 upstream URL：{}", config.upstream))?.connect_lazy();
                let store = Arc::new(Store::open(Path::new(&config.store)).await?);
                let incoming = unix_listener(Path::new(config.socket.as_deref().context("missing socket")?)).await?;
                let cas = proxy::CasProxy { upstream: CasdbServiceClient::new(channel.clone()).max_decoding_message_size(max).max_encoding_message_size(max), blob_dir, store: store.clone(), journal: recorder.clone() };
                let kv = proxy::KvProxy { upstream: KeyValueDbClient::new(channel).max_decoding_message_size(max).max_encoding_message_size(max), store, journal: recorder };
                let rx = receiver.clone();
                tasks.spawn(async move { Server::builder().trace_fn(trace).add_service(CasdbServiceServer::new(cas).max_decoding_message_size(max).max_encoding_message_size(max)).add_service(KeyValueDbServer::new(kv).max_decoding_message_size(max).max_encoding_message_size(max)).serve_with_incoming_shutdown(incoming, stopped(rx)).await.context("proxy server") }.instrument(span.clone()));
                info!(upstream = %config.upstream, store = %config.store, "swiftcache proxy started");
            }
        }
        let socket = config.socket.as_ref().map(PathBuf::from);
        let terminate = shutdown.clone();
        let task = tokio::spawn(async move {
            let mut result = Ok(());
            while let Some(joined) = tasks.join_next().await {
                match joined {
                    Ok(Ok(())) => {},
                    Ok(Err(err)) => { result = Err(err); let _ = terminate.send(true); },
                    Err(err) => { result = Err(err.into()); let _ = terminate.send(true); },
                }
            }
            if let Some(reporter) = reporter { reporter.abort(); let _ = reporter.await; }
            if let Some(path) = socket { let _ = tokio::fs::remove_file(path).await; }
            info!("service stopped");
            result
        }.instrument(span));
        Ok(Self { shutdown, task, started_at: now_ms() })
    }
    pub fn is_finished(&self) -> bool { self.task.is_finished() }
    pub async fn stop(self) -> Result<()> {
        let _ = self.shutdown.send(true);
        self.task.await.context("service task")?
    }
    pub async fn wait_for_signal(mut self) -> Result<()> {
        tokio::select! {
            signal = shutdown_signal() => { signal?; self.stop().await },
            result = &mut self.task => result.context("service task")?,
        }
    }
}

async fn stopped(mut receiver: watch::Receiver<bool>) {
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() { break; }
    }
}

#[cfg(unix)]
async fn unix_listener(path: &Path) -> Result<tokio_stream::wrappers::UnixListenerStream> {
    use std::os::unix::fs::FileTypeExt;
    if let Some(parent) = path.parent() { tokio::fs::create_dir_all(parent).await?; }
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => {
            if !meta.file_type().is_socket() { bail!("Socket 路徑已存在且不是 socket：{}", path.display()); }
            match tokio::net::UnixStream::connect(path).await {
                Ok(_) => bail!("已有服務使用 socket：{}", path.display()),
                Err(err) if matches!(err.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound) => { tokio::fs::remove_file(path).await?; },
                Err(err) => return Err(err).context("檢查現有 Unix socket"),
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {},
        Err(err) => return Err(err.into()),
    }
    Ok(tokio_stream::wrappers::UnixListenerStream::new(tokio::net::UnixListener::bind(path)?))
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().min(i64::MAX as u128) as i64
}

async fn session_reporter(store: Arc<Store>) {
    let mut baseline = store.stats.snapshot();
    let mut last = baseline;
    let mut started = Instant::now();
    let mut last_change = Instant::now();
    let mut active = false;
    let mut session = 0u64;
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let now = store.stats.snapshot();
        if now != last {
            if !active { active = true; session += 1; baseline = last; started = Instant::now(); info!("session #{session} started"); }
            last = now; last_change = Instant::now();
        } else if active && last_change.elapsed() >= Duration::from_secs(2) {
            active = false;
            let took = started.elapsed().saturating_sub(Duration::from_secs(2));
            let delta = now.delta(&baseline);
            let peers = store.stats.take_peers();
            let mut who = peers.iter().take(4).map(ToString::to_string).collect::<Vec<_>>().join(", ");
            if peers.len() > 4 { who.push_str(&format!(", +{} more", peers.len() - 4)); }
            info!("session #{session} done in {:.1}s | {} | {} clients: {who}", took.as_secs_f64(), delta.summary(), peers.len());
        }
    }
}

pub async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)] {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
