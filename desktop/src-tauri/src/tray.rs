//! Menu bar (macOS status item; system tray elsewhere).
//!
//! The title next to the icon is the at-a-glance number — summed local store size of
//! the running services (Serve and Proxy alike; the upstream's size is not observable
//! over the caching protocol). The dropdown carries the
//! overview plus one submenu per service with its own start/stop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use crate::prefs::{self, Locale};
use crate::{request, DesktopState};

pub const MAIN_WINDOW: &str = "main";

const ICON: &[u8] = include_bytes!("../icons/tray/tray.png");
const STATUS_EVERY: Duration = Duration::from_secs(2);
/// `/api/usage` walks the whole store; once per poll would be wasteful.
const USAGE_EVERY: Duration = Duration::from_secs(10);

struct ServiceMenu {
    id: String,
    submenu: Submenu<Wry>,
    toggle: MenuItem<Wry>,
    /// Present only while the service reports an error.
    error: Option<MenuItem<Wry>>,
}

#[derive(Clone)]
struct Items {
    tray: TrayIcon<Wry>,
    status: MenuItem<Wry>,
    usage: MenuItem<Wry>,
    traffic: MenuItem<Wry>,
    health: MenuItem<Wry>,
    open: MenuItem<Wry>,
    settings: MenuItem<Wry>,
    quit: MenuItem<Wry>,
    /// Service submenus in daemon order; the tray menu is rebuilt only when
    /// this id list changes.
    dynamic: Arc<Mutex<Vec<ServiceMenu>>>,
    /// Last scanned (bytes, files) per store path, so the title follows
    /// start/stop immediately instead of waiting for the next usage scan.
    usage_cache: Arc<Mutex<HashMap<String, (u64, u64)>>>,
    /// Last text written per menu item key, to skip redundant native updates.
    last: Arc<Mutex<HashMap<String, String>>>,
    /// A usage scan batch is in flight.
    usage_busy: Arc<AtomicBool>,
    /// Signalled when a usage scan batch finishes.
    usage_ready: Arc<tokio::sync::Notify>,
}

pub fn install(app: &AppHandle) -> tauri::Result<()> {
    // Real text comes from `relabel` + the first `refresh`.
    let item = |id: &str, enabled: bool| MenuItem::with_id(app, id, "…", enabled, None::<&str>);
    let status = item("status", false)?;
    let usage = item("usage", false)?;
    let traffic = item("traffic", false)?;
    let health = item("health", false)?;
    let open = item("open", true)?;
    // Handled by the app-level menu handler, shared with ⌘, in the app menu.
    let settings = item(crate::SETTINGS_MENU_ID, true)?;
    let quit = item("quit", true)?;
    let menu = Menu::with_items(app, &[
        &status, &usage, &traffic, &health,
        &PredefinedMenuItem::separator(app)?,
        &open, &settings,
        &PredefinedMenuItem::separator(app)?,
        &quit,
    ])?;

    let tray = TrayIconBuilder::with_id("swiftcache")
        .icon(Image::from_bytes(ICON)?)
        .icon_as_template(true)
        .tooltip("SwiftCache")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main(app),
            "quit" => app.exit(0),
            id => {
                if let Some(service) = id.strip_prefix("toggle:") {
                    let (app, service) = (app.clone(), service.to_owned());
                    tauri::async_runtime::spawn(async move { toggle_service(&app, &service).await });
                }
            }
        })
        .build(app)?;

    let items = Items { tray, status, usage, traffic, health, open, settings, quit, dynamic: Arc::default(), usage_cache: Arc::default(), last: Arc::default(), usage_busy: Arc::default(), usage_ready: Arc::default() };
    app.manage(items.clone());
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut usage_at: Option<Instant> = None;
        loop {
            let want_usage = usage_at.is_none_or(|at| at.elapsed() >= USAGE_EVERY);
            refresh(&app, &items, want_usage).await;
            if want_usage { usage_at = Some(Instant::now()); }
            // A finished usage scan wakes the loop early so the title follows without waiting out the interval.
            let _ = tokio::time::timeout(STATUS_EVERY, items.usage_ready.notified()).await;
        }
    });
    Ok(())
}

