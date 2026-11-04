//! Local-only authenticated management API used by desktop clients.
use std::{collections::HashMap, fs::{self, OpenOptions}, io::Write, net::{IpAddr, Ipv4Addr, SocketAddr}, path::{Component, Path, PathBuf}, sync::Arc, time::{Duration, Instant}};
use anyhow::{bail, Context, Result};
use axum::{extract::{Path as RoutePath, Query, State}, http::{Request, StatusCode}, middleware::{self, Next}, response::{IntoResponse, Response}, routing::{get, post, put}, Json, Router};
use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use crate::{journal::{HistoryScope, Journal, RecordQuery}, runtime::{self, Config, RunningService}};

pub type LogReload = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;

pub fn default_data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("SWIFTCACHE_DATA_DIR") { return absolute(Path::new(&path)); }
    Ok(ProjectDirs::from("dev", "yume", "SwiftCache").context("找不到應用程式資料目錄")?.data_local_dir().to_path_buf())
}

pub fn prepare_data_dir(path: &Path) -> Result<PathBuf> {
    let path = absolute(path)?;
    fs::create_dir_all(&path)?;
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?; }
    Ok(path)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Discovery { pub address: String, pub token: String, pub pid: u32, pub api: u32 }

/// See `server/api-revision`.
pub const API_REVISION: u32 = include!("../api-revision");

/// Persisted form of one service in `services.json`.
#[derive(Serialize, Deserialize)]
struct SavedService { id: String, config: Config }

/// `busy` marks a start/stop/clear in flight: the registry lock is not held across that slow I/O,
/// so the entry itself carries the transition and rejects conflicting requests meanwhile.
struct Entry { id: String, config: Config, service: Option<RunningService>, error: Option<String>, busy: Option<&'static str> }
impl Entry {
    fn status(&self) -> ServiceStatus {
        ServiceStatus { id: self.id.clone(), config: self.config.clone(), running: self.service.is_some(), started_at: self.service.as_ref().map(|s| s.started_at), error: self.error.clone(), busy: self.busy, summary: crate::journal::Summary::default() }
    }
    /// A starting/stopping/clearing entry still owns its store, blob dir and socket.
    fn holds_resources(&self) -> bool { self.service.is_some() || self.busy.is_some() }
}

fn busy_text(busy: &str) -> &'static str {
    match busy { "starting" => "啟動中", "stopping" => "停止中", _ => "清除中" }
}

fn ensure_idle(entry: &Entry) -> std::result::Result<(), ApiError> {
    match entry.busy {
        Some(busy) => Err(ApiError(StatusCode::CONFLICT, format!("服務正在{}，請稍候", busy_text(busy)))),
        None => Ok(()),
    }
}

struct AppState {
    services: Mutex<Vec<Entry>>, journal: Arc<Journal>, data: PathBuf, records: PathBuf,
    reload: LogReload, usage: UsageCache,
}

const SERVICES_FILE: &str = "services.json";

#[derive(Serialize)]
struct ServiceStatus { id: String, config: Config, running: bool, started_at: Option<i64>, error: Option<String>, busy: Option<&'static str>, summary: crate::journal::Summary }

#[derive(Serialize)]
struct Status {
    services: Vec<ServiceStatus>, records_dir: String,
    platform: &'static str, unix_socket_supported: bool,
    summary: crate::journal::Summary,
}

struct ApiError(StatusCode, String);
impl From<anyhow::Error> for ApiError { fn from(err: anyhow::Error) -> Self { Self(StatusCode::BAD_REQUEST, format!("{err:#}")) } }
impl IntoResponse for ApiError { fn into_response(self) -> Response { (self.0, Json(json!({"error": self.1}))).into_response() } }
type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work).await.context("管理工作執行失敗")?
}

async fn authenticated(State(token): State<Arc<String>>, request: Request<axum::body::Body>, next: Next) -> Response {
    let expected = format!("Bearer {token}");
    if request.headers().get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()) != Some(expected.as_str()) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error":"管理連線驗證失敗，請重新開啟 UI"}))).into_response();
    }
    next.run(request).await
}

