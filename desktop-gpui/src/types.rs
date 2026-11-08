use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

impl Default for Config {
    fn default() -> Self {
        let id = uuid::Uuid::new_v4().as_fields().0;
        Self {
            name: format!("Server-{id:08x}"), mode: "serve".into(), store: format!("/tmp/{id:08x}"),
            socket: cfg!(unix).then(|| format!("/tmp/{id:08x}.sock")),
            tcp: Some("127.0.0.1:50051".into()), upstream: "http://127.0.0.1:50051".into(),
            blob_dir: None, max_message_size: 512 * 1024 * 1024,
            log_level: "swiftcache=info".into(), local_cache_paths: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub up_bytes: u64,
    pub down_bytes: u64,
    pub transfers: u64,
    pub errors: u64,
    pub kv_hit: u64,
    pub kv_miss: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub id: String,
    pub config: Config,
    pub running: bool,
    pub started_at: Option<i64>,
    pub error: Option<String>,
    pub busy: Option<String>,
    pub summary: Summary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub services: Vec<ServiceStatus>,
    pub records_dir: String,
    pub platform: String,
    pub unix_socket_supported: bool,
    pub summary: Summary,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsagePart { pub bytes: u64, pub files: u64 }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Usage {
    pub objects: UsagePart,
    pub kv: UsagePart,
    pub tmp: UsagePart,
    pub total_bytes: u64,
    pub total_files: u64,
    pub records: UsagePart,
    pub disk_available: u64,
    pub disk_total: u64,
    pub scanned_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PayloadRef { pub id: String, pub label: String, pub bytes: u64 }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transfer {
    pub id: i64,
    pub started_at: i64,
    pub duration_us: u64,
    pub peer: String,
    pub method: String,
    pub direction: String,
    pub object_id: Option<String>,
    pub outcome: String,
    pub bytes: u64,
    pub grpc_status: String,
    pub error: Option<String>,
    pub request: Value,
    pub response: Value,
    pub payloads: Vec<PayloadRef>,
    pub service_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppEvent {
    pub id: i64,
    pub time: i64,
    pub level: String,
    pub kind: String,
    pub message: String,
    pub peer: Option<String>,
    pub transfer_id: Option<i64>,
    pub service_id: Option<String>,
    pub fields: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page<T> { pub items: Vec<T>, pub total: u64 }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PayloadChunk {
    pub id: String,
    pub offset: u64,
    pub total: u64,
    pub hex: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClearPlan {
    pub store: String,
    pub paths: Vec<String>,
    pub local_paths: Vec<String>,
    pub estimated_bytes: u64,
    pub running: bool,
    pub plan_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClearResult {
    pub reclaimed_bytes: u64,
    pub removed_files: u64,
    pub restarted: bool,
    pub usage: Usage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind { Service, Transfers, Events, Cache }

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() { value /= 1024.0; unit += 1; }
    if unit == 0 { format!("{bytes} B") }
    else if value >= 100.0 { format!("{value:.0} {}", UNITS[unit]) }
    else if value >= 10.0 { format!("{value:.1} {}", UNITS[unit]) }
    else { format!("{value:.2} {}", UNITS[unit]) }
}

pub fn format_int(value: u64) -> String {
    let text = value.to_string();
    let mut output = String::with_capacity(text.len() + text.len() / 3);
    for (i, ch) in text.chars().enumerate() {
        if i > 0 && (text.len() - i).is_multiple_of(3) { output.push(','); }
        output.push(ch);
    }
    output
}

pub fn exact_bytes(bytes: u64) -> String { format!("{} ({} bytes)", format_bytes(bytes), format_int(bytes)) }
pub fn format_time(ms: i64) -> String {
    Local.timestamp_millis_opt(ms).single().map(|time| time.format("%Y-%m-%d %H:%M:%S%.3f").to_string()).unwrap_or_else(|| ms.to_string())
}
pub fn format_duration(us: u64) -> String {
    if us < 1000 { format!("{us} µs") } else if us < 1_000_000 { format!("{:.1} ms", us as f64 / 1000.0) } else { format!("{:.2} s", us as f64 / 1_000_000.0) }
}
pub fn service_label(config: &Config) -> String {
    if !config.name.trim().is_empty() { config.name.clone() }
    else if config.mode == "proxy" { format!("→ {}", config.upstream) }
    else { config.tcp.as_ref().or(config.socket.as_ref()).cloned().unwrap_or_else(|| config.store.clone()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_connection_defaults_share_one_short_id() {
        let a = Config::default();
        let id = a.name.strip_prefix("Server-").expect("generated Server name");
        assert_eq!(id.len(), 8);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "{id}");
        assert_eq!(a.store, format!("/tmp/{id}"));
        if cfg!(unix) {
            assert_eq!(a.socket.as_deref(), Some(format!("/tmp/{id}.sock").as_str()));
        }
        assert_ne!(Config::default().name, a.name, "each draft gets its own id");
    }

    #[test]
    fn service_label_falls_back_by_role() {
        let mut c = Config { name: "  ".into(), ..Config::default() };
        c.tcp = Some("127.0.0.1:50061".into());
        assert_eq!(service_label(&c), "127.0.0.1:50061");
        c.tcp = None;
        c.socket = Some("/tmp/a.sock".into());
        assert_eq!(service_label(&c), "/tmp/a.sock");
        c.socket = None;
        assert_eq!(service_label(&c), c.store);
        c.mode = "proxy".into();
        assert_eq!(service_label(&c), "→ http://127.0.0.1:50051");
        c.name = "Proxy Demo".into();
        assert_eq!(service_label(&c), "Proxy Demo");
    }

    #[test]
    fn byte_and_integer_formatting_boundaries() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(10 * 1024), "10.0 KiB");
        assert_eq!(format_bytes(100 * 1024), "100 KiB");
        assert_eq!(format_bytes(1024u64.pow(4) * 2048), "2048 TiB", "TiB is the largest unit");
        assert_eq!(format_int(0), "0");
        assert_eq!(format_int(999), "999");
        assert_eq!(format_int(1000), "1,000");
        assert_eq!(format_int(1_234_567), "1,234,567");
        assert_eq!(format_duration(999), "999 µs");
        assert_eq!(format_duration(1500), "1.5 ms");
        assert_eq!(format_duration(2_500_000), "2.50 s");
    }
}
