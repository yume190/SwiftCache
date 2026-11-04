mod prefs;
mod tray;

use std::{fs, path::{Path, PathBuf}, process::{Command, Stdio}, time::{Duration, Instant}};
use directories::ProjectDirs;
use serde::Deserialize;
use serde_json::Value;
use tauri::State;
use tauri_plugin_dialog::DialogExt;
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
const API_REVISION: u32 = include!("../../../server/api-revision");
/// No relaunch attempt within this long of a failed one; callers get the last error meanwhile.
const LAUNCH_BACKOFF: Duration = Duration::from_secs(10);
struct DesktopState {
    connection: Mutex<Option<Discovery>>,
    /// Held for the whole discovery/launch; holds the time and message of the last failure.
    launch: Mutex<Option<(Instant, String)>>,
    client: reqwest::Client,
}

fn data_dir() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("SWIFTCACHE_DATA_DIR") { return Ok(PathBuf::from(path)); }
    ProjectDirs::from("dev", "yume", "SwiftCache").map(|dirs| dirs.data_local_dir().to_path_buf()).ok_or_else(|| "找不到應用程式資料目錄".into())
}
fn descriptor(data: &Path) -> PathBuf {
    std::env::var_os("SWIFTCACHE_DAEMON_FILE").map(PathBuf::from).unwrap_or_else(|| data.join("daemon.json"))
}
fn read_discovery(path: &Path) -> Option<Discovery> {
    let info: Discovery = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let url = reqwest::Url::parse(&info.address).ok()?;
    if url.scheme() != "http" || url.host_str().and_then(|host| host.parse::<std::net::IpAddr>().ok()).is_none_or(|ip| !ip.is_loopback()) { return None; }
    Some(info)
}
async fn alive(client: &reqwest::Client, discovery: &Discovery) -> bool {
    client.get(format!("{}/api/status", discovery.address)).bearer_auth(&discovery.token).timeout(Duration::from_secs(2)).send().await.is_ok_and(|response| response.status().is_success())
}
fn server_binary() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("SWIFTCACHE_BIN") { return Ok(PathBuf::from(path)); }
    let file = format!("swiftcache{}", std::env::consts::EXE_SUFFIX);
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    if let Some(parent) = executable.parent() { let path = parent.join(&file); if path.is_file() { return Ok(path); } }
    let staged = Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries").join(format!("swiftcache-{}{}", env!("SWIFTCACHE_TARGET"), std::env::consts::EXE_SUFFIX));
    if staged.is_file() { return Ok(staged); }
    Err("找不到 swiftcache 服務程式，請重新安裝或執行 bun run prepare:server".into())
}
fn launch_daemon(data: &Path) -> Result<(), String> {
    fs::create_dir_all(data).map_err(|e| e.to_string())?;
    let log = fs::OpenOptions::new().create(true).append(true).open(data.join("daemon.log")).map_err(|e| e.to_string())?;
    let mut command = Command::new(server_binary()?);
    command.arg("daemon").arg("--data-dir").arg(data).stdin(Stdio::null()).stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?)).stderr(Stdio::from(log));
    #[cfg(unix)] { use std::os::unix::process::CommandExt; command.process_group(0); }
    #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x00000008 | 0x00000200); }
    let mut child = command.spawn().map_err(|e| format!("啟動管理程序失敗：{e}"))?;
    // Reap on daemon exit without tying daemon lifetime to the desktop window.
    std::thread::spawn(move || { let _ = child.wait(); });
    Ok(())
}

fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    // SAFETY: signal 0 only probes for existence. EPERM means it exists but belongs to someone else.
    return unsafe { libc::kill(pid as libc::pid_t, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) };
    #[cfg(windows)]
    return Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]).output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")));
}

/// Stops a daemon from an incompatible build and waits until its process is
/// gone, so its data-dir lock is free for the replacement. On Unix it gets
/// SIGTERM and stops its services gracefully first.
async fn retire(info: &Discovery) -> Result<(), String> {
    // An already-gone process (ESRCH) is fine: the wait below sees it exit.
    #[cfg(unix)]
    // SAFETY: plain signal delivery to a pid read from the daemon descriptor.
    unsafe { libc::kill(info.pid as libc::pid_t, libc::SIGTERM); }
    #[cfg(windows)]
    Command::new("taskkill").args(["/F", "/PID"]).arg(info.pid.to_string()).status().map_err(|e| format!("無法停止舊版管理程序：{e}"))?;
    for _ in 0..300 {
        if !process_exists(info.pid) { return Ok(()); }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("舊版管理程序（pid {}）未在 30 秒內結束", info.pid))
}

async fn connection(state: &DesktopState) -> Result<Discovery, String> {
    if let Some(info) = state.connection.lock().await.clone() { return Ok(info); }
    // Single-flight: one caller discovers/launches while the rest wait here for its outcome.
    let mut launch = state.launch.lock().await;
    if let Some(info) = state.connection.lock().await.clone() { return Ok(info); }
    if let Some((at, error)) = launch.as_ref() {
        if at.elapsed() < LAUNCH_BACKOFF { return Err(error.clone()); }
    }
    match establish(&state.client).await {
        Ok(info) => { *launch = None; *state.connection.lock().await = Some(info.clone()); Ok(info) }
        Err(error) => { *launch = Some((Instant::now(), error.clone())); Err(error) }
    }
}