pub async fn daemon(data: PathBuf, records: PathBuf, listen: SocketAddr, journal: Arc<Journal>, reload: LogReload) -> Result<()> {
    if !listen.ip().is_loopback() { bail!("管理 API 只允許監聽 loopback 位址"); }
    let data = prepare_data_dir(&data)?;
    let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(data.join("daemon.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("已有 SwiftCache 管理程序使用此資料目錄")?;
    let saved = load_services(&data)?;
    (reload)(saved.first().map_or("swiftcache=info", |s| s.config.log_level.as_str()))?;
    // Expand relative paths once; preserve aliases such as /tmp in saved settings.
    let services = saved.into_iter().map(|s| Ok(Entry { id: s.id, config: resolve_paths(s.config)?, service: None, error: None, busy: None })).collect::<Result<_>>()?;
    let state = Arc::new(AppState { services: Mutex::new(services), journal, data: data.clone(), records, reload, usage: UsageCache::default() });
    let token = Arc::new(uuid::Uuid::new_v4().to_string());
    let router = Router::new()
        .route("/api/status", get(status))
        .route("/api/services", post(create_service))
        .route("/api/services/{id}", put(update_service).delete(delete_service))
        .route("/api/services/{id}/start", post(start))
        .route("/api/services/{id}/stop", post(stop))
        .route("/api/transfers", get(transfers))
        .route("/api/transfers/{id}", get(transfer))
        .route("/api/transfers/{id}/export", get(export))
        .route("/api/events", get(events))
        .route("/api/payload/{id}", get(payload))
        .route("/api/usage", get(usage))
        .route("/api/clear-plan", get(clear_plan))
        .route("/api/clear", post(clear))
        .route("/api/history/clear", post(clear_history))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .route_layer(middleware::from_fn_with_state(token.clone(), authenticated))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let discovery = Discovery { address: format!("http://{}", listener.local_addr()?), token: (*token).clone(), pid: std::process::id(), api: API_REVISION };
    write_json_private(&data.join("daemon.json"), &discovery)?;
    state.journal.event(None, "INFO", "service", "管理程序已啟動", json!({"address":discovery.address,"pid":discovery.pid}))?;
    tracing::info!(address = %discovery.address, "desktop management ready");
    let result = axum::serve(listener, router).with_graceful_shutdown(async { let _ = runtime::shutdown_signal().await; }).await;
    // Take every service out under the lock, then drain them without holding it.
    let running: Vec<RunningService> = state.services.lock().await.iter_mut().filter_map(|entry| entry.service.take()).collect();
    let mut stop_error = None;
    for service in running { if let Err(err) = service.stop().await { stop_error.get_or_insert(err); } }
    let _ = fs::remove_file(data.join("daemon.json"));
    drop(lock);
    result.context("管理 API")?;
    stop_error.map_or(Ok(()), Err)
}

/// `services.json`, created on first run from the single-service `config.json`
/// of earlier versions (or the defaults), which is then removed.
fn load_services(data: &Path) -> Result<Vec<SavedService>> {
    let (mut saved, existed) = read_services(data)?;
    // Persist the migration so each blank store keeps one stable directory.
    if migrate_blank_proxy_stores(data, &mut saved) || !existed { write_json_private(&data.join(SERVICES_FILE), &saved)?; }
    if !existed { let _ = fs::remove_file(data.join("config.json")); }
    Ok(saved)
}

fn read_services(data: &Path) -> Result<(Vec<SavedService>, bool)> {
    let file = data.join(SERVICES_FILE);
    match fs::read(&file) {
        Ok(bytes) => return Ok((serde_json::from_slice(&bytes).context("讀取服務設定")?, true)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {},
        Err(err) => return Err(err.into()),
    }
    let config = match fs::read(data.join("config.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("讀取舊版服務設定")?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Config::defaults(data),
        Err(err) => return Err(err.into()),
    };
    Ok((vec![SavedService { id: uuid::Uuid::new_v4().to_string(), config }], false))
}

/// Proxies saved before they owned a cache have a blank store. Give each its own
/// per-service directory under `data/cache`; existing paths and data are never touched.
fn migrate_blank_proxy_stores(data: &Path, services: &mut [SavedService]) -> bool {
    let mut changed = false;
    for service in services.iter_mut().filter(|s| s.config.mode == "proxy" && s.config.store.trim().is_empty()) {
        let id: String = service.id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        service.config.store = data.join("cache").join(format!("proxy-{id}")).to_string_lossy().into_owned();
        changed = true;
    }
    changed
}

fn save_services(state: &AppState, services: &[Entry]) -> Result<()> {
    let saved: Vec<SavedService> = services.iter().map(|e| SavedService { id: e.id.clone(), config: e.config.clone() }).collect();
    write_json_private(&state.data.join(SERVICES_FILE), &saved)
}

fn label(config: &Config) -> String {
    if !config.name.trim().is_empty() { return config.name.trim().to_owned(); }
    if config.mode == "proxy" { return format!("Proxy → {}", config.upstream); }
    format!("Serve · {}", config.tcp.as_deref().or(config.socket.as_deref()).unwrap_or(&config.store))
}

fn service_data(id: &str, config: &Config) -> Value { json!({"service": id, "mode": config.mode, "label": label(config)}) }

fn position(services: &[Entry], id: &str) -> std::result::Result<usize, ApiError> {
    services.iter().position(|e| e.id == id).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("找不到服務：{id}")))
}

/// Canonical form used only for resource identity and filesystem operations.
fn resolved(path: &str) -> PathBuf { absolute(Path::new(path)).unwrap_or_else(|_| PathBuf::from(path)) }

/// Equal, ancestor or descendant.
fn overlaps(a: &Path, b: &Path) -> bool { a.starts_with(b) || b.starts_with(a) }

/// Resources two running services must not share. A duplicate TCP bind or a
/// live socket would fail anyway, but a shared (or nested) store or blob dir would not, and
/// this names the service holding it. Starting/stopping/clearing services still hold theirs.
fn clash(services: &[Entry], skip: usize, config: &Config) -> Option<String> {
    let serve_dir = |c: &Config| Some(resolved(&c.store));
    let blob_dir = |c: &Config| c.blob_dir.as_deref().map(resolved);
    let (store, blob) = (serve_dir(config), blob_dir(config));
    let socket = config.socket.as_deref().map(resolved);
    services.iter().enumerate().filter(|(i, e)| *i != skip && e.holds_resources()).find_map(|(_, other)| {
        let (o, who) = (&other.config, label(&other.config));
        let (o_store, o_blob) = (serve_dir(o), blob_dir(o));
        let serve = config.mode == "serve" && o.mode == "serve";
        if serve && config.tcp.is_some() && config.tcp == o.tcp { return Some(format!("TCP {} 已由執行中的服務 {who} 使用", config.tcp.as_deref().unwrap_or_default())); }
        if let (Some(a), Some(b)) = (&store, &o_store) { if overlaps(a, b) { return Some(format!("快取目錄已由執行中的服務 {who} 使用（路徑相同或互相包含）")); } }
        if socket.is_some() && socket == o.socket.as_deref().map(resolved) { return Some(format!("Unix socket 已由執行中的服務 {who} 使用")); }
        if let (Some(a), Some(b)) = (&blob, &o_blob) { if overlaps(a, b) { return Some(format!("Blob 目錄已由執行中的服務 {who} 使用（路徑相同或互相包含）")); } }
        if [(&store, &o_blob), (&blob, &o_store)].into_iter().any(|(a, b)| matches!((a, b), (Some(a), Some(b)) if overlaps(a, b))) { return Some(format!("快取目錄與執行中的服務 {who} 的 Blob 目錄重疊")); }
        None
    })
}

/// Another holding service whose store, blob dir or socket equals, contains or lies inside a deletion target.
fn clear_clash(services: &[Entry], skip: usize, targets: &[PathBuf]) -> Option<String> {
    services.iter().enumerate().filter(|(i, e)| *i != skip && e.holds_resources()).find_map(|(_, other)| {
        let o = &other.config;
        let protected = [Some(resolved(&o.store)), o.blob_dir.as_deref().map(resolved), o.socket.as_deref().map(resolved)];
        protected.iter().flatten().find_map(|p| targets.iter().find(|t| overlaps(t, p)).map(|t| (p, t)))
            .map(|(p, t)| format!("執行中的服務 {} 使用的路徑 {} 與清除目標 {} 重疊，請先停止它", label(o), p.display(), t.display()))
    })
}

pub fn loopback_ephemeral() -> SocketAddr { SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0) }

fn write_json_private(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("tmp");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    let mut file = options.open(&temporary)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?; file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

/// Apply `apply` to the entry once slow I/O is done and clear its transitional state.
async fn settle(state: &AppState, id: &str, apply: impl FnOnce(&mut Entry)) {
    if let Some(entry) = state.services.lock().await.iter_mut().find(|e| e.id == id) { apply(entry); entry.busy = None; }
}

async fn status_value(state: &Arc<AppState>) -> Result<Status> {
    // Reap finished tasks; stop() is awaited with the registry unlocked.
    let finished: Vec<(String, Config, RunningService)> = state.services.lock().await.iter_mut()
        .filter(|e| e.busy.is_none() && e.service.as_ref().is_some_and(RunningService::is_finished))
        .filter_map(|entry| { entry.busy = Some("stopping"); entry.service.take().map(|s| (entry.id.clone(), entry.config.clone(), s)) })
        .collect();
    for (id, config, service) in finished {
        let error = service.stop().await.err().map(|err| format!("{err:#}"));
        settle(state, &id, |entry| entry.error = error.clone()).await;
        if let Some(err) = error { state.journal.event(Some(&id), "ERROR", "service", "服務意外停止", json!({"service": id, "label": label(&config), "error": err}))?; }
    }
    let mut list: Vec<ServiceStatus> = state.services.lock().await.iter().map(Entry::status).collect();
    let journal = state.journal.clone();
    let (summary, mut per_service) = blocking(move || Ok((journal.summary()?, journal.service_summaries()?))).await?;
    for item in &mut list { if let Some(s) = per_service.remove(&item.id) { item.summary = s; } }
    Ok(Status { services: list, records_dir: state.records.to_string_lossy().into_owned(), platform: std::env::consts::OS, unix_socket_supported: cfg!(unix), summary })
}
async fn status(State(state): State<Arc<AppState>>) -> ApiResult<Status> { Ok(Json(status_value(&state).await?)) }

fn resolve_paths(mut config: Config) -> Result<Config> {
    let trimmed = config.name.trim();
    if trimmed.len() != config.name.len() { config.name = trimmed.to_owned(); }
    config.store = lexical_absolute(Path::new(&config.store))?.to_string_lossy().into_owned();
    for value in [&mut config.socket, &mut config.blob_dir].into_iter().flatten() {
        *value = lexical_absolute(Path::new(value))?.to_string_lossy().into_owned();
    }
    for path in &mut config.local_cache_paths { *path = lexical_absolute(Path::new(path))?.to_string_lossy().into_owned(); }
    Ok(config)
}

fn normalize(config: Config, state: &AppState) -> Result<Config> {
    config.validate()?;
    let config = resolve_paths(config)?;
    validate_layout(&config, &state.records, &state.data)?;
    Ok(config)
}

async fn create_service(State(state): State<Arc<AppState>>, Json(config): Json<Config>) -> ApiResult<ServiceStatus> {
    let config = normalize(config, &state)?;
    let mut services = state.services.lock().await;
    services.push(Entry { id: uuid::Uuid::new_v4().to_string(), config, service: None, error: None, busy: None });
    if let Err(err) = save_services(&state, &services) { services.pop(); return Err(err.into()); }
    let created = services.last().map(Entry::status).context("新增服務")?;
    state.journal.event(Some(&created.id), "INFO", "service", "已新增服務", service_data(&created.id, &created.config))?;
    Ok(Json(created))
}

async fn update_service(State(state): State<Arc<AppState>>, RoutePath(id): RoutePath<String>, Json(config): Json<Config>) -> ApiResult<ServiceStatus> {
    let config = normalize(config, &state)?;
    let mut services = state.services.lock().await;
    let i = position(&services, &id)?;
    ensure_idle(&services[i])?;
    if config.mode != services[i].config.mode { return Err(ApiError(StatusCode::CONFLICT, "連線類型在建立後不能修改，請新增另一筆連線".into())); }
    if services[i].service.is_some() { return Err(ApiError(StatusCode::CONFLICT, "請先停止服務再修改設定".into())); }
    let previous = std::mem::replace(&mut services[i].config, config);
    if let Err(err) = save_services(&state, &services) { services[i].config = previous; return Err(err.into()); }
    services[i].error = None;
    state.journal.event(Some(&id), "INFO", "service", "已儲存服務設定", service_data(&id, &services[i].config))?;
    Ok(Json(services[i].status()))
}

async fn delete_service(State(state): State<Arc<AppState>>, RoutePath(id): RoutePath<String>) -> ApiResult<Value> {
    let mut services = state.services.lock().await;
    let i = position(&services, &id)?;
    ensure_idle(&services[i])?;
    if services[i].service.is_some() { return Err(ApiError(StatusCode::CONFLICT, "請先停止服務再移除".into())); }
    let removed = services.remove(i);
    if let Err(err) = save_services(&state, &services) { services.insert(i, removed); return Err(err.into()); }
    state.usage.forget(&resolved(&removed.config.store));
    state.journal.event(Some(&id), "INFO", "service", "已移除服務", service_data(&id, &removed.config))?;
    Ok(Json(json!({})))
}

async fn start(State(state): State<Arc<AppState>>, RoutePath(id): RoutePath<String>) -> ApiResult<Status> {
    tokio::spawn(start_owned(state, id)).await.map_err(|err| ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
}
async fn start_owned(state: Arc<AppState>, id: String) -> ApiResult<Status> {
    let config = {
        let mut services = state.services.lock().await;
        let i = position(&services, &id)?;
        ensure_idle(&services[i])?;
        if services[i].service.is_some() { return Err(ApiError(StatusCode::CONFLICT, "服務已在執行".into())); }
        if let Some(reason) = clash(&services, i, &services[i].config) { return Err(ApiError(StatusCode::CONFLICT, reason)); }
        let config = services[i].config.clone();
        validate_layout(&config, &state.records, &state.data)?;
        // The text log filter is process-wide; the most recently started service sets it.
        (state.reload)(&config.log_level)?;
        services[i].busy = Some("starting");
        config
    };
    match RunningService::start(&config, state.journal.clone(), Some(&id)).await {
        Ok(service) => {
            settle(&state, &id, |entry| { entry.service = Some(service); entry.error = None; }).await;
            state.journal.event(Some(&id), "INFO", "service", "服務已啟動", service_data(&id, &config))?;
        },
        Err(err) => {
            let message = format!("{err:#}");
            settle(&state, &id, |entry| entry.error = Some(message.clone())).await;
            let mut data = service_data(&id, &config); data["error"] = json!(message);
            state.journal.event(Some(&id), "ERROR", "service", "服務啟動失敗", data)?;
            return Err(err.into());
        },
    }
    Ok(Json(status_value(&state).await?))
}
async fn stop(State(state): State<Arc<AppState>>, RoutePath(id): RoutePath<String>) -> ApiResult<Status> {
    tokio::spawn(stop_owned(state, id)).await.map_err(|err| ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
}
async fn stop_owned(state: Arc<AppState>, id: String) -> ApiResult<Status> {
    let (config, running) = {
        let mut services = state.services.lock().await;
        let i = position(&services, &id)?;
        ensure_idle(&services[i])?;
        let entry = &mut services[i];
        let running = entry.service.take();
        if running.is_some() { entry.busy = Some("stopping"); }
        (entry.config.clone(), running)
    };
    if let Some(service) = running {
        if let Err(err) = service.stop().await {
            settle(&state, &id, |entry| entry.error = Some(format!("{err:#}"))).await;
            return Err(err.into());
        }
        settle(&state, &id, |_| {}).await;
    }
    state.journal.event(Some(&id), "INFO", "service", "服務已停止，處理中的請求已完成", service_data(&id, &config))?;
    Ok(Json(status_value(&state).await?))
}
async fn transfers(State(state): State<Arc<AppState>>, Query(query): Query<RecordQuery>) -> ApiResult<crate::journal::Page<crate::journal::Transfer>> { let journal = state.journal.clone(); Ok(Json(blocking(move || journal.transfers(&query)).await?)) }
async fn transfer(State(state): State<Arc<AppState>>, RoutePath(id): RoutePath<i64>) -> ApiResult<crate::journal::Transfer> {
    let journal = state.journal.clone();
    let item = blocking(move || journal.transfer(id)).await?.ok_or_else(|| ApiError(StatusCode::NOT_FOUND,"找不到此筆傳輸".into()))?;
    Ok(Json(item))
}
async fn export(State(state): State<Arc<AppState>>, RoutePath(id): RoutePath<i64>) -> ApiResult<Value> { let journal = state.journal.clone(); Ok(Json(blocking(move || journal.export(id)).await?)) }
async fn events(State(state): State<Arc<AppState>>, Query(query): Query<RecordQuery>) -> ApiResult<crate::journal::Page<crate::journal::Event>> { let journal = state.journal.clone(); Ok(Json(blocking(move || journal.events(&query)).await?)) }
#[derive(Deserialize)]
struct PayloadQuery { #[serde(default)] offset: u64, limit: Option<usize> }
async fn payload(State(state): State<Arc<AppState>>, RoutePath(id): RoutePath<String>, Query(query): Query<PayloadQuery>) -> ApiResult<crate::journal::PayloadChunk> { let journal = state.journal.clone(); Ok(Json(blocking(move || journal.payload(&id, query.offset, query.limit.unwrap_or(4096))).await?)) }

#[derive(Default, Clone, Serialize)]
pub struct DirectoryUsage { pub bytes: u64, pub files: u64 }
#[derive(Serialize)]
pub struct Usage {
    pub objects: DirectoryUsage, pub kv: DirectoryUsage, pub tmp: DirectoryUsage,
    pub total_bytes: u64, pub total_files: u64, pub records: DirectoryUsage,
    pub disk_available: u64, pub disk_total: u64, pub scanned_at: i64,
}
fn scan_dir(root: &Path) -> Result<DirectoryUsage> {
    let mut usage = DirectoryUsage::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = match fs::symlink_metadata(&path) { Ok(meta) => meta, Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue, Err(err) => return Err(err).with_context(|| format!("掃描 {}", path.display())) };
        if metadata.file_type().is_symlink() { continue; }
        if metadata.is_dir() { for entry in fs::read_dir(&path)? { pending.push(entry?.path()); } }
        else if metadata.is_file() {
            usage.bytes += allocated_size(&path, &metadata)?;
            usage.files += 1;
        }
    }
    Ok(usage)
}
/// On-disk size. Unix stat already reports allocated 512-byte blocks, so no
/// file needs to be opened (a store can hold hundreds of thousands).
#[cfg(unix)]
fn allocated_size(_: &Path, metadata: &fs::Metadata) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(metadata.blocks() * 512)
}
#[cfg(not(unix))]
fn allocated_size(path: &Path, _: &fs::Metadata) -> Result<u64> {
    match fs::File::open(path) {
        Ok(file) => Ok(fs2::FileExt::allocated_size(&file)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(err.into()),
    }
}
/// Size of a service's local store (serve store or proxy cache): three subtrees.
#[derive(Default, Clone)]
struct StoreUsage { objects: DirectoryUsage, kv: DirectoryUsage, tmp: DirectoryUsage }
fn scan_store(root: &Path) -> Result<StoreUsage> {
    Ok(StoreUsage { objects: scan_dir(&root.join("objects"))?, kv: scan_dir(&root.join("kv"))?, tmp: scan_dir(&root.join("tmp"))? })
}
fn disk_space(root: &Path) -> Result<(u64, u64)> {
    let mut disk = root;
    while !disk.exists() { disk = disk.parent().context("找不到所在磁碟")?; }
    Ok((fs2::available_space(disk)?, fs2::total_space(disk)?))
}

const USAGE_TTL: Duration = Duration::from_secs(15);
/// Last scan result: when, wall-clock ms, value. The async mutex is held for the whole walk so
/// concurrent requests for the same key coalesce into one scan.
type Slot<T> = Arc<Mutex<Option<(Instant, i64, T)>>>;
#[derive(Default)]
struct UsageCache { stores: std::sync::Mutex<HashMap<PathBuf, Slot<StoreUsage>>>, records: Slot<DirectoryUsage> }
impl UsageCache {
    fn store_slot(&self, store: &Path) -> Slot<StoreUsage> {
        self.stores.lock().unwrap_or_else(|e| e.into_inner()).entry(store.to_path_buf()).or_default().clone()
    }
    /// Drop the cached result; waits for a walk in progress so its stale result cannot land afterwards.
    async fn invalidate(&self, store: &Path) { *self.store_slot(store).lock().await = None; }
    fn forget(&self, store: &Path) { self.stores.lock().unwrap_or_else(|e| e.into_inner()).remove(store); }
    /// Journal files changed size; the next records usage request rescans.
    async fn invalidate_records(&self) { *self.records.lock().await = None; }
}
/// Cached value or one fresh scan. `fresh` accepts only a result produced after the request began.
async fn cached<T: Clone + Send + 'static>(slot: &Slot<T>, fresh: bool, work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<(T, i64)> {
    let requested = Instant::now();
    let mut guard = slot.lock().await;
    if let Some((at, ms, value)) = guard.as_ref() {
        if if fresh { *at >= requested } else { at.elapsed() < USAGE_TTL } { return Ok((value.clone(), *ms)); }
    }
    let value = blocking(work).await?;
    let ms = runtime::now_ms();
    *guard = Some((Instant::now(), ms, value.clone()));
    Ok((value, ms))
}
async fn usage_of(state: &AppState, config: &Config, fresh: bool) -> Result<Usage> {
    let records_dir = state.records.clone();
    let (records, records_at) = cached(&state.usage.records, fresh, move || scan_dir(&records_dir)).await?;
    let root = resolved(&config.store);
    let (slot, dir) = (state.usage.store_slot(&root), root.clone());
    let (store, store_at) = cached(&slot, fresh, move || scan_store(&dir)).await?;
    let (disk_available, disk_total) = blocking(move || disk_space(&root)).await?;
    let StoreUsage { objects, kv, tmp } = store;
    Ok(Usage { total_bytes: objects.bytes + kv.bytes + tmp.bytes, total_files: objects.files + kv.files + tmp.files, objects, kv, tmp, records, disk_available, disk_total, scanned_at: records_at.min(store_at) })
}
#[derive(Deserialize)]
struct ServiceQuery { service: String }
#[derive(Deserialize)]
struct UsageQuery { service: String, fresh: Option<String> }
async fn service_config(state: &AppState, id: &str) -> std::result::Result<Config, ApiError> {
    let services = state.services.lock().await;
    Ok(services[position(&services, id)?].config.clone())
}
async fn usage(State(state): State<Arc<AppState>>, Query(query): Query<UsageQuery>) -> ApiResult<Usage> {
    let config = service_config(&state, &query.service).await?;
    Ok(Json(usage_of(&state, &config, matches!(query.fresh.as_deref(), Some("1" | "true"))).await?))
}

fn lexical_absolute(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() { bail!("路徑不可為空白"); }
    let expanded = if let Ok(tail) = path.strip_prefix("~") { BaseDirs::new().context("找不到家目錄")?.home_dir().join(tail) } else { path.to_path_buf() };
    let path = if expanded.is_absolute() { expanded } else { std::env::current_dir()?.join(expanded) };
    let mut clean = PathBuf::new();
    for component in path.components() { match component { Component::ParentDir => { clean.pop(); }, Component::CurDir => {}, _ => clean.push(component.as_os_str()) } }
    Ok(clean)
}
fn absolute(path: &Path) -> Result<PathBuf> {
    let clean = lexical_absolute(path)?;
    // Resolve existing ancestors so aliases cannot hide overlap with the records directory.
    let mut ancestor = clean.as_path(); let mut suffix = Vec::new();
    while !ancestor.exists() { suffix.push(ancestor.file_name().context("無效的路徑")?.to_os_string()); ancestor = ancestor.parent().context("無效的路徑")?; }
    let mut resolved = fs::canonicalize(ancestor)?;
    for part in suffix.iter().rev() { resolved.push(part); }
    Ok(resolved)
}
fn validate_target(path: &Path, records: &Path, data: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) { bail!("清除目標不可為符號連結：{}", path.display()); }
    let path = absolute(path)?; let records = absolute(records)?; let data = absolute(data)?;
    let home = BaseDirs::new().context("找不到家目錄")?.home_dir().to_path_buf();
    let home = absolute(&home)?;
    if path.parent().is_none() || path == home || home.starts_with(&path) || records.starts_with(&path) || path.starts_with(&records) || data.starts_with(&path) {
        bail!("拒絕清除受保護的路徑：{}", path.display());
    }
    Ok(())
}
fn validate_layout(config: &Config, records: &Path, data: &Path) -> Result<()> {
    validate_target(Path::new(&config.store), records, data)?;
    for path in &config.local_cache_paths { validate_target(Path::new(path), records, data)?; }
    if let Some(blob) = &config.blob_dir {
        let blob = absolute(Path::new(blob))?;
        let records = absolute(records)?;
        if blob.starts_with(&records) || records.starts_with(&blob) { bail!("Blob 目錄必須與紀錄目錄分開"); }
        let store = absolute(Path::new(&config.store))?;
        if blob.starts_with(&store) || store.starts_with(&blob) { bail!("Blob 目錄必須與 cache store 分開"); }
    }
    Ok(())
}
fn config_revision(config: &Config) -> Result<String> {
    let resolved_store = absolute(Path::new(&config.store))?;
    let resolved_local = config.local_cache_paths.iter().map(|p| absolute(Path::new(p))).collect::<Result<Vec<_>>>()?;
    Ok(blake3::hash(&serde_json::to_vec(&json!({"config": config, "store": resolved_store, "local_paths": resolved_local}))?).to_hex().to_string())
}
#[derive(Serialize)]
struct ClearPlan { store: String, paths: Vec<String>, local_paths: Vec<String>, estimated_bytes: u64, running: bool, plan_id: String }
async fn clear_plan(State(state): State<Arc<AppState>>, Query(query): Query<ServiceQuery>) -> ApiResult<ClearPlan> {
    let services = state.services.lock().await;
    let entry = &services[position(&services, &query.service)?];
    let config = entry.config.clone(); let running = entry.service.is_some(); drop(services);
    let store = config.store.clone(); let local_paths = config.local_cache_paths.clone();
    let plan_id = config_revision(&config)?;
    let usage = usage_of(&state, &config, false).await?;
    Ok(Json(ClearPlan { paths: ["objects", "kv", "tmp"].iter().map(|name| Path::new(&store).join(name).to_string_lossy().into_owned()).collect(), store, local_paths, estimated_bytes: usage.total_bytes, running, plan_id }))
}
#[derive(Deserialize)]
struct ClearRequest { service: String, restart: bool, #[serde(default)] local_paths: Vec<String>, plan_id: String }
#[derive(Serialize)]
struct ClearResult { reclaimed_bytes: u64, removed_files: u64, restarted: bool, usage: Usage }
async fn clear(State(state): State<Arc<AppState>>, Json(request): Json<ClearRequest>) -> ApiResult<ClearResult> {
    tokio::spawn(clear_owned(state, request)).await.map_err(|err| ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
}
fn merged(base: &Value, extra: Value) -> Value {
    let mut data = base.clone();
    if let (Some(d), Value::Object(e)) = (data.as_object_mut(), extra) { d.extend(e); }
    data
}
async fn clear_owned(state: Arc<AppState>, request: ClearRequest) -> ApiResult<ClearResult> {
    let id = request.service.clone();
    // Validate and claim the entry under the lock; the stop, delete and restart below run unlocked.
    let (config, paths, previous) = {
        let mut services = state.services.lock().await;
        let i = position(&services, &id)?;
        ensure_idle(&services[i])?;
        let config = services[i].config.clone();
        validate_layout(&config, &state.records, &state.data)?;
        if request.plan_id != config_revision(&config)? { return Err(ApiError(StatusCode::CONFLICT, "設定或刪除路徑已變更，請重新檢視清除範圍後再確認".into())); }
        let configured = config.local_cache_paths.iter().map(|p| absolute(Path::new(p))).collect::<Result<Vec<_>>>()?;
        let mut selected = Vec::with_capacity(request.local_paths.len());
        for path in &request.local_paths {
            let path = absolute(Path::new(path))?;
            if !configured.contains(&path) { return Err(ApiError(StatusCode::BAD_REQUEST,"清除範圍包含未登記的本機路徑".into())); }
            selected.push(path);
        }
        let mut paths = ["objects", "kv", "tmp"].iter().map(|name| Path::new(&config.store).join(name)).collect::<Vec<_>>();
        for path in selected { if !paths.contains(&path) { paths.push(path); } }
        for path in &paths { validate_target(path, &state.records, &state.data)?; }
        // Only this service is stopped below; never delete anything another one is using.
        let targets = paths.iter().map(|p| absolute(p)).collect::<Result<Vec<_>>>()?;
        if let Some(reason) = clear_clash(&services, i, &targets) { return Err(ApiError(StatusCode::CONFLICT, reason)); }
        state.journal.event(Some(&id), "INFO", "service", "開始清除快取", merged(&service_data(&id, &config), json!({"paths":paths,"local_paths":request.local_paths,"phase":"started"})))?;
        let entry = &mut services[i];
        entry.busy = Some("clearing");
        (config, paths, entry.service.take())
    };
    let base = service_data(&id, &config);
    let with = |extra: Value| merged(&base, extra);
    if let Some(service) = previous {
        if let Err(err) = service.stop().await {
            let message = format!("{err:#}");
            settle(&state, &id, |entry| entry.error = Some(message.clone())).await;
            state.journal.event(Some(&id), "ERROR","cleanup","停止服務失敗，未開始刪除",with(json!({"error":message,"ok":false})))?;
            return Err(err.into());
        }
    }
    let result = blocking(move || {
        let mut reclaimed = 0; let mut files = 0;
        for path in paths {
            let usage = scan_dir(&path)?;
            match fs::remove_dir_all(&path) { Ok(()) => {}, Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}, Err(err) => return Err(err).with_context(|| format!("清除 {}", path.display())) }
            reclaimed += usage.bytes; files += usage.files;
        }
        Ok((reclaimed, files))
    }).await;
    // Cached numbers are stale whether or not the delete finished.
    state.usage.invalidate(&resolved(&config.store)).await;
    let (reclaimed_bytes, removed_files) = match result {
        Ok(value) => value,
        Err(err) => {
            let message = format!("{err:#}");
            settle(&state, &id, |entry| entry.error = Some(message.clone())).await;
            state.journal.event(Some(&id), "ERROR","cleanup","清除失敗，服務保持停止",with(json!({"ok":false,"error":message,"local_paths":request.local_paths})))?;
            return Err(err.into());
        }
    };
    let started = if request.restart { Some(RunningService::start(&config, state.journal.clone(), Some(&id)).await) } else { None };
    let (running, failure) = match started { Some(Ok(service)) => (Some(service), None), Some(Err(err)) => (None, Some(err)), None => (None, None) };
    let restarted = running.is_some();
    let failure_text = failure.as_ref().map(|err| format!("{err:#}"));
    settle(&state, &id, |entry| { entry.service = running; entry.error = failure_text.clone(); }).await;
    if let (Some(err), Some(message)) = (failure, failure_text) {
        state.journal.event(Some(&id), "ERROR","cleanup","快取已清除，但服務重啟失敗",with(json!({"ok":false,"error":message,"reclaimed_bytes":reclaimed_bytes,"removed_files":removed_files,"restarted":false})))?;
        return Err(err.into());
    }
    state.journal.event(Some(&id), "INFO","cleanup","快取清除完成",with(json!({"ok":true,"reclaimed_bytes":reclaimed_bytes,"removed_files":removed_files,"restarted":restarted,"local_paths":request.local_paths})))?;
    let usage = usage_of(&state, &config, true).await?;
    Ok(Json(ClearResult { reclaimed_bytes, removed_files, restarted, usage }))
}

#[derive(Deserialize)]
struct HistoryClearRequest { service: String, scope: String }
async fn clear_history(State(state): State<Arc<AppState>>, Json(request): Json<HistoryClearRequest>) -> ApiResult<Value> {
    let scope = match request.scope.as_str() {
        "transfers" => HistoryScope::Transfers,
        "events" => HistoryScope::Events,
        other => return Err(ApiError(StatusCode::BAD_REQUEST, format!("不支援的清除範圍：{other}"))),
    };
    tokio::spawn(clear_history_owned(state, request.service, scope)).await.map_err(|err| ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
}
/// Runs detached from the request so a dropped connection cannot leave the entry busy. The
/// deletion itself is serialized with recording inside the journal writer; the service keeps running.
async fn clear_history_owned(state: Arc<AppState>, id: String, scope: HistoryScope) -> ApiResult<Value> {
    {
        let mut services = state.services.lock().await;
        let i = position(&services, &id)?;
        ensure_idle(&services[i])?;
        services[i].busy = Some("clearing");
    }
    let journal = state.journal.clone();
    let target = id.clone();
    let result = blocking(move || journal.clear_history(&target, scope)).await;
    state.usage.invalidate_records().await;
    settle(&state, &id, |_| {}).await;
    result?;
    Ok(Json(json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A service that holds its resources (starting counts, like running).
    fn holding(id: &str, store: &str, blob: Option<&str>, local: &[&str]) -> Entry {
        let mut config = Config::defaults(Path::new("/unused"));
        config.store = store.into();
        // Socket only (unique per id), so the store/blob checks are what decide.
        config.tcp = None;
        config.socket = Some(format!("/tmp/swiftcache-test-{id}.sock"));
        config.blob_dir = blob.map(Into::into);
        config.local_cache_paths = local.iter().map(|p| p.to_string()).collect();
        Entry { id: id.into(), config, service: None, error: None, busy: Some("starting") }
    }

    fn targets(store: &str, local: &[&str]) -> Vec<PathBuf> {
        let mut t: Vec<PathBuf> = ["objects", "kv", "tmp"].iter().map(|n| Path::new(store).join(n)).collect();
        t.extend(local.iter().map(PathBuf::from));
        t
    }

    #[test]
    fn clear_refuses_local_path_that_is_another_live_store() {
        let services = [holding("a", "/cache/a", None, &["/cache/b"]), holding("b", "/cache/b", None, &[])];
        assert!(clear_clash(&services, 0, &targets("/cache/a", &["/cache/b"])).is_some());
        assert!(clear_clash(&services, 0, &targets("/cache/a", &[])).is_none());
    }

    #[test]
    fn clear_refuses_targets_nested_in_or_around_live_resources() {
        // Other service's store lives inside our objects dir; its blob dir contains our store.
        let inner = [holding("a", "/cache/a", None, &[]), holding("b", "/cache/a/objects/b", None, &[])];
        assert!(clear_clash(&inner, 0, &targets("/cache/a", &[])).is_some());
        let outer = [holding("a", "/cache/a", None, &[]), holding("b", "/other", Some("/cache"), &[])];
        assert!(clear_clash(&outer, 0, &targets("/cache/a", &[])).is_some());
    }

    #[test]
    fn idle_services_do_not_block_clear() {
        let mut b = holding("b", "/cache/b", None, &[]);
        b.busy = None;
        let services = [holding("a", "/cache/a", None, &["/cache/b"]), b];
        assert!(clear_clash(&services, 0, &targets("/cache/a", &["/cache/b"])).is_none());
    }

    #[test]
    fn start_refuses_nested_stores() {
        let services = [holding("a", "/cache", None, &[]), holding("b", "/cache/b", None, &[])];
        assert!(clash(&services, 1, &services[1].config).is_some());
        let disjoint = [holding("a", "/cache/a", None, &[]), holding("b", "/cache/ab", None, &[])];
        assert!(clash(&disjoint, 1, &disjoint[1].config).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn start_refuses_socket_aliases() {
        let root = std::env::temp_dir().join(format!("swiftcache-socket-alias-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        let mut services = [holding("a", "/cache/a", None, &[]), holding("b", "/cache/b", None, &[])];
        services[0].config.socket = Some(root.join("real/cache.sock").to_string_lossy().into_owned());
        services[1].config.socket = Some(root.join("alias/cache.sock").to_string_lossy().into_owned());
        let conflict = clash(&services, 1, &services[1].config);
        fs::remove_dir_all(root).unwrap();
        assert!(conflict.is_some());
    }

    fn proxy(store: &str, blob: Option<&str>) -> Config {
        let mut config = Config::defaults(Path::new("/unused"));
        config.mode = "proxy".into();
        config.store = store.into();
        config.tcp = None;
        config.socket = Some("/tmp/swiftcache-test-proxy.sock".into());
        config.blob_dir = blob.map(Into::into);
        config
    }

    #[cfg(unix)]
    #[test]
    fn proxy_requires_a_store_and_checks_its_layout() {
        let (records, data) = (Path::new("/tmp/swiftcache-test-records"), Path::new("/tmp/swiftcache-test-data"));
        assert!(proxy("", None).validate().is_err());
        assert!(proxy("   ", None).validate().is_err());
        let config = proxy("/tmp/swiftcache-test-proxy-store", Some("/tmp/swiftcache-test-blob"));
        config.validate().unwrap();
        let config = resolve_paths(config).unwrap();
        validate_layout(&config, records, data).unwrap();
        // The proxy store gets the same protection as a serve store.
        assert!(validate_layout(&proxy("/tmp/swiftcache-test-records/store", None), records, data).is_err());
        assert!(validate_layout(&proxy("/tmp/swiftcache-test-data", None), records, data).is_err());
        // Blob dir may not overlap the records dir or the proxy store.
        assert!(validate_layout(&proxy("/tmp/swiftcache-test-proxy-store", Some("/tmp/swiftcache-test-records/blob")), records, data).is_err());
        assert!(validate_layout(&proxy("/tmp/swiftcache-test-proxy-store", Some("/tmp/swiftcache-test-proxy-store/blob")), records, data).is_err());
    }

    #[test]
    fn proxy_store_is_isolated_like_a_serve_store() {
        let mut proxy_entry = holding("p", "/cache/p", None, &[]);
        proxy_entry.config.mode = "proxy".into();
        let mut same = holding("s", "/cache/p/objects/sub", None, &[]);
        same.config.mode = "serve".into();
        let services = [proxy_entry, same];
        assert!(clash(&services, 1, &services[1].config).is_some());
        assert!(clash(&services, 0, &services[0].config).is_some());
        // Clearing a proxy's cache must not touch a live serve store inside it.
        assert!(clear_clash(&services, 0, &targets("/cache/p", &[])).is_some());
        let mut other = holding("o", "/cache/other", None, &[]);
        other.config.mode = "proxy".into();
        let disjoint = [services[0].config.clone(), other.config.clone()].map(|config| Entry { id: "x".into(), config, service: None, error: None, busy: Some("starting") });
        assert!(clash(&disjoint, 1, &disjoint[1].config).is_none());
    }

    fn saved(id: &str, mode: &str, store: &str) -> SavedService {
        let mut config = Config::defaults(Path::new("/unused"));
        config.mode = mode.into();
        config.store = store.into();
        SavedService { id: id.into(), config }
    }

    #[test]
    fn legacy_blank_proxy_stores_get_distinct_stable_local_dirs() {
        let data = Path::new("/data");
        let mut services = vec![saved("a", "proxy", ""), saved("b", "proxy", "  "), saved("c", "proxy", "/tmp/keep"), saved("d", "serve", "/cache/d")];
        assert!(migrate_blank_proxy_stores(data, &mut services));
        assert_eq!(services[0].config.store, "/data/cache/proxy-a");
        assert_eq!(services[1].config.store, "/data/cache/proxy-b");
        assert_eq!(services[2].config.store, "/tmp/keep", "existing paths keep their spelling");
        assert_eq!(services[3].config.store, "/cache/d");
        // Idempotent: a second pass changes nothing, so the assignment is stable.
        assert!(!migrate_blank_proxy_stores(data, &mut services));
        assert_eq!(services[0].config.store, "/data/cache/proxy-a");
    }

    #[test]
    fn legacy_blank_proxy_migration_is_persisted() {
        let data = std::env::temp_dir().join(format!("swiftcache-migrate-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&data).unwrap();
        write_json_private(&data.join(SERVICES_FILE), &[saved("a", "proxy", "")]).unwrap();
        let first = load_services(&data).unwrap();
        let second = load_services(&data).unwrap();
        let on_disk: Vec<SavedService> = serde_json::from_slice(&fs::read(data.join(SERVICES_FILE)).unwrap()).unwrap();
        let expected = data.join("cache/proxy-a").to_string_lossy().into_owned();
        fs::remove_dir_all(&data).unwrap();
        assert_eq!(first[0].config.store, expected);
        assert_eq!(second[0].config.store, expected);
        assert_eq!(on_disk[0].config.store, expected);
    }
}