/// Static labels plus placeholders until the next `refresh` fills them in.
pub fn relabel(app: &AppHandle, l: Locale) {
    let Some(items) = app.try_state::<Items>() else { return };
    let _ = items.open.set_text(l.pick("開啟 SwiftCache…", "Open SwiftCache…"));
    let _ = items.settings.set_text(l.pick("設定…", "Settings…"));
    let _ = items.quit.set_text(l.pick("結束介面（服務繼續執行）", "Quit window (services keep running)"));
    if items.status.text().is_ok_and(|t| t == "…") {
        let _ = items.status.set_text(l.pick("連線中…", "Connecting…"));
    }
}

pub async fn refresh_now(app: &AppHandle) {
    let Some(items) = app.try_state::<Items>() else { return };
    let items = items.inner().clone();
    refresh(app, &items, false).await;
}

pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

async fn toggle_service(app: &AppHandle, id: &str) {
    let state = app.state::<DesktopState>();
    let items = app.state::<Items>();
    {
        let dynamic = items.dynamic.lock().unwrap();
        if let Some(s) = dynamic.iter().find(|s| s.id == id) {
            let _ = s.toggle.set_enabled(false);
        }
    }
    let result = async {
        let status = request(&state, reqwest::Method::GET, "/api/status", None).await?;
        let on = status
            .get("services")
            .and_then(Value::as_array)
            .and_then(|list| list.iter().find(|s| s.get("id").and_then(Value::as_str) == Some(id)))
            .map(running)
            .unwrap_or(false);
        let action = if on { "stop" } else { "start" };
        request(&state, reqwest::Method::POST, &format!("/api/services/{id}/{action}"), None).await
    }
    .await;
    if let Err(err) = result {
        app.dialog().message(err).title("SwiftCache").kind(MessageDialogKind::Error).show(|_| {});
    }
    refresh(app, &items, false).await;
}

/// Builds the whole menu around the current service submenus.
fn build_menu(app: &AppHandle, items: &Items, services: &[ServiceMenu]) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::new(app)?;
    menu.append(&items.status)?;
    menu.append(&items.usage)?;
    menu.append(&items.traffic)?;
    menu.append(&items.health)?;
    if !services.is_empty() {
        menu.append(&PredefinedMenuItem::separator(app)?)?;
        for s in services {
            menu.append(&s.submenu)?;
        }
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&items.open)?;
    menu.append(&items.settings)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&items.quit)?;
    Ok(menu)
}

fn new_service_menu(app: &AppHandle, id: &str) -> tauri::Result<ServiceMenu> {
    let toggle = MenuItem::with_id(app, format!("toggle:{id}"), "…", true, None::<&str>)?;
    let submenu = Submenu::with_id(app, format!("service:{id}"), "…", true)?;
    submenu.append(&toggle)?;
    Ok(ServiceMenu { id: id.to_owned(), submenu, toggle, error: None })
}

fn non_empty<'a>(config: &'a Value, key: &str) -> Option<&'a str> {
    config.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn is_proxy(config: &Value) -> bool {
    config.get("mode").and_then(Value::as_str) == Some("proxy")
}

fn service_label(config: &Value, on: bool) -> String {
    let dot = if on { "●" } else { "○" };
    if let Some(name) = non_empty(config, "name").map(str::trim).filter(|name| !name.is_empty()) {
        return format!("{dot} {name}");
    }
    if is_proxy(config) {
        match non_empty(config, "upstream") {
            Some(u) => format!("{dot} Proxy → {u}"),
            None => format!("{dot} Proxy"),
        }
    } else {
        let listener = non_empty(config, "tcp").or_else(|| non_empty(config, "socket")).or_else(|| non_empty(config, "store"));
        match listener {
            Some(l) => format!("{dot} Serve · {l}"),
            None => format!("{dot} Serve"),
        }
    }
}

/// Sets a menu text only when it differs from what was last written under `key`.
fn put(items: &Items, key: &str, text: String, apply: impl FnOnce(&str) -> tauri::Result<()>) {
    let mut last = items.last.lock().unwrap();
    if last.get(key) == Some(&text) { return; }
    if apply(&text).is_ok() { last.insert(key.to_owned(), text); }
}

