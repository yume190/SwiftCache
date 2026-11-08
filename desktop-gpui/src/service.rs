//! Service (connection) editor: draft state, validation, CLI preview and save/create/start.
//!
//! The editable widgets keep their text in retained `InputState` entities; everything else
//! (mode, listen scope, host choices, units, toggles) lives in [`Parts`]. A draft is turned into a
//! [`Config`] with [`Parts::compose`], which keeps the original spelling of any value the user
//! did not touch (scheme, path, IPv6, sizes that are not whole units, ...).

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::radio::Radio;
use gpui_kit::component::{ActiveTheme, Disableable, Selectable, Sizable, h_flex, v_flex};
use gpui_kit::*;

use crate::app::Workspace;
use crate::types::{Config, ServiceStatus, Status};

const MIB: usize = 1024 * 1024;
const GIB: usize = 1024 * MIB;
const DEFAULT_TCP: &str = "127.0.0.1:50051";
const DEFAULT_BLOB_DIR: &str = "/tmp/swiftcache/blobs";
const LOG_LEVELS: [&str; 6] = [
    "swiftcache=info",
    "swiftcache=debug",
    "swiftcache=trace",
    "swiftcache=warn",
    "info",
    "debug",
];

// ---------------------------------------------------------------------------------------------
// Pure draft model
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SizeUnit {
    Mib,
    Gib,
}

impl SizeUnit {
    fn bytes(self) -> usize {
        match self {
            SizeUnit::Mib => MIB,
            SizeUnit::Gib => GIB,
        }
    }
    fn label(self) -> &'static str {
        match self {
            SizeUnit::Mib => "MiB",
            SizeUnit::Gib => "GiB",
        }
    }
    /// Largest unit that shows the value as a whole number (512 MiB, 2 GiB).
    fn fitting(bytes: usize) -> Self {
        if bytes > 0 && bytes.is_multiple_of(GIB) {
            SizeUnit::Gib
        } else {
            SizeUnit::Mib
        }
    }
}

fn size_text(bytes: usize, unit: SizeUnit) -> String {
    if bytes.is_multiple_of(unit.bytes()) {
        (bytes / unit.bytes()).to_string()
    } else {
        format!("{}", bytes as f64 / unit.bytes() as f64)
    }
}

/// Splits `host:port` at the last colon; a value without colon is all host.
fn split_tcp(address: &str) -> (String, String) {
    match address.rfind(':') {
        Some(i) => (address[..i].to_string(), address[i + 1..].to_string()),
        None => (address.to_string(), String::new()),
    }
}

struct UpstreamParts {
    prefix: String,
    host: String,
    port: String,
    suffix: String,
}

