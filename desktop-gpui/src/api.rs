//! Client for the SwiftCache daemon's management API.
//!
//! Mirrors the Tauri shell: discovers (or launches) the detached daemon via its
//! descriptor file, checks the API revision, and sends bearer-authenticated
//! requests. All network I/O runs on a private Tokio runtime so callers can
//! await from the GPUI executor without a Tokio context.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use directories::ProjectDirs;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;
use tokio::sync::Mutex;

#[derive(Clone, Deserialize)]
struct Discovery {
    address: String,
    token: String,
    pid: u32,
    /// Missing in daemons older than the revision handshake.
    #[serde(default)]
    api: Option<u32>,
}

/// Must equal the bundled daemon's; see `server/api-revision`.
const API_REVISION: u32 = include!("../../server/api-revision");
/// No relaunch attempt within this long of a failed one; callers get the last error meanwhile.
const LAUNCH_BACKOFF: Duration = Duration::from_secs(10);

struct Inner {
    client: reqwest::Client,
    connection: Mutex<Option<Discovery>>,
    /// Held for the whole discovery/launch; holds the time and message of the last failure.
    launch: Mutex<Option<(Instant, String)>>,
}

pub struct ApiClient {
    // Option so Drop can shut the runtime down in the background.
    runtime: Option<tokio::runtime::Runtime>,
    inner: Arc<Inner>,
}