async fn establish(client: &reqwest::Client) -> Result<Discovery, String> {
    let data = data_dir()?;
    let file = descriptor(&data);
    if let Some(info) = read_discovery(&file) {
        if alive(client, &info).await {
            if info.api == Some(API_REVISION) { return Ok(info); }
            // A daemon left running by an older build speaks an older API; the UI would break on it.
            retire(&info).await?;
        }
    }
    launch_daemon(&data)?;
    for _ in 0..100 {
        if let Some(info) = read_discovery(&file) {
            if info.api == Some(API_REVISION) && alive(client, &info).await { return Ok(info); }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("管理程序未就緒，請查看 {}", data.join("daemon.log").display()))
}

/// One authenticated call to the daemon's management API, shared by the
/// webview (`api` command) and the menu bar.
async fn request(state: &DesktopState, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
    let info = connection(state).await?;
    let mut request = state.client.request(method, format!("{}{}", info.address, path)).bearer_auth(&info.token);
    if let Some(body) = body { request = request.json(&body); }
    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => { *state.connection.lock().await = None; return Err(format!("管理連線失敗：{err}。請重試。")); }
    };
    let code = response.status();
    if code == reqwest::StatusCode::UNAUTHORIZED { *state.connection.lock().await = None; }
    let text = response.text().await.map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&text).map_err(|_| format!("無效的管理回應（{code}）：{text}"))?;
    if !code.is_success() { return Err(value.get("error").and_then(Value::as_str).unwrap_or(&text).to_string()); }
    Ok(value)
}

/// The (method, route) pairs the UI actually calls; anything else is refused so
/// a compromised webview cannot reach the rest of the management API.
fn route_allowed(method: &reqwest::Method, pathname: &str) -> bool {
    use reqwest::Method as M;
    let parts: Vec<&str> = pathname.trim_start_matches('/').split('/').collect();
    if parts.first() != Some(&"api") || parts.iter().skip(1).any(|part| part.is_empty()) { return false; }
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

#[tauri::command]
async fn api(state: State<'_, DesktopState>, method: String, path: String, body: Option<Value>) -> Result<Value, String> {
    let pathname = path.split('?').next().unwrap_or_default();
    if !pathname.starts_with("/api/") || pathname.contains('%') || pathname.split('/').any(|part| part == ".." || part == ".") || path.contains('#') { return Err("無效的管理 API 路徑".into()); }
    let method = match method.as_str() { "GET" => reqwest::Method::GET, "POST" => reqwest::Method::POST, "PUT" => reqwest::Method::PUT, "DELETE" => reqwest::Method::DELETE, _ => return Err("不支援的管理操作".into()) };
    if !route_allowed(&method, pathname) { return Err("不支援的管理操作".into()); }
    request(&state, method, &path, body).await
}

#[tauri::command(rename_all = "snake_case")]
async fn save_export(app: tauri::AppHandle, suggested_name: String, contents: String) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let selected = app.dialog().file().set_file_name(suggested_name).blocking_save_file();
        let Some(selected) = selected else { return Ok(false); };
        let path = selected.into_path().map_err(|e| e.to_string())?;
        fs::write(path, contents).map_err(|e| e.to_string())?;
        Ok(true)
    }).await.map_err(|e| e.to_string())?
}

pub(crate) const SETTINGS_MENU_ID: &str = "settings";

/// Shows the window with the preferences sheet open.
pub(crate) fn open_settings(app: &tauri::AppHandle) {
    use tauri::Emitter;
    tray::show_main(app);
    let _ = app.emit("open-settings", ());
}

#[cfg(target_os = "macos")]
struct AppMenuSettings(tauri::menu::MenuItem<tauri::Wry>);

/// The stock macOS menu plus the conventional "Settings…" (⌘,) after About.
#[cfg(target_os = "macos")]
fn app_menu(app: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, MenuItem, MenuItemKind, PredefinedMenuItem};
    use tauri::Manager;
    let menu = Menu::default(app)?;
    if let Some(MenuItemKind::Submenu(app_submenu)) = menu.items()?.into_iter().next() {
        let settings = MenuItem::with_id(app, SETTINGS_MENU_ID, "Settings…", true, Some("CmdOrCtrl+,"))?;
        app_submenu.insert(&settings, 2)?;
        app_submenu.insert(&PredefinedMenuItem::separator(app)?, 3)?;
        app.manage(AppMenuSettings(settings));
    }
    Ok(menu)
}

pub(crate) fn relabel_app_menu(app: &tauri::AppHandle, locale: prefs::Locale) {
    #[cfg(target_os = "macos")]
    {
        use tauri::Manager;
        if let Some(item) = app.try_state::<AppMenuSettings>() {
            let _ = item.0.set_text(locale.pick("設定…", "Settings…"));
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, locale);
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(DesktopState { connection: Mutex::new(None), launch: Mutex::new(None), client: reqwest::Client::builder().timeout(Duration::from_secs(120)).build().expect("HTTP client") })
        .manage(prefs::PrefsState(std::sync::Mutex::new(prefs::load())))
        .invoke_handler(tauri::generate_handler![api, save_export, prefs::get_prefs, prefs::set_prefs])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_menu(app_menu(app.handle())?)?;
            tray::install(app.handle())?;
            prefs::apply(app.handle(), &prefs::current(app.handle()));
            Ok(())
        })
        .on_menu_event(|app, event| {
            // Both the app menu (⌘,) and the menu bar item use this id.
            if event.id() == SETTINGS_MENU_ID {
                open_settings(app);
            }
        })
        .on_window_event(|window, event| {
            // macOS menu bar app: closing the window only hides it; the menu bar
            // item (and the daemon behind it) stay alive. Quit lives in the menu.
            #[cfg(target_os = "macos")]
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == tray::MAIN_WINDOW {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (window, event);
        })
        .build(tauri::generate_context!())
        .expect("無法啟動 SwiftCache 桌面程式")
        .run(|app, event| {
            // Clicking the Dock icon while the window is hidden brings it back.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { has_visible_windows: false, .. } = event {
                tray::show_main(app);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
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
}