async fn refresh(app: &AppHandle, items: &Items, want_usage: bool) {
    let state = app.state::<DesktopState>();
    let prefs = prefs::current(app);
    let l = prefs.locale;
    let set_title = |title: &str| {
        let shown = prefs.menu_bar_title.then_some(title);
        put(items, "title", shown.unwrap_or("\0hidden").to_owned(), |_| items.tray.set_title(shown));
    };
    let status = match request(&state, reqwest::Method::GET, "/api/status", None).await {
        Ok(status) => status,
        Err(err) => {
            set_title("!");
            put(items, "status", format!("{}{err}", l.pick("無法連線管理程序：", "Cannot reach the daemon: ")), |t| items.status.set_text(t));
            for s in items.dynamic.lock().unwrap().iter() {
                let _ = s.toggle.set_enabled(false);
            }
            return;
        }
    };
    let list: Vec<&Value> = status.get("services").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let id_of = |s: &Value| s.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let total = list.len();
    let running_count = list.iter().filter(|s| running(s)).count();

    let headline = if running_count == 0 {
        l.pick("○ 服務已停止", "○ All services stopped").to_owned()
    } else {
        match l {
            Locale::ZhTw => format!("● {running_count} 個服務執行中 · 共 {total} 個"),
            Locale::En => format!("● {running_count} of {total} services running"),
        }
    };
    put(items, "status", headline, |t| items.status.set_text(t));

    // Service submenus: rebuild the tray menu only when the ordered id list changes.
    {
        let ids: Vec<String> = list.iter().map(|s| id_of(s)).collect();
        let mut dynamic = items.dynamic.lock().unwrap();
        if dynamic.iter().map(|s| s.id.as_str()).ne(ids.iter().map(String::as_str)) {
            let mut fresh = Vec::with_capacity(ids.len());
            for id in &ids {
                match new_service_menu(app, id) {
                    Ok(s) => fresh.push(s),
                    Err(_) => return,
                }
            }
            if let Ok(menu) = build_menu(app, items, &fresh) {
                let _ = items.tray.set_menu(Some(menu));
                *dynamic = fresh;
                // The new submenu items start with their creation-time text.
                items.last.lock().unwrap().retain(|k, _| !k.starts_with("svc:"));
            } else {
                return;
            }
        }
        for (s, svc) in dynamic.iter_mut().zip(&list) {
            let on = running(svc);
            let config = svc.get("config").unwrap_or(&Value::Null);
            put(items, &format!("svc:label:{}", s.id), service_label(config, on), |t| s.submenu.set_text(t));
            put(items, &format!("svc:toggle:{}", s.id), (if on { l.pick("停止服務", "Stop service") } else { l.pick("啟動服務", "Start service") }).to_owned(), |t| s.toggle.set_text(t));
            let _ = s.toggle.set_enabled(true);
            let error = svc.get("error").and_then(Value::as_str).filter(|e| !e.is_empty());
            let text = error.map(|e| format!("{}{e}", l.pick("錯誤：", "Error: ")));
            let error_key = format!("svc:error:{}", s.id);
            match (text, s.error.as_ref()) {
                (Some(text), Some(item)) => put(items, &error_key, text, |t| item.set_text(t)),
                (Some(text), None) => {
                    if let Ok(item) = MenuItem::with_id(app, format!("error:{}", s.id), &text, false, None::<&str>) {
                        if s.submenu.append(&item).is_ok() {
                            s.error = Some(item);
                            items.last.lock().unwrap().insert(error_key, text);
                        }
                    }
                }
                (None, Some(item)) => {
                    let _ = s.submenu.remove(item);
                    s.error = None;
                    items.last.lock().unwrap().remove(&error_key);
                }
                (None, None) => {}
            }
        }
    }

    let n = |key: &str| status.pointer(&format!("/summary/{key}")).and_then(Value::as_u64).unwrap_or(0);
    let (up, down, count) = (human(n("up_bytes")), human(n("down_bytes")), n("transfers"));
    put(items, "traffic", match l {
        Locale::ZhTw => format!("上傳 {up} · 下載 {down} · {count} 筆"),
        Locale::En => format!("Up {up} · Down {down} · {count} transfers"),
    }, |t| items.traffic.set_text(t));
    let (hit, miss) = (n("kv_hit"), n("kv_miss"));
    let rate = if hit + miss == 0 { "—".to_owned() } else { format!("{:.1}%", hit as f64 * 100.0 / (hit + miss) as f64) };
    let errors = n("errors");
    put(items, "health", match l {
        Locale::ZhTw => format!("KV 命中率 {rate} · 錯誤 {errors}"),
        Locale::En => format!("KV hit rate {rate} · {errors} errors"),
    }, |t| items.health.set_text(t));

    // Services with a distinct store (first one wins) — the unit usage is summed over.
    let mut stores: Vec<(String, String)> = Vec::new(); // (store, service id)
    for svc in &list {
        let config = svc.get("config").unwrap_or(&Value::Null);
        let store = non_empty(config, "store").map(str::to_owned).unwrap_or_else(|| id_of(svc));
        if !stores.iter().any(|(s, _)| *s == store) {
            stores.push((store, id_of(svc)));
        }
    }
    if want_usage && !items.usage_busy.swap(true, Ordering::AcqRel) {
        // Scans can take a long time on big stores: run them off the status loop, all
        // stores at once, then re-render the usage line and title from the new cache.
        let (app, items, stores) = (app.clone(), items.clone(), stores.clone());
        tauri::async_runtime::spawn(async move {
            let scans: Vec<_> = stores.into_iter().map(|(store, id)| {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<DesktopState>();
                    let usage = request(&state, reqwest::Method::GET, &format!("/api/usage?service={id}"), None).await.ok()?;
                    let bytes = usage.get("total_bytes").and_then(Value::as_u64).unwrap_or(0);
                    let files = usage.get("total_files").and_then(Value::as_u64).unwrap_or(0);
                    Some((store, bytes, files))
                })
            }).collect();
            for scan in scans {
                if let Ok(Some((store, bytes, files))) = scan.await {
                    items.usage_cache.lock().unwrap().insert(store, (bytes, files));
                }
            }
            items.usage_busy.store(false, Ordering::Release);
            items.usage_ready.notify_one();
        });
    }
    let cache = items.usage_cache.lock().unwrap().clone();
    let stopped = l.pick("停止", "Stopped");
    if stores.is_empty() {
        put(items, "usage", l.pick("快取用量：—", "Cache usage: —").to_owned(), |t| items.usage.set_text(t));
    } else {
        let known: Vec<(u64, u64)> = stores.iter().filter_map(|(s, _)| cache.get(s).copied()).collect();
        let (bytes, files) = known.iter().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        let usage_text = if known.is_empty() { "—".to_owned() } else { human(bytes) };
        put(items, "usage", match l {
            Locale::ZhTw => format!("快取用量：{usage_text} · {files} 個檔案"),
            Locale::En => format!("Cache usage: {usage_text} · {files} files"),
        }, |t| items.usage.set_text(t));
    }

    // Title: summed size of running services (distinct stores), else stopped.
    let mut running_stores: Vec<String> = Vec::new();
    for svc in list.iter().filter(|s| running(s)) {
        let config = svc.get("config").unwrap_or(&Value::Null);
        let store = non_empty(config, "store").map(str::to_owned).unwrap_or_else(|| id_of(svc));
        if !running_stores.contains(&store) { running_stores.push(store); }
    }
    if !running_stores.is_empty() {
        let sizes: Vec<u64> = running_stores.iter().filter_map(|s| cache.get(s).map(|c| c.0)).collect();
        set_title(&if sizes.is_empty() { "…".to_owned() } else { human(sizes.iter().sum()) });
    } else {
        set_title(stopped);
    }
}

fn running(status: &Value) -> bool {
    status.get("running").and_then(Value::as_bool).unwrap_or(false)
}

fn human(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() { v /= 1024.0; u += 1; }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}