/// `http(s)://host[:port][/path…]`, keeping the scheme spelling, `[IPv6]` brackets and the tail.
fn split_upstream(address: &str) -> UpstreamParts {
    let lower = address.to_ascii_lowercase();
    let scheme_len = if lower.starts_with("https://") {
        8
    } else if lower.starts_with("http://") {
        7
    } else {
        return UpstreamParts {
            prefix: "http://".into(),
            host: address.into(),
            port: "50051".into(),
            suffix: String::new(),
        };
    };
    let prefix = &address[..scheme_len];
    let rest = &address[scheme_len..];
    let host_end = if rest.starts_with('[') {
        match rest.find(']') {
            Some(i) => i + 1,
            None => rest.find([':', '/', '?', '#']).unwrap_or(rest.len()),
        }
    } else {
        rest.find([':', '/', '?', '#']).unwrap_or(rest.len())
    };
    let host = &rest[..host_end];
    let after = &rest[host_end..];
    let (port, suffix) = match after.strip_prefix(':') {
        Some(p) => {
            let end = p.find(['/', '?', '#']).unwrap_or(p.len());
            (p[..end].to_string(), p[end..].to_string())
        }
        None => (
            if prefix.eq_ignore_ascii_case("https://") {
                "443"
            } else {
                "80"
            }
            .to_string(),
            after.to_string(),
        ),
    };
    UpstreamParts {
        prefix: prefix.into(),
        host: host.into(),
        port,
        suffix,
    }
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn port_in(s: &str, min: u64, max: u64) -> bool {
    digits(s) && s.parse::<u64>().map(|n| n >= min && n <= max).unwrap_or(false)
}

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_/.:=@%+,-".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Equivalent `swiftcache serve|proxy` invocation, one flag per line.
pub fn build_cli(c: &Config) -> String {
    let env = if c.log_level.is_empty() {
        String::new()
    } else {
        format!("SWIFTCACHE_LOG={} ", shell_quote(&c.log_level))
    };
    let mut lines = vec![format!("{env}swiftcache {}", c.mode)];
    lines.push(format!("--store {}", shell_quote(&c.store)));
    if c.mode == "serve"
        && let Some(tcp) = c.tcp.as_deref().filter(|v| !v.is_empty())
    {
        lines.push(format!("--tcp {}", shell_quote(tcp)));
    }
    if let Some(socket) = c.socket.as_deref().filter(|v| !v.is_empty()) {
        lines.push(format!("--socket {}", shell_quote(socket)));
    }
    if c.mode == "proxy" {
        lines.push(format!("--upstream {}", shell_quote(&c.upstream)));
    }
    if let Some(blob) = c.blob_dir.as_deref().filter(|v| !v.is_empty()) {
        lines.push(format!("--blob-dir {}", shell_quote(blob)));
    }
    lines.push(format!("--max-message-size {}", c.max_message_size));
    lines.join(" \\\n  ")
}

fn normalize(c: Config) -> Config {
    Config {
        name: c.name.trim().to_string(),
        store: c.store.trim().to_string(),
        upstream: c.upstream.trim().to_string(),
        tcp: c.tcp.map(|v| v.trim().to_string()),
        socket: c.socket.map(|v| v.trim().to_string()),
        blob_dir: c.blob_dir.map(|v| v.trim().to_string()),
        log_level: c.log_level.trim().to_string(),
        local_cache_paths: c
            .local_cache_paths
            .iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        ..c
    }
}

/// Name a new draft takes when its role changes. Only a name still generated for this draft
/// (`Server-<id>` / `Proxy-<id>`, with the `id` from `generated`) follows the role; custom names stay.
fn role_default_name(generated: &str, current: &str, from_serve: bool, to_serve: bool) -> Option<String> {
    let id = generated.strip_prefix("Server-")?;
    let role = if from_serve { "Server-" } else { "Proxy-" };
    (current.strip_prefix(role) == Some(id)).then(|| format!("{}-{id}", if to_serve { "Server" } else { "Proxy" }))
}

/// Everything a draft is made of. The `String` fields at the bottom are mirrored from the input
/// widgets; the rest is owned here.
#[derive(Clone, Debug)]
struct Parts {
    mode: String,
    unix_ok: bool,
    // TCP (serve)
    tcp_enabled: bool,
    tcp_host: String,
    tcp_raw: Option<String>,
    tcp_orig: (String, String),
    proxy_tcp: Option<String>,
    // upstream (proxy)
    up_local: bool,
    up_prefix: String,
    up_suffix: String,
    up_raw: String,
    up_orig: (bool, String, String),
    // size
    size_unit: SizeUnit,
    size_orig: (String, SizeUnit, usize),
    // optional paths
    blob_enabled: bool,
    socket_keep: Option<String>,
    // mirrored input text
    name: String,
    store: String,
    socket: String,
    tcp_port: String,
    up_host: String,
    up_port: String,
    size: String,
    log: String,
    blob: String,
    paths: String,
}

impl Parts {
    fn from_config(c: &Config, unix_ok: bool) -> Self {
        let tcp_src = c.tcp.clone().unwrap_or_else(|| DEFAULT_TCP.to_string());
        let (tcp_host, tcp_port) = split_tcp(&tcp_src);
        let up = split_upstream(&c.upstream);
        let up_local = up.host == "127.0.0.1";
        let up_host = if up_local { String::new() } else { up.host.clone() };
        let unit = SizeUnit::fitting(c.max_message_size);
        let size = size_text(c.max_message_size, unit);
        Parts {
            mode: c.mode.clone(),
            unix_ok,
            tcp_enabled: c.tcp.is_some(),
            tcp_host: tcp_host.clone(),
            tcp_raw: if c.mode == "serve" { c.tcp.clone() } else { None },
            tcp_orig: (tcp_host, tcp_port.clone()),
            proxy_tcp: if c.mode == "serve" { None } else { c.tcp.clone() },
            up_local,
            up_prefix: up.prefix,
            up_suffix: up.suffix,
            up_raw: c.upstream.clone(),
            up_orig: (up_local, up_host.clone(), up.port.clone()),
            size_unit: unit,
            size_orig: (size.clone(), unit, c.max_message_size),
            blob_enabled: c.blob_dir.is_some(),
            socket_keep: c.socket.clone(),
            name: c.name.clone(),
            store: c.store.clone(),
            socket: c.socket.clone().unwrap_or_default(),
            tcp_port,
            up_host,
            up_port: up.port,
            size,
            log: c.log_level.clone(),
            blob: c.blob_dir.clone().unwrap_or_default(),
            paths: c.local_cache_paths.join("\n"),
        }
    }

    fn serve(&self) -> bool {
        self.mode == "serve"
    }

    fn compose(&self) -> Config {
        let serve = self.serve();
        let tcp = if serve {
            if !self.tcp_enabled {
                None
            } else if (self.tcp_host.as_str(), self.tcp_port.as_str())
                == (self.tcp_orig.0.as_str(), self.tcp_orig.1.as_str())
                && self.tcp_raw.is_some()
            {
                self.tcp_raw.clone()
            } else {
                Some(format!("{}:{}", self.tcp_host, self.tcp_port))
            }
        } else {
            self.proxy_tcp.clone()
        };
        let upstream = if serve
            || (self.up_local, &self.up_host, &self.up_port) == (self.up_orig.0, &self.up_orig.1, &self.up_orig.2)
        {
            self.up_raw.clone()
        } else {
            let host = if self.up_local {
                "127.0.0.1"
            } else {
                self.up_host.as_str()
            };
            let authority = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]")
            } else {
                host.to_string()
            };
            format!("{}{}:{}{}", self.up_prefix, authority, self.up_port, self.up_suffix)
        };
        let max_message_size = if (self.size.as_str(), self.size_unit) == (self.size_orig.0.as_str(), self.size_orig.1)
        {
            self.size_orig.2
        } else {
            let amount = self
                .size
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .unwrap_or(0.0)
                .max(0.0)
                .round();
            (amount * self.size_unit.bytes() as f64) as usize
        };
        normalize(Config {
            name: self.name.clone(),
            mode: self.mode.clone(),
            store: self.store.clone(),
            socket: if self.unix_ok || !serve {
                Some(self.socket.clone())
            } else {
                self.socket_keep.clone()
            },
            tcp,
            upstream,
            blob_dir: self.blob_enabled.then(|| self.blob.clone()),
            max_message_size,
            log_level: self.log.clone(),
            local_cache_paths: self.paths.split('\n').map(str::to_string).collect(),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

pub struct ServiceState {
    pub is_new: bool,
    parts: Parts,
    /// Saved (server-side) config the draft is compared against; `normalize`d.
    baseline: Config,
    advanced_open: bool,
    name: Entity<InputState>,
    store: Entity<InputState>,
    socket: Entity<InputState>,
    tcp_port: Entity<InputState>,
    up_host: Entity<InputState>,
    up_port: Entity<InputState>,
    size: Entity<InputState>,
    log: Entity<InputState>,
    blob: Entity<InputState>,
    paths: Entity<TextareaState>,
    _subscriptions: Vec<Subscription>,
}

impl ServiceState {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let config = Config::default();
        let parts = Parts::from_config(&config, cfg!(unix));
        let name = cx.new(|cx| InputState::new(window, cx));
        let store = cx.new(|cx| InputState::new(window, cx));
        let socket = cx.new(|cx| InputState::new(window, cx));
        let tcp_port = cx.new(|cx| InputState::new(window, cx));
        let up_host = cx.new(|cx| InputState::new(window, cx));
        let up_port = cx.new(|cx| InputState::new(window, cx));
        let size = cx.new(|cx| InputState::new(window, cx));
        let log = cx.new(|cx| InputState::new(window, cx));
        let blob = cx.new(|cx| InputState::new(window, cx));
        let paths = cx.new(|cx| TextareaState::new(window, cx).rows(3));
        let mut subscriptions = Vec::new();
        macro_rules! watch {
            ($($entity:expr),+) => {$(
                subscriptions.push(cx.subscribe_in(&$entity, window, |_, _, event: &InputEvent, _, cx| {
                    // Only user edits emit Change; `set_value` (used for loading) is silent, so
                    // loading a service can never make the draft look dirty.
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }));
            )+};
        }
        watch!(name, store, socket, tcp_port, up_host, up_port, size, log, blob, paths);
        let state = ServiceState {
            is_new: true,
            baseline: parts.compose(),
            parts,
            advanced_open: false,
            name,
            store,
            socket,
            tcp_port,
            up_host,
            up_port,
            size,
            log,
            blob,
            paths,
            _subscriptions: subscriptions,
        };
        state.write_inputs(window, cx);
        state
    }

    fn write_inputs(&self, window: &mut Window, cx: &mut Context<Workspace>) {
        let p = &self.parts;
        let set = set_input;
        set(&self.name, &p.name, window, cx);
        set(&self.store, &p.store, window, cx);
        set(&self.socket, &p.socket, window, cx);
        set(&self.tcp_port, &p.tcp_port, window, cx);
        set(&self.up_host, &p.up_host, window, cx);
        set(&self.up_port, &p.up_port, window, cx);
        set(&self.size, &p.size, window, cx);
        set(&self.log, &p.log, window, cx);
        set(&self.blob, &p.blob, window, cx);
        let paths = SharedString::from(p.paths.clone());
        self.paths.update(cx, |s, cx| s.set_value(paths, window, cx));
    }

    /// Current draft: owned state overlaid with the live text of every input.
    fn parts_now(&self, cx: &App) -> Parts {
        let mut p = self.parts.clone();
        p.name = self.name.read(cx).value().to_string();
        p.store = self.store.read(cx).value().to_string();
        p.socket = self.socket.read(cx).value().to_string();
        p.tcp_port = self.tcp_port.read(cx).value().to_string();
        p.up_host = self.up_host.read(cx).value().to_string();
        p.up_port = self.up_port.read(cx).value().to_string();
        p.size = self.size.read(cx).value().to_string();
        p.log = self.log.read(cx).value().to_string();
        p.blob = self.blob.read(cx).value().to_string();
        p.paths = self.paths.read(cx).value().to_string();
        p
    }
}

fn set_input(entity: &Entity<InputState>, text: &str, window: &mut Window, cx: &mut Context<Workspace>) {
    let text = SharedString::from(text.to_string());
    entity.update(cx, |s, cx| s.set_value(text, window, cx));
}

fn sync_placeholder(entity: &Entity<InputState>, text: &str, window: &mut Window, cx: &mut Context<Workspace>) {
    if &**entity.read(cx).presentation().placeholder() != text {
        let text = SharedString::from(text.to_string());
        entity.update(cx, |s, cx| s.set_placeholder(text, window, cx));
    }
}

// ---------------------------------------------------------------------------------------------
// Workspace integration
// ---------------------------------------------------------------------------------------------

impl Workspace {
    fn unix_supported(&self) -> bool {
        self.status
            .as_ref()
            .map(|s| s.unix_socket_supported)
            .unwrap_or(cfg!(unix))
    }

    /// A fresh configuration for the "new connection" flow (unique store/socket, socket only
    /// where Unix sockets exist).
    pub fn new_service_config(&self) -> Config {
        let mut config = Config::default();
        if !self.unix_supported() {
            config.socket = None;
        }
        config
    }

    fn service_running(&self) -> bool {
        !self.service.is_new && self.selected_service().map(|s| s.running).unwrap_or(false)
    }

    fn service_locked(&self) -> bool {
        self.service_running()
            || self.busy
            || (!self.service.is_new && self.selected_service().map(|s| s.busy.is_some()).unwrap_or(false))
    }

    fn validate_config(&self, c: &Config) -> Vec<String> {
        let unix_ok = self.unix_supported();
        let mut errs = Vec::new();
        if c.store.trim().is_empty() {
            errs.push(
                self.t("請填寫快取目錄（--store）。", "Enter a cache directory (--store).")
                    .to_string(),
            );
        }
        if c.mode == "serve" {
            if c.tcp.is_none() && c.socket.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
                errs.push(
                    self.t(
                        "至少啟用 TCP 或 Unix socket 其中一種接收方式。",
                        "Enable at least one of TCP or Unix socket.",
                    )
                    .to_string(),
                );
            }
        } else {
            if !unix_ok {
                errs.push(
                    self.t(
                        "此平台不支援 Unix socket，無法使用 Proxy 模式。",
                        "This platform does not support Unix sockets, so Proxy mode is unavailable.",
                    )
                    .to_string(),
                );
            }
            let up = split_upstream(c.upstream.trim());
            if up.host.trim().is_empty() {
                errs.push(
                    self.t(
                        "請填寫上游位址（--upstream）。",
                        "Enter an upstream address (--upstream).",
                    )
                    .to_string(),
                );
            }
            if !port_in(&up.port, 1, 65535) {
                errs.push(
                    self.t(
                        "上游 Port 必須是 1–65535 的整數。",
                        "Upstream port must be an integer from 1 to 65535.",
                    )
                    .to_string(),
                );
            }
        }
        if c.mode == "serve"
            && let Some(tcp) = &c.tcp
        {
            if tcp.trim().is_empty() {
                errs.push(
                    self.t("TCP 位址不可為空。", "The TCP address can’t be empty.")
                        .to_string(),
                );
            } else if !port_in(&split_tcp(tcp.trim()).1, 0, 65535) {
                errs.push(
                    self.t(
                        "TCP Port 必須是 0–65535 的整數。",
                        "TCP port must be an integer from 0 to 65535.",
                    )
                    .to_string(),
                );
            }
        }
        if (unix_ok || c.mode == "proxy") && c.socket.as_deref().map(|s| s.trim().is_empty()).unwrap_or(true) {
            errs.push(
                self.t(
                    "請填寫本機 Unix socket 路徑（--socket）。",
                    "Enter a local Unix socket path (--socket).",
                )
                .to_string(),
            );
        }
        if c.blob_dir.as_deref().map(|b| b.trim().is_empty()).unwrap_or(false) {
            errs.push(
                self.t("檔案回應目錄不可為空。", "The file response directory can’t be empty.")
                    .to_string(),
            );
        }
        if c.max_message_size < MIB {
            errs.push(
                self.t(
                    "單筆訊息上限至少 1 MiB。",
                    "The maximum message size must be at least 1 MiB.",
                )
                .to_string(),
            );
        }
        errs
    }

    /// The validated, normalized config of the editor (what would be sent to the daemon).
    pub fn current_config(&self, cx: &App) -> Result<Config, String> {
        let config = self.service.parts_now(cx).compose();
        let errs = self.validate_config(&config);
        if errs.is_empty() {
            Ok(config)
        } else {
            Err(errs.join("\n"))
        }
    }

    /// True for a not-yet-created connection, or when the draft differs from the saved config.
    pub fn service_dirty(&self, cx: &App) -> bool {
        self.service.is_new || self.service.parts_now(cx).compose() != self.service.baseline
    }

    /// True when loading `config` into the editor would give the editor's current baseline.
    pub fn editor_matches(&self, config: &Config) -> bool {
        Parts::from_config(config, self.unix_supported()).compose() == self.service.baseline
    }

    pub fn can_save(&self, cx: &App) -> bool {
        self.service_dirty(cx) && !self.service_locked() && self.current_config(cx).is_ok()
    }

    /// Loads `config` into the editor (silently: no change events, so not dirty afterwards).
    pub fn load_service(&mut self, config: Config, is_new: bool, window: &mut Window, cx: &mut Context<Self>) {
        let parts = Parts::from_config(&config, self.unix_supported());
        self.service.is_new = is_new;
        self.service.baseline = parts.compose();
        self.service.parts = parts;
        self.service.advanced_open = false;
        self.service.write_inputs(window, cx);
        cx.notify();
    }

    /// Saves (creates or updates) the draft; with `start_after` the service is started once
    /// saved. On failure the draft stays untouched and the error is shown by `perform`.
    pub fn save_service(&mut self, start_after: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.service_running() {
            self.error = Some(
                self.t(
                    "服務執行中；停止後才能修改設定。",
                    "The service is running. Stop it to change settings.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let is_new = self.service.is_new;
        let dirty = self.service_dirty(cx);
        let id = self.selected_id.clone();
        if !is_new && id.is_none() {
            self.error = Some(self.t("沒有選取的服務。", "No service is selected.").to_string());
            cx.notify();
            return;
        }
        let save_needed = is_new || dirty;
        let config = if save_needed {
            match self.current_config(cx) {
                Ok(config) => Some(config),
                Err(message) => {
                    self.error = Some(message);
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        let body = match config.as_ref().map(serde_json::to_value).transpose() {
            Ok(body) => body,
            Err(e) => {
                self.error = Some(e.to_string());
                cx.notify();
                return;
            }
        };
        let api = self.api.clone();
        self.perform(
            cx,
            async move {
                let saved: Option<ServiceStatus> = match (body, is_new) {
                    (Some(body), true) => Some(api.request("POST", "/api/services", Some(body)).await?),
                    (Some(body), false) => {
                        let id = id.clone().unwrap_or_default();
                        Some(api.request("PUT", &format!("/api/services/{id}"), Some(body)).await?)
                    }
                    (None, _) => None,
                };
                let target = saved.as_ref().map(|s| s.id.clone()).or(id).unwrap_or_default();
                let started = if start_after {
                    Some(
                        api.request::<Status>("POST", &format!("/api/services/{target}/start"), None)
                            .await,
                    )
                } else {
                    None
                };
                Ok((saved, started))
            },
            move |this, result, cx| {
                let Ok((saved, started)) = result else { return };
                if let Some(saved) = saved {
                    if let Some(status) = this.status.as_mut() {
                        match status.services.iter().position(|s| s.id == saved.id) {
                            Some(i) => status.services[i] = saved.clone(),
                            None => status.services.push(saved.clone()),
                        }
                    }
                    if this.selected_id.as_deref() != Some(saved.id.as_str()) {
                        this.selected_id = Some(saved.id.clone());
                        this.generation += 1;
                        this.records.reset();
                        this.cache.reset();
                    }
                    // Reload the editor from what the daemon stored. Deferred: `load_service`
                    // needs the window and we are inside an update of this view.
                    let entity = cx.entity();
                    let window = this.window;
                    let config = saved.config.clone();
                    cx.defer(move |cx| {
                        let _ = window.update(cx, |_, window, cx| {
                            entity.update(cx, |this, cx| this.load_service(config, false, window, cx));
                        });
                    });
                }
                match started {
                    Some(Ok(status)) => {
                        this.status = Some(status);
                        let message = this.t("服務已啟動。", "Service started.").to_string();
                        this.notice(message, cx);
                    }
                    Some(Err(e)) => {
                        let prefix = if save_needed {
                            this.t("設定已儲存，但啟動失敗：", "Settings were saved, but starting failed: ")
                        } else {
                            this.t("啟動失敗：", "Starting failed: ")
                        };
                        this.error = Some(format!("{prefix}{e}"));
                    }
                    None => {
                        let message = if is_new {
                            this.t("連線已建立。", "Connection created.")
                        } else {
                            this.t("設定已儲存。", "Settings saved.")
                        }
                        .to_string();
                        this.notice(message, cx);
                    }
                }
                this.refresh_status(cx);
            },
        );
    }

    pub fn render_service(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let unix_ok = self.unix_supported();
        let platform = self.status.as_ref().map(|s| s.platform.clone()).unwrap_or_default();
        let records_dir = self.status.as_ref().map(|s| s.records_dir.clone()).unwrap_or_default();
        let is_new = self.service.is_new;
        let locked = self.service_locked();
        let running = self.service_running();
        let busy = self.busy || (!is_new && self.selected_service().map(|s| s.busy.is_some()).unwrap_or(false));
        let parts = self.service.parts_now(cx);
        let config = parts.compose();
        let errs = self.validate_config(&config);
        let serve = parts.serve();
        let cli = build_cli(&config);

        // Placeholders follow the language and the draft.
        let name_placeholder = if serve {
            config
                .tcp
                .clone()
                .filter(|v| !v.is_empty())
                .or_else(|| config.socket.clone().filter(|v| !v.is_empty()))
                .or_else(|| Some(config.store.clone()).filter(|v| !v.is_empty()))
                .unwrap_or_else(|| "Serve".to_string())
        } else {
            format!(
                "→ {}",
                config
                    .upstream
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
            )
        };
        sync_placeholder(&self.service.name, &name_placeholder, window, cx);
        sync_placeholder(
            &self.service.store,
            self.t("/path/to/swiftcache/store", "/path/to/swiftcache/store"),
            window,
            cx,
        );
        sync_placeholder(&self.service.socket, "/tmp/swiftcache/cache.sock", window, cx);
        sync_placeholder(&self.service.up_host, "192.168.1.10 / cache-host", window, cx);
        sync_placeholder(
            &self.service.blob,
            self.t("未設定；目前回傳 inline bytes", "Not set; inline bytes are returned"),
            window,
            cx,
        );
        {
            let placeholder = SharedString::from("~/Library/Developer/Xcode/DerivedData\n/path/to/project/.build");
            if self.service.paths.read(cx).presentation().placeholder() != &placeholder {
                self.service
                    .paths
                    .update(cx, |s, cx| s.set_placeholder(placeholder, window, cx));
            }
        }

        let pal = crate::ui::palette(cx);
        let (text, muted, border, panel, chip, accent) =
            (pal.text, pal.muted, pal.border, pal.panel, pal.chip, pal.accent);
        let (field_bg, subtle) = (pal.field, pal.subtle);
        let (warn_bg, warn_border, warn_text) = (pal.warn_bg, pal.warn_border, pal.warn_text);
        let mono = cx.theme().mono_font_family.clone();
        let narrow = window.viewport_size().width < px(1320.);

        let card = move || {
            v_flex()
                .min_w_0()
                .gap(px(14.))
                .px(px(20.))
                .py(px(18.))
                .bg(panel)
                .border_1()
                .border_color(border)
                .rounded(px(10.))
        };
        let heading = |text: &str| {
            div()
                .text_size(px(16.))
                .font_weight(FontWeight(650.))
                .child(text.to_string())
        };
        let label = move |text: &str| {
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(muted)
                .child(text.to_string())
        };
        let hint = move |text: &str| div().text_size(px(12.)).text_color(muted).child(text.to_string());
        let field = || v_flex().gap(px(6.)).min_w_0();
        let banner = move |content: Div| {
            content
                .px(px(14.))
                .py(px(10.))
                .rounded(px(8.))
                .border_1()
                .border_color(warn_border)
                .bg(warn_bg)
                .text_color(warn_text)
                .text_size(px(13.))
        };
        // Single-line text input at the CSS control size.
        let control = |input: Input, locked: bool| input.h(px(36.)).disabled(locked);
        let mono_control = {
            let mono = mono.clone();
            move |input: Input, locked: bool| {
                input
                    .h(px(36.))
                    .font_family(mono.clone())
                    .text_size(px(12.5))
                    .disabled(locked)
            }
        };

        // ---- mode bar ----
        let mode_title = |m: &str| match m {
            "serve" => self.t("Server · 快取服務", "Server · Cache service"),
            _ => self.t("Proxy · 本機中繼", "Proxy · Local relay"),
        };
        let mode_bar = {
            let mut row = h_flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .px(px(16.))
                .py(px(13.))
                .bg(panel)
                .border_1()
                .border_color(border)
                .rounded(px(10.));
            if is_new {
                let mut group = h_flex().p(px(3.)).gap(px(2.)).bg(chip).rounded(px(8.));
                for (index, m) in ["serve", "proxy"].into_iter().enumerate() {
                    let active = parts.mode == m;
                    let disabled = locked || (m == "proxy" && !unix_ok);
                    let mut seg = div()
                        .id(("svc-mode", index))
                        .px(px(20.))
                        .py(px(8.))
                        .rounded(px(6.))
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(if active { accent } else { muted })
                        .child(mode_title(m).to_string());
                    if active {
                        seg = seg.bg(panel).shadow_sm();
                    }
                    seg = if disabled {
                        seg.opacity(0.5)
                    } else {
                        seg.cursor_pointer().on_click(cx.listener(move |this, _, window, cx| {
                            if this.service.parts.mode != m {
                                // The new draft's baseline keeps its generated Server name, even after changing roles.
                                let current = this.service.name.read(cx).value().to_string();
                                if let Some(name) = role_default_name(
                                    &this.service.baseline.name,
                                    &current,
                                    this.service.parts.serve(),
                                    m == "serve",
                                ) {
                                    set_input(&this.service.name, &name, window, cx);
                                }
                                this.service.parts.mode = m.to_string();
                                if m == "proxy" {
                                    this.service.parts.proxy_tcp = None;
                                }
                                cx.notify();
                            }
                        }))
                    };
                    group = group.child(seg);
                }
                row = row.child(group).child(hint(self.t(
                    "建立時選擇類型，儲存後即固定。",
                    "Choose the type when creating this connection. Saving makes it permanent.",
                )));
            } else {
                row = row
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .child(mode_title(&parts.mode).to_string()),
                    )
                    .child(hint(self.t(
                        "類型已固定；如需改用另一種類型，請新增連線。",
                        "The type is fixed. Create another connection to use a different type.",
                    )));
            }
            row
        };

        // ---- connection card ----
        let mut connection = card();
        connection = connection
            .child(
                v_flex()
                    .child(heading(if serve {
                        self.t("Server 連線與儲存", "Server connection and storage")
                    } else {
                        self.t("Proxy 連線與本機快取", "Proxy connection and local cache")
                    }))
                    .child(hint(if serve && config.tcp.is_none() {
                        self.t(
                            "這筆既有 Server 只使用 Unix socket，原設定會保留；需要對外提供服務時可在進階設定加入 TCP。",
                            "This existing Server uses only a Unix socket. Its settings are preserved; add TCP under Advanced settings to accept network clients.",
                        )
                    } else if serve {
                        self.t(
                            "本機工具鏈透過 socket 使用快取；其他機器的 Proxy 透過 TCP 連入。",
                            "Local toolchains use this cache through its socket; Proxies on other machines connect over TCP.",
                        )
                    } else {
                        self.t(
                            "Proxy 在本機以 Unix socket 接收 client，命中時由本機快取回應，未命中與寫入則轉送到遠端 serve。",
                            "Proxy accepts clients on a local Unix socket, serves hits from its local cache, and forwards misses and writes to a remote serve.",
                        )
                    })),
            )
            .child(
                field()
                    .child(label(self.t("連線名稱（選填）", "Connection name (optional)")))
                    .child(control(Input::new(&self.service.name), locked))
                    .child(hint(self.t("留空會依連線位址自動顯示預設名稱。名稱可以重複。", "Leave blank to use a name based on the connection address. Names can be shared."))),
            )
            .child(
                field()
                    .child(label(self.t("快取目錄 · --store / SWIFTCACHE_STORE", "Cache directory · --store / SWIFTCACHE_STORE")))
                    .child(mono_control(Input::new(&self.service.store), locked))
                    .child(hint(if serve {
                        self.t("Serve 的所有 CAS / KV 資料都存放在此目錄。", "Serve keeps all CAS / KV data in this directory.")
                    } else {
                        self.t(
                            "Proxy 在此保存本機讀取快取（必填）。寫入仍會先送到上游；清除快取不會影響上游。",
                            "Proxy keeps a local read-through cache here (required). Writes still go to the upstream first; clearing it never touches the upstream.",
                        )
                    })),
            );

        if unix_ok || !serve {
            connection = connection.child(
                field()
                    .child(label(self.t("本機 Unix socket · --socket", "Local Unix socket · --socket")))
                    .child(mono_control(Input::new(&self.service.socket), locked || !unix_ok))
                    .child(hint(if serve {
                        self.t(
                            "將此 socket 路徑填入 Xcode／Swift 的快取設定；TCP 供其他機器連入。",
                            "Use this socket path in the Xcode / Swift cache settings. TCP accepts connections from other machines.",
                        )
                    } else {
                        self.t(
                            "必填：本機工具鏈連到這個 socket；Proxy 再透過 TCP 連到上游 Server。",
                            "Required: local toolchains connect to this socket; Proxy connects to the upstream Server over TCP.",
                        )
                    })),
            );
        }

        // `.tcp-fields`: fluid column + 140px port column, gap 16.
        let two_columns = |left: Div, right: Div| {
            h_flex()
                .items_start()
                .gap(px(16.))
                .child(left.flex_1().min_w_0())
                .child(right.w(px(140.)).flex_none())
        };

        if serve && parts.tcp_enabled {
            let mut scopes: Vec<(String, &str)> = vec![
                ("127.0.0.1".to_string(), self.t("僅本機", "Local only")),
                ("0.0.0.0".to_string(), self.t("所有介面", "All interfaces")),
            ];
            let existing = parts.tcp_orig.0.clone();
            if existing != "127.0.0.1" && existing != "0.0.0.0" {
                scopes.push((existing, self.t("現有位址", "Existing address")));
            }
            let mut radios = h_flex().flex_wrap().gap_x(px(20.)).gap_y(px(12.));
            for (index, (host, title)) in scopes.into_iter().enumerate() {
                let checked = parts.tcp_host == host;
                let target = host.clone();
                radios = radios.child(
                    Radio::new(("svc-tcp-scope", index))
                        .label(format!("{title} {host}"))
                        .checked(checked)
                        .disabled(locked)
                        .on_click(cx.listener(move |this, _: &bool, _, cx| {
                            this.service.parts.tcp_host = target.clone();
                            cx.notify();
                        })),
                );
            }
            connection = connection.child(two_columns(
                v_flex()
                    .gap(px(10.))
                    .child(label(self.t("監聽範圍", "Listen scope")))
                    .child(radios),
                field()
                    .child(label(self.t("Port", "Port")))
                    .child(mono_control(Input::new(&self.service.tcp_port), locked)),
            ));
        }

        if !serve {
            let mut radios = h_flex().flex_wrap().gap_x(px(20.)).gap_y(px(12.));
            radios = radios
                .child(
                    Radio::new(("svc-up-host", 0usize))
                        .label(format!("{} 127.0.0.1", self.t("僅本機", "Local only")))
                        .checked(parts.up_local)
                        .disabled(locked)
                        .on_click(cx.listener(|this, _: &bool, _, cx| {
                            this.service.parts.up_local = true;
                            cx.notify();
                        })),
                )
                .child(
                    Radio::new(("svc-up-host", 1usize))
                        .label(self.t("自訂", "Custom"))
                        .checked(!parts.up_local)
                        .disabled(locked)
                        .on_click(cx.listener(|this, _: &bool, _, cx| {
                            this.service.parts.up_local = false;
                            cx.notify();
                        })),
                );
            let mut host_col = field().child(
                v_flex()
                    .gap(px(10.))
                    .child(label(self.t("上游主機", "Upstream host")))
                    .child(radios),
            );
            if !parts.up_local {
                host_col = host_col.child(mono_control(Input::new(&self.service.up_host), locked));
            }
            connection = connection.child(
                field()
                    .child(label(self.t(
                        "上游 Serve · --upstream / SWIFTCACHE_UPSTREAM",
                        "Upstream Serve · --upstream / SWIFTCACHE_UPSTREAM",
                    )))
                    .child(two_columns(
                        host_col,
                        field()
                            .child(label(self.t("Port", "Port")))
                            .child(mono_control(Input::new(&self.service.up_port), locked)),
                    )),
            );
        }

        // ---- advanced (`details.advanced-settings`) ----
        let advanced_open = self.service.advanced_open;
        let mut advanced_box = v_flex().border_t_1().border_color(border).pt(px(12.)).child(
            v_flex()
                .id("svc-advanced")
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.service.advanced_open = !this.service.advanced_open;
                    cx.notify();
                }))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                    "{} {}",
                    if advanced_open { "▾" } else { "▸" },
                    self.t("進階設定", "Advanced settings")
                )))
                .child(div().ml(px(16.)).mt(px(5.)).child(hint(self.t(
                    "訊息上限、Log、檔案回應與本機清除路徑",
                    "Message limit, logs, file responses, and local cleanup paths",
                )))),
        );
        if advanced_open {
            let mut advanced = v_flex().min_w_0().gap(px(12.)).mt(px(16.));
            if serve && !parts.tcp_enabled {
                advanced = advanced.child(
                    h_flex().child(
                        Button::new("svc-add-tcp")
                            .outline()
                            .h(px(36.))
                            .label(self.t("加入 TCP 監聽", "Add TCP listener"))
                            .disabled(locked)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let (host, port) = split_tcp(DEFAULT_TCP);
                                this.service.parts.tcp_enabled = true;
                                this.service.parts.tcp_host = host;
                                let input = this.service.tcp_port.clone();
                                set_input(&input, &port, window, cx);
                                cx.notify();
                            })),
                    ),
                );
            }
            let size_unit = parts.size_unit;
            let mut units = h_flex().gap(px(8.));
            for (index, unit) in [SizeUnit::Mib, SizeUnit::Gib].into_iter().enumerate() {
                let active = size_unit == unit;
                let button = Button::new(("svc-size-unit", index))
                    .h(px(36.))
                    .label(unit.label())
                    .disabled(locked)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // Keep the number, change the magnitude: 20 MiB -> 20 GiB.
                        this.service.parts.size_unit = unit;
                        cx.notify();
                    }));
                units = units.child(if active { button.primary() } else { button.outline() });
            }
            let mut presets = h_flex().gap(px(4.)).flex_wrap();
            for (index, level) in LOG_LEVELS.into_iter().enumerate() {
                presets = presets.child(
                    Button::new(("svc-log-preset", index))
                        .ghost()
                        .small()
                        .label(level)
                        .selected(parts.log.trim() == level)
                        .disabled(locked)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let input = this.service.log.clone();
                            set_input(&input, level, window, cx);
                            cx.notify();
                        })),
                );
            }
            // `.grid2`: two equal columns, gap 16.
            advanced = advanced.child(
                h_flex()
                    .items_start()
                    .gap(px(16.))
                    .child(
                        field()
                            .flex_1()
                            .child(label(self.t(
                                "單筆訊息上限 · --max-message-size",
                                "Maximum message size · --max-message-size",
                            )))
                            .child(
                                h_flex()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .child(mono_control(Input::new(&self.service.size), locked)),
                                    )
                                    .child(units),
                            ),
                    )
                    .child(
                        field()
                            .flex_1()
                            .child(label(
                                self.t("文字 Log 層級 · SWIFTCACHE_LOG", "Text log level · SWIFTCACHE_LOG"),
                            ))
                            .child(mono_control(Input::new(&self.service.log), locked))
                            .child(presets),
                    ),
            );
            advanced = advanced.child(
                field()
                    .child(
                        Checkbox::new("svc-blob-enabled")
                            .label(self.t("啟用檔案回應目錄 · --blob-dir", "Enable file response directory · --blob-dir"))
                            .checked(parts.blob_enabled)
                            .disabled(locked)
                            .on_click(cx.listener(|this, checked: &bool, window, cx| {
                                this.service.parts.blob_enabled = *checked;
                                if *checked {
                                    let input = this.service.blob.clone();
                                    if input.read(cx).value().trim().is_empty() {
                                        set_input(&input, DEFAULT_BLOB_DIR, window, cx);
                                    }
                                }
                                cx.notify();
                            })),
                    )
                    .child(mono_control(Input::new(&self.service.blob), locked || !parts.blob_enabled))
                    .child(hint(if serve {
                        self.t("只適用於 client 與 server 共用檔案系統；跨機器部署維持 bytes。", "Only applies when client and server share a file system; cross-machine setups keep using bytes.")
                    } else {
                        self.t(
                            "此目錄位於開發機，用於提供本機 client 可存取的檔案回應；上傳時本機 file_path 會被讀成 bytes 再傳到遠端。",
                            "This directory is on the development machine and serves file responses that local clients can access; on upload, the local file_path is read into bytes and sent to the remote.",
                        )
                    })),
            );
            advanced = advanced.child(
                field()
                    .child(label(self.t("本機建置快取路徑（選填，每行一個）", "Local build cache paths (optional, one per line)")))
                    .child(Textarea::new(&self.service.paths).disabled(locked).h(px(78.)).font_family(mono.clone()).text_size(px(12.5)))
                    .child(hint(self.t(
                        "供「快取管理」清除時勾選；只有此處列出的路徑可被清除，不會刪除未宣告的目錄。",
                        "Offered as choices when clearing in Cache; only paths listed here can be cleared, and undeclared directories are never deleted.",
                    ))),
            );
            advanced_box = advanced_box.child(advanced);
        }
        connection = connection.child(advanced_box);

        if !errs.is_empty() {
            let mut list = v_flex().gap(px(4.));
            for message in &errs {
                list = list.child(div().child(message.clone()));
            }
            connection = connection.child(banner(list));
        }

        connection = connection.child(hint(self.t(
            "不支援熱更新；套用設定時明確重啟。",
            "Hot reload isn’t supported; settings are applied by an explicit restart.",
        )));

        // ---- left column ----
        let mut left = v_flex().min_w_0().gap(px(18.));
        if !unix_ok {
            left = left.child(banner(div().child(if self.t("zh", "en") == "zh" {
                format!("此平台（{platform}）不支援 Unix socket：Serve 僅能使用 TCP，Proxy 模式需要 Unix socket，因此無法使用。")
            } else {
                format!("This platform ({platform}) does not support Unix sockets: Serve can only use TCP, and Proxy mode requires a Unix socket, so it is unavailable.")
            })));
        }
        left = left.child(mode_bar);
        if !is_new {
            left = left.child(hint(if busy {
                self.t(
                    "服務正在處理操作，完成後可修改設定。",
                    "A service operation is in progress. Settings can be changed when it finishes.",
                )
            } else if running {
                self.t(
                    "服務執行中；停止後才能修改設定。",
                    "The service is running. Stop it to change settings.",
                )
            } else {
                self.t(
                    "設定儲存後，按啟動服務開始接收請求。",
                    "Save your settings, then start the service to accept requests.",
                )
            }));
        }
        left = left.child(connection);

        let page = v_flex()
            .id("service-page")
            .size_full()
            .overflow_y_scroll()
            .text_size(px(14.))
            .text_color(text);
        if is_new {
            return page.child(left).into_any_element();
        }

        // ---- right column: recorded data, equivalent CLI, remote note ----
        let kv = |key: &str, value: &str| {
            h_flex()
                .items_start()
                .gap(px(12.))
                .text_size(px(13.))
                .child(div().w(px(96.)).flex_none().text_color(muted).child(key.to_string()))
                .child(div().flex_1().min_w_0().child(value.to_string()))
        };
        let recorded = card()
            .child(heading(self.t("紀錄內容", "Recorded data")))
            .child(
                v_flex()
                    .gap(px(9.))
                    .child(kv(self.t("每條事件", "Each event"), self.t("訊息、層級、時間、來源", "Message, level, time, source")))
                    .child(kv(self.t("每筆傳輸", "Each transfer"), self.t("完整請求 / 回應、結果與耗時", "Full request / response, result, and duration")))
                    .child(kv("Payload", self.t("保存原始 bytes，供預覽與匯出", "Raw bytes are kept for preview and export"))),
            )
            .child(label(self.t("紀錄存放位置（唯讀）", "Log storage location (read-only)")))
            .child(
                div()
                    .px(px(12.))
                    .py(px(10.))
                    .border_1()
                    .border_color(border)
                    .rounded(px(7.))
                    .bg(field_bg)
                    .font_family(mono.clone())
                    .text_size(px(12.5))
                    .child(records_dir),
            )
            .child(hint(self.t(
                "紀錄與快取目錄分開；清空快取時保留。文字 log 層級與 UI 詳細紀錄分別設定。",
                "Logs are kept separate from the store and survive clearing the cache. The text log level and detailed UI logs are configured separately.",
            )));

        let copy_text = cli.replace(" \\\n  ", " ");
        let cli_card = card()
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.))
                    .child(heading(self.t("同等 CLI 指令", "Equivalent CLI command")))
                    .child(
                        Button::new("svc-copy-cli")
                            .outline()
                            .small()
                            .label(self.t("複製 CLI 指令", "Copy CLI command"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                                let message = this.t("已複製 CLI 指令。", "CLI command copied.").to_string();
                                this.notice(message, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("svc-cli-text")
                    .p(px(12.))
                    .border_1()
                    .border_color(border)
                    .rounded(px(8.))
                    .bg(subtle)
                    .max_h(px(340.))
                    .overflow_scroll()
                    .font_family(mono.clone())
                    .text_size(px(12.5))
                    .line_height(relative(1.55))
                    .whitespace_nowrap()
                    .child(cli),
            )
            .child(hint(self.t(
                "反映上方表單內容；未啟用的 socket / blob 選項不產生 CLI 參數。UI 詳細紀錄另由 UI 管理。",
                "Reflects the form above; disabled socket / blob options produce no CLI arguments. Detailed UI logs are managed by the UI.",
            )));

        let mut right = v_flex().min_w_0().gap(px(18.)).child(recorded).child(cli_card);
        if !serve {
            right = right.child(
                card()
                    .child(heading(self.t("遠端容量與清除", "Remote capacity and clearing")))
                    .child(hint(self.t(
                        "現有 gRPC 僅提供 CAS / KV，不含管理 API，因此無法取得上游磁碟用量，也不提供遠端清除。快取清除作用於資料所在的主機；本機建置快取請另行於「快取管理」選擇。",
                        "The current gRPC API only offers CAS / KV and no management API, so upstream disk usage is unavailable and remote clearing isn’t offered. Clearing acts on the host where the data lives; choose local build caches separately in Cache.",
                    ))),
            );
        }

        // `.service-grid`: fluid + 400px, gap 18; one column below 1320px.
        if narrow {
            page.child(v_flex().gap(px(18.)).child(left).child(right))
                .into_any_element()
        } else {
            page.child(
                h_flex()
                    .items_start()
                    .gap(px(18.))
                    .child(left.flex_1())
                    .child(right.w(px(400.)).flex_none()),
            )
            .into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that re-exports `gpui_kit::*`, whose `test` macro shadows the built-in one.
    use super::{GIB, MIB, Parts, SizeUnit, build_cli, role_default_name, shell_quote, split_tcp, split_upstream};
    use crate::types::Config;

    fn config(mode: &str) -> Config {
        Config {
            name: "Demo".into(),
            mode: mode.into(),
            store: "/var/cache/swiftcache".into(),
            socket: Some("/tmp/swiftcache/cache.sock".into()),
            tcp: if mode == "serve" {
                Some("0.0.0.0:50051".into())
            } else {
                None
            },
            upstream: "http://127.0.0.1:50051".into(),
            blob_dir: None,
            max_message_size: 512 * MIB,
            log_level: "swiftcache=info".into(),
            local_cache_paths: Vec::new(),
        }
    }

    #[test]
    fn generated_name_follows_role_and_custom_names_stay() {
        assert_eq!(
            role_default_name("Server-1a2b3c4d", "Server-1a2b3c4d", true, false).as_deref(),
            Some("Proxy-1a2b3c4d")
        );
        assert_eq!(
            role_default_name("Server-1a2b3c4d", "Proxy-1a2b3c4d", false, true).as_deref(),
            Some("Server-1a2b3c4d")
        );
        // A user-typed name, or one carrying another draft's id, is never replaced.
        assert_eq!(role_default_name("Server-1a2b3c4d", "My cache", true, false), None);
        assert_eq!(
            role_default_name("Server-1a2b3c4d", "Server-ffffffff", true, false),
            None
        );
        // The current name must match the role being left, not the other one.
        assert_eq!(
            role_default_name("Server-1a2b3c4d", "Proxy-1a2b3c4d", true, false),
            None
        );
        // Saved connections have no generated baseline.
        assert_eq!(role_default_name("Local Cache", "Local Cache", true, false), None);
    }

    #[test]
    fn loading_a_config_and_composing_it_unchanged_is_lossless() {
        let mut odd = config("serve");
        odd.tcp = Some("[::1]:7000".into());
        odd.max_message_size = 512 * MIB + 1;
        odd.blob_dir = Some("/tmp/blobs".into());
        odd.local_cache_paths = vec!["/tmp/a".into(), "/tmp/b".into()];
        let mut proxy = config("proxy");
        proxy.upstream = "HTTPS://[fe80::1]:9443/cache?x=1".into();
        proxy.max_message_size = 2 * GIB;
        let mut hostname = config("proxy");
        hostname.upstream = "http://cache-host".into();
        let mut legacy_proxy_tcp = config("proxy");
        legacy_proxy_tcp.tcp = Some("127.0.0.1:1".into());
        for c in [config("serve"), odd, proxy, hostname, legacy_proxy_tcp] {
            // A difference here would mark every freshly loaded connection as unsaved.
            assert_eq!(Parts::from_config(&c, true).compose(), c, "{c:?}");
        }
    }

    #[test]
    fn serve_without_unix_sockets_keeps_its_saved_socket_value() {
        let mut c = config("serve");
        c.socket = None;
        assert_eq!(Parts::from_config(&c, false).compose().socket, None);
    }

    #[test]
    fn edited_upstream_keeps_scheme_brackets_and_path() {
        let mut c = config("proxy");
        c.upstream = "https://[::1]:9000/cache".into();
        let mut parts = Parts::from_config(&c, true);
        parts.up_port = "9001".into();
        assert_eq!(parts.compose().upstream, "https://[::1]:9001/cache");

        parts.up_host = "fe80::2".into();
        assert_eq!(parts.compose().upstream, "https://[fe80::2]:9001/cache");

        let mut parts = Parts::from_config(&config("proxy"), true);
        parts.up_local = false;
        parts.up_host = "cache-host".into();
        assert_eq!(parts.compose().upstream, "http://cache-host:50051");
    }

    #[test]
    fn edited_tcp_and_size_are_rebuilt_from_the_fields() {
        let mut parts = Parts::from_config(&config("serve"), true);
        parts.tcp_host = "127.0.0.1".into();
        parts.tcp_port = "6000".into();
        parts.size = "2".into();
        parts.size_unit = SizeUnit::Gib;
        let c = parts.compose();
        assert_eq!(c.tcp.as_deref(), Some("127.0.0.1:6000"));
        assert_eq!(c.max_message_size, 2 * GIB);

        parts.tcp_enabled = false;
        parts.size = "garbage".into();
        let c = parts.compose();
        assert_eq!(c.tcp, None);
        assert_eq!(
            c.max_message_size, 0,
            "unparsable sizes become 0 so validation rejects them"
        );
    }

    #[test]
    fn upstream_and_tcp_splitting() {
        let up = split_upstream("http://cache-host:50051/x");
        assert_eq!(
            (
                up.prefix.as_str(),
                up.host.as_str(),
                up.port.as_str(),
                up.suffix.as_str()
            ),
            ("http://", "cache-host", "50051", "/x")
        );
        let up = split_upstream("https://cache-host");
        assert_eq!((up.host.as_str(), up.port.as_str()), ("cache-host", "443"));
        let up = split_upstream("cache-host");
        assert_eq!(
            (up.prefix.as_str(), up.host.as_str(), up.port.as_str()),
            ("http://", "cache-host", "50051")
        );
        assert_eq!(split_tcp("[::1]:50051"), ("[::1]".to_string(), "50051".to_string()));
        assert_eq!(split_tcp("localhost"), ("localhost".to_string(), String::new()));
    }

    #[test]
    fn cli_preview_matches_role_and_quotes_unsafe_values() {
        let mut serve = config("serve");
        serve.store = "/tmp/my cache".into();
        assert_eq!(
            build_cli(&serve),
            "SWIFTCACHE_LOG=swiftcache=info swiftcache serve \\\n  --store '/tmp/my cache' \\\n  --tcp 0.0.0.0:50051 \\\n  --socket /tmp/swiftcache/cache.sock \\\n  --max-message-size 536870912"
        );
        let mut proxy = config("proxy");
        proxy.tcp = Some("127.0.0.1:1".into());
        proxy.log_level = String::new();
        let cli = build_cli(&proxy);
        assert!(cli.starts_with("swiftcache proxy"), "{cli}");
        assert!(cli.contains("--upstream http://127.0.0.1:50051"), "{cli}");
        assert!(!cli.contains("--tcp"), "proxy never listens on TCP: {cli}");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