impl Drop for ApiClient {
    fn drop(&mut self) {
        // Dropping a runtime from within async context panics; shut it down without blocking.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

pub fn data_dir() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("SWIFTCACHE_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    ProjectDirs::from("dev", "yume", "SwiftCache")
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .ok_or_else(|| "找不到應用程式資料目錄".to_string())
}

/// Appends non-empty `params` to `path` as a percent-encoded query string.
pub fn query(path: &str, params: &[(&str, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    let mut any = false;
    for (key, value) in params {
        if !value.is_empty() {
            serializer.append_pair(key, value);
            any = true;
        }
    }
    if !any {
        return path.to_string();
    }
    let separator = if path.contains('?') { '&' } else { '?' };
    format!("{path}{separator}{}", serializer.finish())
}

fn descriptor(data: &Path) -> PathBuf {
    std::env::var_os("SWIFTCACHE_DAEMON_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| data.join("daemon.json"))
}

fn read_discovery(path: &Path) -> Option<Discovery> {
    let info: Discovery = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let url = reqwest::Url::parse(&info.address).ok()?;
    if url.scheme() != "http"
        || url
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
            .is_none_or(|ip| !ip.is_loopback())
    {
        return None;
    }
    Some(info)
}

async fn alive(client: &reqwest::Client, discovery: &Discovery) -> bool {
    client
        .get(format!("{}/api/status", discovery.address))
        .bearer_auth(&discovery.token)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

fn server_binary() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("SWIFTCACHE_BIN") {
        return Ok(PathBuf::from(path));
    }
    let file = format!("swiftcache{}", std::env::consts::EXE_SUFFIX);
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    if let Some(parent) = executable.parent() {
        let path = parent.join(&file);
        if path.is_file() {
            return Ok(path);
        }
    }
    // Development: the workspace's release build of the server.
    let development = Path::new(env!("CARGO_MANIFEST_DIR")).join("../server/target/release").join(&file);
    if development.is_file() {
        return Ok(development);
    }
    if let Ok(cwd) = std::env::current_dir() {
        let path = cwd.join("../server/target/release").join(&file);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("找不到 swiftcache 服務程式，請重新安裝或先建置 server（cargo build --release）".into())
}

fn launch_daemon(data: &Path) -> Result<(), String> {
    fs::create_dir_all(data).map_err(|e| e.to_string())?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data.join("daemon.log"))
        .map_err(|e| e.to_string())?;
    let mut command = Command::new(server_binary()?);
    command
        .arg("daemon")
        .arg("--data-dir")
        .arg(data)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000008 | 0x00000200);
    }
    let mut child = command.spawn().map_err(|e| format!("啟動管理程序失敗：{e}"))?;
    // Reap on daemon exit without tying daemon lifetime to the desktop window.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    // SAFETY: signal 0 only probes for existence. EPERM means it exists but belongs to someone else.
    return unsafe {
        libc::kill(pid as libc::pid_t, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };
    #[cfg(windows)]
    return Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")));
}

/// Stops a daemon from an incompatible build and waits until its process is
/// gone, so its data-dir lock is free for the replacement. On Unix it gets
/// SIGTERM and stops its services gracefully first.
async fn retire(info: &Discovery) -> Result<(), String> {
    // An already-gone process (ESRCH) is fine: the wait below sees it exit.
    #[cfg(unix)]
    // SAFETY: plain signal delivery to a pid read from the daemon descriptor.
    unsafe {
        libc::kill(info.pid as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(windows)]
    Command::new("taskkill")
        .args(["/F", "/PID"])
        .arg(info.pid.to_string())
        .status()
        .map_err(|e| format!("無法停止舊版管理程序：{e}"))?;
    for _ in 0..300 {
        if !process_exists(info.pid) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("舊版管理程序（pid {}）未在 30 秒內結束", info.pid))
}

async fn establish(client: &reqwest::Client) -> Result<Discovery, String> {
    let data = data_dir()?;
    let file = descriptor(&data);
    if let Some(info) = read_discovery(&file) {
        if alive(client, &info).await {
            if info.api == Some(API_REVISION) {
                return Ok(info);
            }
            // A daemon left running by an older build speaks an older API; the UI would break on it.
            retire(&info).await?;
        }
    }
    launch_daemon(&data)?;
    for _ in 0..100 {
        if let Some(info) = read_discovery(&file) {
            if info.api == Some(API_REVISION) && alive(client, &info).await {
                return Ok(info);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("管理程序未就緒，請查看 {}", data.join("daemon.log").display()))
}

impl Inner {
    async fn connection(&self) -> Result<Discovery, String> {
        if let Some(info) = self.connection.lock().await.clone() {
            return Ok(info);
        }
        // Single-flight: one caller discovers/launches while the rest wait here for its outcome.
        let mut launch = self.launch.lock().await;
        if let Some(info) = self.connection.lock().await.clone() {
            return Ok(info);
        }
        if let Some((at, error)) = launch.as_ref() {
            if at.elapsed() < LAUNCH_BACKOFF {
                return Err(error.clone());
            }
        }
        match establish(&self.client).await {
            Ok(info) => {
                *launch = None;
                *self.connection.lock().await = Some(info.clone());
                Ok(info)
            }
            Err(error) => {
                *launch = Some((Instant::now(), error.clone()));
                Err(error)
            }
        }
    }

    async fn request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
        let info = self.connection().await?;
        let mut request = self
            .client
            .request(method, format!("{}{}", info.address, path))
            .bearer_auth(&info.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => {
                *self.connection.lock().await = None;
                return Err(format!("管理連線失敗：{err}。請重試。"));
            }
        };
        let code = response.status();
        if code == reqwest::StatusCode::UNAUTHORIZED {
            *self.connection.lock().await = None;
        }
        let text = response.text().await.map_err(|e| e.to_string())?;
        let value: Value =
            serde_json::from_str(&text).map_err(|_| format!("無效的管理回應（{code}）：{text}"))?;
        if !code.is_success() {
            return Err(value.get("error").and_then(Value::as_str).unwrap_or(&text).to_string());
        }
        Ok(value)
    }
}

/// The (method, route) pairs the UI actually calls; anything else is refused.
fn route_allowed(method: &reqwest::Method, pathname: &str) -> bool {
    use reqwest::Method as M;
    let parts: Vec<&str> = pathname.trim_start_matches('/').split('/').collect();
    if parts.first() != Some(&"api") || parts.iter().skip(1).any(|part| part.is_empty()) {
        return false;
    }
    match (method, &parts[1..]) {
        (&M::GET, ["status" | "usage" | "transfers" | "events" | "clear-plan"]) => true,
        (&M::POST, ["services"] | ["clear"]) => true,
        (&M::POST, ["history", "clear"]) => true,
        (&M::PUT | &M::DELETE, ["services", _]) => true,
        (&M::POST, ["services", _, "start" | "stop"]) => true,
        (&M::GET, ["payload", _] | ["transfers", _] | ["transfers", _, "export"]) => true,
        _ => false,
    }
}

fn validate(method: &str, path: &str) -> Result<reqwest::Method, String> {
    let pathname = path.split('?').next().unwrap_or_default();
    if !pathname.starts_with("/api/")
        || pathname.contains('%')
        || pathname.split('/').any(|part| part == ".." || part == ".")
        || path.contains('#')
    {
        return Err("無效的管理 API 路徑".into());
    }
    let method = match method {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        _ => return Err("不支援的管理操作".into()),
    };
    if !route_allowed(&method, pathname) {
        return Err("不支援的管理操作".into());
    }
    Ok(method)
}

impl ApiClient {
    pub fn new() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("swiftcache-api")
            .enable_all()
            .build()
            .map_err(|e| format!("無法建立網路執行環境：{e}"))?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| format!("無法建立 HTTP 用戶端：{e}"))?;
        Ok(Self {
            runtime: Some(runtime),
            inner: Arc::new(Inner { client, connection: Mutex::new(None), launch: Mutex::new(None) }),
        })
    }

    /// One authenticated call to the daemon; `path` includes any query string.
    pub async fn request<T: DeserializeOwned + Send + 'static>(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, String> {
        let method = validate(method, path)?;
        let runtime = self.runtime.as_ref().ok_or_else(|| "網路執行環境已關閉".to_string())?;
        let inner = self.inner.clone();
        let path = path.to_string();
        runtime
            .spawn(async move {
                let value = inner.request(method, &path, body).await?;
                serde_json::from_value::<T>(value).map_err(|e| format!("管理回應格式不符：{e}"))
            })
            .await
            .map_err(|e| format!("管理請求中斷：{e}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_clear_requires_the_explicit_post_route() {
        assert!(route_allowed(&reqwest::Method::POST, "/api/history/clear"));
        for method in [reqwest::Method::GET, reqwest::Method::PUT, reqwest::Method::DELETE] {
            assert!(!route_allowed(&method, "/api/history/clear"));
        }
        assert!(!route_allowed(&reqwest::Method::POST, "/api/history/clear/all"));
    }

    #[test]
    fn validate_rejects_traversal_and_unknown_routes() {
        assert!(validate("GET", "/api/status").is_ok());
        assert!(validate("GET", "/api/transfers?limit=10").is_ok());
        assert!(validate("GET", "/api/../x").is_err());
        assert!(validate("GET", "/api/transfers/%2e").is_err());
        assert!(validate("PATCH", "/api/status").is_err());
        assert!(validate("DELETE", "/api/status").is_err());
    }

    #[test]
    fn query_encodes_and_skips_empty_values() {
        assert_eq!(query("/api/events", &[("a", String::new())]), "/api/events");
        assert_eq!(
            query("/api/events", &[("q", "a b&c".into()), ("x", String::new()), ("n", "1".into())]),
            "/api/events?q=a+b%26c&n=1"
        );
        assert_eq!(query("/api/x?a=1", &[("b", "2".into())]), "/api/x?a=1&b=2");
    }
}
