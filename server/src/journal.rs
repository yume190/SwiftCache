//! Durable record of every RPC and every tracing event.
//!
//! The journal lives in its own directory, separate from the cache store, so
//! clearing the cache never erases history:
//!
//! ```text
//! <root>/journal.sqlite3        metadata: transfers, payload refs, events
//! <root>/payloads/ab/<digest>   deduplicated payload bytes (blake3 hex)
//! ```
//!
//! Handlers open a [`Rec`] per RPC, hand it borrowed request/response bytes as
//! they become available, and [`Rec::done`] it before returning the
//! response, so history survives the UI (or the process) going away.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context as _, Result};
use parking_lot::Mutex;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::oneshot;
use tonic::Status;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event as TracingEvent, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use crate::peer::Peer;
use crate::proto::cas::{cas_bytes, CasBytes};
use crate::store::human_bytes;

const MAX_PAGE: usize = 200;
const DEFAULT_PAGE: usize = 50;
const MAX_CHUNK: usize = 65536;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS transfers (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at  INTEGER NOT NULL,
    duration_us INTEGER NOT NULL,
    peer        TEXT NOT NULL,
    method      TEXT NOT NULL,
    direction   TEXT NOT NULL,
    object_id   TEXT,
    outcome     TEXT NOT NULL,
    bytes       INTEGER NOT NULL,
    grpc_status TEXT NOT NULL,
    error       TEXT,
    request     TEXT NOT NULL,
    response    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS transfers_started ON transfers(started_at);
CREATE TABLE IF NOT EXISTS transfer_payloads (
    transfer_id INTEGER NOT NULL REFERENCES transfers(id),
    seq         INTEGER NOT NULL,
    digest      TEXT NOT NULL,
    label       TEXT NOT NULL,
    bytes       INTEGER NOT NULL,
    PRIMARY KEY (transfer_id, seq)
);
DROP INDEX IF EXISTS transfer_payloads_digest;
CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    time        INTEGER NOT NULL,
    level       TEXT NOT NULL,
    kind        TEXT NOT NULL,
    message     TEXT NOT NULL,
    peer        TEXT,
    transfer_id INTEGER,
    fields      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_time ON events(time);
CREATE INDEX IF NOT EXISTS events_kind ON events(kind);
CREATE INDEX IF NOT EXISTS events_transfer ON events(transfer_id) WHERE transfer_id IS NOT NULL;
-- Payloads up to INLINE_PAYLOAD bytes (KV entries, small objects): one row in
-- the recording transaction instead of a file create + rename per payload.
CREATE TABLE IF NOT EXISTS payload_blobs (
    digest      TEXT PRIMARY KEY,
    data        BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS summary (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    up_bytes    INTEGER NOT NULL,
    down_bytes  INTEGER NOT NULL,
    transfers   INTEGER NOT NULL,
    errors      INTEGER NOT NULL,
    kv_hit      INTEGER NOT NULL,
    kv_miss     INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS service_summary (
    service_id  TEXT PRIMARY KEY,
    up_bytes    INTEGER NOT NULL,
    down_bytes  INTEGER NOT NULL,
    transfers   INTEGER NOT NULL,
    errors      INTEGER NOT NULL,
    kv_hit      INTEGER NOT NULL,
    kv_miss     INTEGER NOT NULL
);
";

/// Columns added after the first release; `ALTER TABLE` has no `IF NOT EXISTS`.
const SERVICE_COLUMNS: [(&str, &str); 2] = [("transfers", "transfers_service"), ("events", "events_service")];

const TRANSFER_COLS: &str = "id, started_at, duration_us, peer, method, direction, object_id, \
                             outcome, bytes, grpc_status, error, request, response, service_id";
const EVENT_COLS: &str = "id, time, level, kind, message, peer, transfer_id, fields, service_id";

/// Name of the span field that attributes tracing events to a service
/// (see [`RunningService`](crate::runtime::RunningService)).
pub const SERVICE_SPAN_FIELD: &str = "service_id";

#[derive(Debug, Default, Clone, Deserialize)]
pub struct RecordQuery {
    pub search: Option<String>,
    pub direction: Option<String>,
    pub outcome: Option<String>,
    /// `CAS` or `KV`.
    pub service: Option<String>,
    pub level: Option<String>,
    pub kind: Option<String>,
    /// Unix milliseconds; only records at or after this time.
    pub since: Option<i64>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Events only: restrict to those linked to this transfer.
    pub transfer_id: Option<i64>,
    /// Restrict to records produced by this desktop service.
    pub service_id: Option<String>,
    /// Inclusive snapshot boundary for stable exports during live traffic.
    pub max_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PayloadRef {
    pub id: String,
    pub label: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub id: i64,
    pub time: i64,
    pub level: String,
    pub kind: String,
    pub message: String,
    pub peer: Option<String>,
    pub transfer_id: Option<i64>,
    pub fields: Value,
    pub service_id: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Summary {
    pub up_bytes: u64,
    pub down_bytes: u64,
    pub transfers: u64,
    pub errors: u64,
    pub kv_hit: u64,
    pub kv_miss: u64,
}

#[derive(Debug, Serialize)]
pub struct PayloadChunk {
    pub id: String,
    pub offset: u64,
    pub total: u64,
    pub hex: String,
    pub text: String,
}

pub struct Journal {
    payload_dir: PathBuf,
    /// Single writer thread (see [`writer_loop`]); every insert goes through it
    /// so concurrent RPCs group-commit instead of queueing on a connection lock.
    writer: mpsc::Sender<Job>,
    /// History queries for the UI. WAL lets it read while the writer commits.
    reader: Mutex<Connection>,
    tmp_counter: AtomicU64,
    /// CAS object id -> (payload digest, length). Object ids are content
    /// derived, so a hit on a known id never needs its bytes re-hashed.
    known: Mutex<KnownObjects>,
}

/// CAS object id -> (payload digest, payload length).
type KnownObjects = HashMap<Box<[u8]>, (String, u64)>;

/// Bound on [`Journal::known`]; cleared wholesale when reached.
const KNOWN_OBJECTS: usize = 65_536;
/// Payloads at most this large are stored in `payload_blobs`, larger ones as
/// files under `payloads/`.
const INLINE_PAYLOAD: usize = 16 * 1024;
/// Most rows the writer commits in one transaction.
const MAX_BATCH: usize = 512;

type AckResult = std::result::Result<(), String>;

enum Ack {
    None,
    Async(oneshot::Sender<AckResult>),
    Sync(mpsc::Sender<AckResult>),
}

impl Ack {
    fn send(self, result: AckResult) {
        match self {
            Ack::None => {
                if let Err(e) = result {
                    // Cannot go through tracing: the journal layer feeds this writer.
                    eprintln!("journal: recording failed: {e}");
                }
            }
            Ack::Async(tx) => drop(tx.send(result)),
            Ack::Sync(tx) => drop(tx.send(result)),
        }
    }
}

/// A transfer, fully serialized on the RPC's own task before it is queued.
struct TransferRow {
    started: Instant,
    started_at: i64,
    peer: String,
    method: &'static str,
    direction: &'static str,
    object_id: Option<String>,
    outcome: &'static str,
    bytes: u64,
    grpc_status: String,
    error: Option<String>,
    request: String,
    response: String,
    payloads: Vec<PayloadRef>,
    /// (digest, bytes) of payloads stored inline.
    inline: Vec<(String, Vec<u8>)>,
    service: Option<Arc<str>>,
}

struct EventRow {
    time: i64,
    level: String,
    kind: &'static str,
    message: String,
    peer: Option<String>,
    fields: String,
    service: Option<Arc<str>>,
}

enum Pending {
    Transfer(Box<TransferRow>),
    Event(EventRow),
    /// Acknowledged once everything queued before it is committed.
    Flush,
    /// Deletes one service's history; applied in queue order with the rows around it.
    Clear {
        service: String,
        scope: HistoryScope,
    },
}

struct Job {
    row: Pending,
    ack: Ack,
}

/// Runs blocking disk work without stalling the async worker it is called on.
fn blocking<R>(f: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn valid_digest(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `%needle%` with LIKE metacharacters escaped (use with `ESCAPE '\'`).
fn like(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len() + 2);
    out.push('%');
    for c in needle.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

fn nonempty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Accumulates a parameterized `WHERE` clause.
#[derive(Default)]
struct Filter {
    clauses: Vec<String>,
    args: Vec<SqlValue>,
}

impl Filter {
    fn eq(&mut self, column: &str, value: Option<&str>, lower: bool) {
        if let Some(v) = value {
            self.clauses.push(format!("{column} = ?"));
            self.args
                .push(SqlValue::Text(if lower { v.to_lowercase() } else { v.to_owned() }));
        }
    }

    fn search(&mut self, columns: &[&str], value: Option<&str>) {
        if let Some(v) = value {
            let or: Vec<String> = columns.iter().map(|c| format!("{c} LIKE ? ESCAPE '\\'")).collect();
            self.clauses.push(format!("({})", or.join(" OR ")));
            for _ in columns {
                self.args.push(SqlValue::Text(like(v)));
            }
        }
    }

    fn since(&mut self, column: &str, since: Option<i64>) {
        if let Some(s) = since {
            self.clauses.push(format!("{column} >= ?"));
            self.args.push(SqlValue::Integer(s));
        }
    }

    fn sql(&self) -> String {
        if self.clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", self.clauses.join(" AND "))
        }
    }
}

fn window(q: &RecordQuery) -> (i64, i64) {
    let limit = q.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    (limit as i64, q.offset.unwrap_or(0) as i64)
}

fn page<T>(
    conn: &Connection,
    table: &str,
    cols: &str,
    filter: &Filter,
    q: &RecordQuery,
    map: impl Fn(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Page<T>> {
    let wh = filter.sql();
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {table}{wh}"),
        params_from_iter(filter.args.iter()),
        |r| r.get(0),
    )?;
    let (limit, offset) = window(q);
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols} FROM {table}{wh} ORDER BY id DESC LIMIT ? OFFSET ?"
    ))?;
    let mut args: Vec<SqlValue> = filter.args.clone();
    args.push(SqlValue::Integer(limit));
    args.push(SqlValue::Integer(offset));
    let items = stmt
        .query_map(params_from_iter(args.iter()), map)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Page {
        items,
        total: total as u64,
    })
}

fn parse_json(s: String) -> Value {
    serde_json::from_str(&s).unwrap_or(Value::String(s))
}

fn transfer_row(r: &Row<'_>) -> rusqlite::Result<Transfer> {
    Ok(Transfer {
        id: r.get(0)?,
        started_at: r.get(1)?,
        duration_us: r.get::<_, i64>(2)? as u64,
        peer: r.get(3)?,
        method: r.get(4)?,
        direction: r.get(5)?,
        object_id: r.get(6)?,
        outcome: r.get(7)?,
        bytes: r.get::<_, i64>(8)? as u64,
        grpc_status: r.get(9)?,
        error: r.get(10)?,
        request: parse_json(r.get(11)?),
        response: parse_json(r.get(12)?),
        payloads: Vec::new(),
        service_id: r.get(13)?,
    })
}

fn event_row(r: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: r.get(0)?,
        time: r.get(1)?,
        level: r.get(2)?,
        kind: r.get(3)?,
        message: r.get(4)?,
        peer: r.get(5)?,
        transfer_id: r.get(6)?,
        fields: parse_json(r.get(7)?),
        service_id: r.get(8)?,
    })
}

/// Payload refs for a page of transfers in one query instead of one per row.
fn attach_payloads(conn: &Connection, items: &mut [Transfer]) -> rusqlite::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let marks = vec!["?"; items.len()].join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT transfer_id, digest, label, bytes FROM transfer_payloads \
         WHERE transfer_id IN ({marks}) ORDER BY transfer_id, seq"
    ))?;
    let mut by_id: HashMap<i64, Vec<PayloadRef>> = HashMap::with_capacity(items.len());
    let rows = stmt.query_map(params_from_iter(items.iter().map(|t| t.id)), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            PayloadRef {
                id: r.get(1)?,
                label: r.get(2)?,
                bytes: r.get::<_, i64>(3)? as u64,
            },
        ))
    })?;
    for row in rows {
        let (id, p) = row?;
        by_id.entry(id).or_default().push(p);
    }
    for t in items {
        t.payloads = by_id.remove(&t.id).unwrap_or_default();
    }
    Ok(())
}

fn insert_event(conn: &Connection, e: &EventRow, transfer_id: Option<i64>) -> rusqlite::Result<i64> {
    conn.prepare_cached(
        "INSERT INTO events (time, level, kind, message, peer, transfer_id, fields, service_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )?
    .execute(params![
        e.time,
        e.level,
        e.kind,
        e.message,
        e.peer,
        transfer_id,
        e.fields,
        e.service.as_deref()
    ])?;
    Ok(conn.last_insert_rowid())
}

fn insert_transfer(tx: &Connection, t: &TransferRow) -> rusqlite::Result<()> {
    // Measured here, after the queue wait, so recorded latency includes the
    // time the RPC spent waiting to be recorded.
    let duration_us = t.started.elapsed().as_micros() as i64;
    tx.prepare_cached(
        "INSERT INTO transfers (started_at, duration_us, peer, method, direction, object_id, \
         outcome, bytes, grpc_status, error, request, response, service_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?
    .execute(params![
        t.started_at,
        duration_us,
        t.peer,
        t.method,
        t.direction,
        t.object_id,
        t.outcome,
        t.bytes as i64,
        t.grpc_status,
        t.error,
        t.request,
        t.response,
        t.service.as_deref(),
    ])?;
    let id = tx.last_insert_rowid();
    {
        let mut stmt = tx.prepare_cached("INSERT OR IGNORE INTO payload_blobs (digest, data) VALUES (?, ?)")?;
        for (digest, data) in &t.inline {
            stmt.execute(params![digest, data])?;
        }
    }
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO transfer_payloads (transfer_id, seq, digest, label, bytes) VALUES (?, ?, ?, ?, ?)",
        )?;
        for (seq, p) in t.payloads.iter().enumerate() {
            stmt.execute(params![id, seq as i64, p.id, p.label, p.bytes as i64])?;
        }
    }
    let event = EventRow {
        time: now_ms(),
        level: if t.outcome == "error" { "ERROR" } else { "INFO" }.to_owned(),
        kind: "rpc",
        message: format!("{} {} {}", t.method, t.outcome, human_bytes(t.bytes)),
        peer: Some(t.peer.clone()),
        fields: json!({
            "method": t.method,
            "direction": t.direction,
            "outcome": t.outcome,
            "object_id": t.object_id,
            "bytes": t.bytes,
            "duration_us": duration_us,
            "grpc_status": t.grpc_status,
            "error": t.error,
        })
        .to_string(),
        service: t.service.clone(),
    };
    insert_event(tx, &event, Some(id))?;
    let (up, down) = if t.direction == "up" {
        (t.bytes as i64, 0)
    } else {
        (0, t.bytes as i64)
    };
    let error = (t.outcome == "error") as i64;
    let hit = (t.method == "KV.GetValue" && t.outcome == "hit") as i64;
    let miss = (t.method == "KV.GetValue" && t.outcome == "miss") as i64;
    tx.prepare_cached(
        "UPDATE summary SET \
           up_bytes = up_bytes + ?1, down_bytes = down_bytes + ?2, \
           transfers = transfers + 1, errors = errors + ?3, \
           kv_hit = kv_hit + ?4, kv_miss = kv_miss + ?5 \
         WHERE id = 1",
    )?
    .execute(params![up, down, error, hit, miss])?;
    if let Some(service) = t.service.as_deref() {
        tx.prepare_cached(
            "INSERT INTO service_summary (service_id, up_bytes, down_bytes, transfers, errors, kv_hit, kv_miss) \
             VALUES (?6, ?1, ?2, 1, ?3, ?4, ?5) \
             ON CONFLICT(service_id) DO UPDATE SET \
               up_bytes = up_bytes + ?1, down_bytes = down_bytes + ?2, \
               transfers = transfers + 1, errors = errors + ?3, \
               kv_hit = kv_hit + ?4, kv_miss = kv_miss + ?5",
        )?
        .execute(params![up, down, error, hit, miss, service])?;
    }
    Ok(())
}

/// Which part of a service's history [`Journal::clear_history`] removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryScope {
    /// Transfer records, their payload refs and the service's statistics.
    Transfers,
    /// The service's events only.
    Events,
}

/// Runs inside the writer's transaction. Ids stay monotonic (AUTOINCREMENT) and
/// payload bytes are left alone: an in-flight RPC may still be about to
/// reference a digest, and unreferenced payloads are never listed anyway.
fn clear_rows(tx: &Connection, service: &str, scope: HistoryScope) -> rusqlite::Result<()> {
    match scope {
        HistoryScope::Events => {
            tx.execute("DELETE FROM events WHERE service_id = ?", [service])?;
        }
        HistoryScope::Transfers => {
            // The global totals lose exactly what this service's transfers contributed.
            tx.execute(
                "UPDATE summary SET \
                   up_bytes = MAX(0, up_bytes - t.up), down_bytes = MAX(0, down_bytes - t.down), \
                   transfers = MAX(0, transfers - t.n), errors = MAX(0, errors - t.err), \
                   kv_hit = MAX(0, kv_hit - t.hit), kv_miss = MAX(0, kv_miss - t.miss) \
                 FROM (SELECT \
                   COALESCE(SUM(CASE WHEN direction = 'up' THEN bytes END), 0) AS up, \
                   COALESCE(SUM(CASE WHEN direction = 'down' THEN bytes END), 0) AS down, \
                   COUNT(*) AS n, \
                   COUNT(CASE WHEN outcome = 'error' THEN 1 END) AS err, \
                   COUNT(CASE WHEN method = 'KV.GetValue' AND outcome = 'hit' THEN 1 END) AS hit, \
                   COUNT(CASE WHEN method = 'KV.GetValue' AND outcome = 'miss' THEN 1 END) AS miss \
                   FROM transfers WHERE service_id = ?1) AS t \
                 WHERE summary.id = 1",
                [service],
            )?;
            tx.execute(
                "UPDATE events SET transfer_id = NULL WHERE transfer_id IN (SELECT id FROM transfers WHERE service_id = ?)",
                [service],
            )?;
            tx.execute(
                "DELETE FROM transfer_payloads WHERE transfer_id IN (SELECT id FROM transfers WHERE service_id = ?)",
                [service],
            )?;
            tx.execute("DELETE FROM transfers WHERE service_id = ?", [service])?;
            tx.execute("DELETE FROM service_summary WHERE service_id = ?", [service])?;
        }
    }
    Ok(())
}

fn insert_row(tx: &Connection, row: &Pending) -> rusqlite::Result<()> {
    match row {
        Pending::Transfer(t) => insert_transfer(tx, t),
        Pending::Event(e) => insert_event(tx, e, None).map(drop),
        Pending::Flush => Ok(()),
        Pending::Clear { service, scope } => clear_rows(tx, service, *scope),
    }
}

fn commit(conn: &mut Connection, rows: &[Job]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for job in rows {
        insert_row(&tx, &job.row)?;
    }
    tx.commit()
}

/// Group commit: takes whatever is queued (up to [`MAX_BATCH`]) and writes it
/// in one transaction, so N concurrent RPCs cost one commit, not N lock
/// handoffs and N commits. If the batch fails, rows are retried one by one so
/// a single bad row cannot fail its neighbours.
fn writer_loop(mut conn: Connection, rx: mpsc::Receiver<Job>) {
    let mut batch = Vec::with_capacity(MAX_BATCH);
    while let Ok(first) = rx.recv() {
        batch.push(first);
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(job) => batch.push(job),
                Err(_) => break,
            }
        }
        if commit(&mut conn, &batch).is_ok() {
            for job in batch.drain(..) {
                job.ack.send(Ok(()));
            }
            continue;
        }
        for job in batch.drain(..) {
            let result = commit(&mut conn, std::slice::from_ref(&job)).map_err(|e| e.to_string());
            job.ack.send(result);
        }
    }
}

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

fn open_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening journal {}", path.display()))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

/// Converting a rollback-journal database to WAL needs an exclusive lock, and while other
/// processes are opening the same file SQLite reports `SQLITE_BUSY` for it straight away,
/// regardless of `busy_timeout`. Retry for as long as the busy timeout would have waited.
fn enable_wal(conn: &Connection) -> Result<()> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result.context("switching journal to WAL"),
        }
    }
}

impl Journal {
    pub fn open(root: &Path) -> Result<Journal> {
        let payload_dir = root.join("payloads");
        fs::create_dir_all(&payload_dir).with_context(|| format!("creating journal dir {}", payload_dir.display()))?;
        let db_path = root.join("journal.sqlite3");
        let mut conn = open_connection(&db_path)?;
        enable_wal(&conn)?;
        // Several processes may open the same journal at once (CLI runs share
        // the default records directory); IMMEDIATE takes the write lock
        // before the schema is inspected, so they cannot both decide to ALTER.
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .context("locking journal schema")?;
        tx.execute_batch(SCHEMA).context("creating journal schema")?;
        for (table, index) in SERVICE_COLUMNS {
            let present: bool = tx.query_row(
                &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = 'service_id'"),
                [],
                |r| r.get::<_, i64>(0).map(|n| n > 0),
            )?;
            if !present {
                tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN service_id TEXT"))?;
            }
            tx.execute_batch(&format!("CREATE INDEX IF NOT EXISTS {index} ON {table}(service_id)"))?;
        }
        // One aggregate row, seeded once from existing history and then kept
        // current inside each recording transaction, so every process sharing
        // this directory sees the same totals. `INSERT OR IGNORE ... SELECT`
        // would still aggregate all history on every start, so check first.
        let seeded: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM summary WHERE id = 1)", [], |r| r.get(0))?;
        if !seeded {
            tx.execute(
                "INSERT INTO summary (id, up_bytes, down_bytes, transfers, errors, kv_hit, kv_miss) \
                 SELECT 1, \
                   COALESCE(SUM(CASE WHEN direction = 'up' THEN bytes END), 0), \
                   COALESCE(SUM(CASE WHEN direction = 'down' THEN bytes END), 0), \
                   COUNT(*), \
                   COUNT(CASE WHEN outcome = 'error' THEN 1 END), \
                   COUNT(CASE WHEN method = 'KV.GetValue' AND outcome = 'hit' THEN 1 END), \
                   COUNT(CASE WHEN method = 'KV.GetValue' AND outcome = 'miss' THEN 1 END) \
                 FROM transfers",
                [],
            )
            .context("seeding journal summary")?;
        }
        tx.commit()?;
        let reader = open_connection(&db_path)?;
        reader.pragma_update(None, "query_only", true)?;
        let (writer, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("journal-writer".into())
            .spawn(move || writer_loop(conn, rx))
            .context("starting journal writer")?;
        Ok(Journal {
            payload_dir,
            writer,
            reader: Mutex::new(reader),
            tmp_counter: AtomicU64::new(0),
            known: Mutex::new(HashMap::new()),
        })
    }

    /// A `tracing` layer that stores every event; add with `.with(...)`.
    pub fn layer(self: &Arc<Self>) -> JournalLayer {
        JournalLayer(self.clone())
    }

    /// Records RPCs on behalf of one service (`None`: CLI runs, unattributed).
    pub fn recorder(self: &Arc<Self>, service: Option<&str>) -> Recorder {
        Recorder {
            journal: self.clone(),
            service: service.map(Arc::from),
        }
    }

    fn submit(&self, row: Pending, ack: Ack) {
        if let Err(mpsc::SendError(job)) = self.writer.send(Job { row, ack }) {
            job.ack.send(Err("journal writer stopped".into()));
        }
    }

    /// Queues `row` and waits until it is committed.
    fn submit_sync(&self, row: Pending) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.submit(row, Ack::Sync(tx));
        blocking(|| rx.recv())
            .map_err(|_| anyhow!("journal writer stopped"))?
            .map_err(|e| anyhow!(e))
    }

    /// Records a management/lifecycle event, attributed to `service` if given.
    /// Returns once it is committed, so a following history read sees it.
    pub fn event(
        &self,
        service: Option<&str>,
        level: &str,
        kind: &'static str,
        message: &str,
        fields: Value,
    ) -> Result<()> {
        self.submit_sync(Pending::Event(EventRow {
            time: now_ms(),
            level: level.to_uppercase(),
            kind,
            message: message.to_owned(),
            peer: None,
            fields: fields.to_string(),
            service: service.map(Arc::from),
        }))
    }

    /// Waits until everything queued so far (including log lines, which are
    /// recorded without waiting) is committed. Call before the process exits.
    pub fn flush(&self) -> Result<()> {
        self.submit_sync(Pending::Flush)
    }

    /// Deletes `service`'s history and returns once committed. Rows queued
    /// before this call are committed first (and cleared); rows queued after
    /// it are kept.
    pub fn clear_history(&self, service: &str, scope: HistoryScope) -> Result<()> {
        self.submit_sync(Pending::Clear {
            service: service.to_owned(),
            scope,
        })
    }

    pub fn transfers(&self, query: &RecordQuery) -> Result<Page<Transfer>> {
        let mut f = Filter::default();
        f.search(
            &["method", "peer", "object_id", "error", "grpc_status"],
            nonempty(&query.search),
        );
        f.eq("direction", nonempty(&query.direction), true);
        f.eq("outcome", nonempty(&query.outcome), true);
        if let Some(svc) = nonempty(&query.service) {
            if svc.bytes().all(|b| b.is_ascii_alphanumeric()) {
                f.clauses.push("method LIKE ?".into());
                f.args.push(SqlValue::Text(format!("{}.%", svc.to_uppercase())));
            } else {
                f.clauses.push("0".into());
            }
        }
        f.since("started_at", query.since);
        f.eq("service_id", nonempty(&query.service_id), false);
        if let Some(max_id) = query.max_id {
            f.clauses.push("id <= ?".into());
            f.args.push(SqlValue::Integer(max_id));
        }
        let conn = self.reader.lock();
        // One snapshot for the count, the page and its payload refs.
        let tx = conn.unchecked_transaction()?;
        let mut out = page(&tx, "transfers", TRANSFER_COLS, &f, query, transfer_row)?;
        attach_payloads(&tx, &mut out.items)?;
        Ok(out)
    }

    pub fn transfer(&self, id: i64) -> Result<Option<Transfer>> {
        let conn = self.reader.lock();
        let tx = conn.unchecked_transaction()?;
        let mut found: Vec<Transfer> = tx
            .query_row(
                &format!("SELECT {TRANSFER_COLS} FROM transfers WHERE id = ?"),
                [id],
                transfer_row,
            )
            .optional()?
            .into_iter()
            .collect();
        attach_payloads(&tx, &mut found)?;
        Ok(found.pop())
    }

    pub fn events(&self, query: &RecordQuery) -> Result<Page<Event>> {
        let mut f = Filter::default();
        f.search(&["message", "fields", "peer", "kind"], nonempty(&query.search));
        let level = nonempty(&query.level).map(str::to_uppercase);
        f.eq("level", level.as_deref(), false);
        f.eq("kind", nonempty(&query.kind), false);
        f.since("time", query.since);
        f.eq("service_id", nonempty(&query.service_id), false);
        if let Some(t) = query.transfer_id {
            f.clauses.push("transfer_id = ?".into());
            f.args.push(SqlValue::Integer(t));
        }
        if let Some(max_id) = query.max_id {
            f.clauses.push("id <= ?".into());
            f.args.push(SqlValue::Integer(max_id));
        }
        let conn = self.reader.lock();
        let tx = conn.unchecked_transaction()?;
        page(&tx, "events", EVENT_COLS, &f, query, event_row)
    }

    pub fn summary(&self) -> Result<Summary> {
        let conn = self.reader.lock();
        let s = conn.query_row(
            "SELECT up_bytes, down_bytes, transfers, errors, kv_hit, kv_miss FROM summary WHERE id = 1",
            [],
            |r| {
                Ok(Summary {
                    up_bytes: r.get::<_, i64>(0)? as u64,
                    down_bytes: r.get::<_, i64>(1)? as u64,
                    transfers: r.get::<_, i64>(2)? as u64,
                    errors: r.get::<_, i64>(3)? as u64,
                    kv_hit: r.get::<_, i64>(4)? as u64,
                    kv_miss: r.get::<_, i64>(5)? as u64,
                })
            },
        )?;
        Ok(s)
    }

    /// Per-service totals, keyed by service id.
    pub fn service_summaries(&self) -> Result<HashMap<String, Summary>> {
        let conn = self.reader.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT service_id, up_bytes, down_bytes, transfers, errors, kv_hit, kv_miss FROM service_summary",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Summary {
                    up_bytes: r.get::<_, i64>(1)? as u64,
                    down_bytes: r.get::<_, i64>(2)? as u64,
                    transfers: r.get::<_, i64>(3)? as u64,
                    errors: r.get::<_, i64>(4)? as u64,
                    kv_hit: r.get::<_, i64>(5)? as u64,
                    kv_miss: r.get::<_, i64>(6)? as u64,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn payload_path(&self, id: &str) -> Result<PathBuf> {
        if !valid_digest(id) {
            bail!("invalid payload id");
        }
        Ok(self.payload_dir.join(&id[..2]).join(id))
    }

    /// Payload stored inline in the database (see [`INLINE_PAYLOAD`]).
    fn inline_payload(&self, id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .reader
            .lock()
            .prepare_cached("SELECT data FROM payload_blobs WHERE digest = ?")?
            .query_row([id], |r| r.get(0))
            .optional()?)
    }

    pub fn payload(&self, id: &str, offset: u64, limit: usize) -> Result<PayloadChunk> {
        let path = self.payload_path(id)?;
        let span = |total: u64| {
            let start = offset.min(total);
            (start, (total - start).min(limit.min(MAX_CHUNK) as u64) as usize)
        };
        let (start, total, buf) = match self.inline_payload(id)? {
            Some(data) => {
                let (start, want) = span(data.len() as u64);
                (
                    start,
                    data.len() as u64,
                    data[start as usize..start as usize + want].to_vec(),
                )
            }
            None => {
                let mut file = fs::File::open(&path).with_context(|| format!("opening payload {id}"))?;
                let total = file.metadata()?.len();
                let (start, want) = span(total);
                let mut buf = vec![0u8; want];
                file.seek(SeekFrom::Start(start))?;
                file.read_exact(&mut buf)?;
                (start, total, buf)
            }
        };
        Ok(PayloadChunk {
            id: id.to_owned(),
            offset: start,
            total,
            hex: hex::encode(&buf),
            text: String::from_utf8_lossy(&buf).into_owned(),
        })
    }

    /// Full record plus complete payload bytes (hex). Expensive by design.
    pub fn export(&self, id: i64) -> Result<Value> {
        let t = self.transfer(id)?.ok_or_else(|| anyhow!("transfer {id} not found"))?;
        let mut data = Vec::with_capacity(t.payloads.len());
        for p in &t.payloads {
            let bytes = match self.inline_payload(&p.id)? {
                Some(bytes) => bytes,
                None => fs::read(self.payload_path(&p.id)?).with_context(|| format!("reading payload {}", p.id))?,
            };
            data.push(json!({
                "id": p.id,
                "label": p.label,
                "bytes": bytes.len(),
                "hex": hex::encode(&bytes),
            }));
        }
        let mut v = serde_json::to_value(&t)?;
        v["payload_data"] = Value::Array(data);
        Ok(v)
    }

    /// Stores `data` under its blake3 digest unless already present.
    fn store_payload(&self, data: &[u8]) -> Result<String> {
        let digest = blake3::hash(data).to_hex().to_string();
        let path = self.payload_path(&digest)?;
        if path.exists() {
            return Ok(digest);
        }
        let dir = path.parent().expect("payload path has a parent");
        fs::create_dir_all(dir)?;
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = self.payload_dir.join(format!("tmp-{}-{n}", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(data)?;
            fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            let _ = fs::remove_file(&tmp);
            return Err(anyhow::Error::new(e).context(format!("writing payload {digest}")));
        }
        Ok(digest)
    }
}

/// Canonical gRPC status name, e.g. `InvalidArgument` -> `INVALID_ARGUMENT`.
fn grpc_name(code: tonic::Code) -> String {
    let mut out = String::new();
    for (i, c) in format!("{code:?}").chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

/// Starts per-RPC [`Rec`]s attributed to one service.
#[derive(Clone)]
pub struct Recorder {
    journal: Arc<Journal>,
    service: Option<Arc<str>>,
}

impl Recorder {
    /// Starts recording one RPC. `direction` is `"up"` or `"down"`.
    pub fn begin(&self, method: &'static str, direction: &'static str, peer: Peer) -> Rec {
        Rec {
            journal: self.journal.clone(),
            service: self.service.clone(),
            started: Instant::now(),
            started_at: now_ms(),
            peer: peer.to_string(),
            method,
            direction,
            object_id: None,
            outcome: "success",
            bytes: 0,
            error: None,
            request: Value::Null,
            response: Value::Null,
            payloads: Vec::new(),
            payload_error: None,
            inline: Vec::new(),
            files: Vec::new(),
            finished: false,
        }
    }
}

/// One in-flight RPC. Fill in what is known, then [`Rec::done`] before
/// responding. Dropping it unfinished (the client went away mid-request) is
/// recorded as a cancelled error.
pub struct Rec {
    journal: Arc<Journal>,
    service: Option<Arc<str>>,
    started: Instant,
    started_at: i64,
    peer: String,
    method: &'static str,
    direction: &'static str,
    object_id: Option<String>,
    outcome: &'static str,
    bytes: u64,
    error: Option<String>,
    pub request: Value,
    pub response: Value,
    payloads: Vec<PayloadRef>,
    payload_error: Option<anyhow::Error>,
    /// Small payloads, written by the journal writer with the transfer.
    inline: Vec<(String, Vec<u8>)>,
    /// Response files created for the client; deleted unless the RPC succeeds.
    files: Vec<PathBuf>,
    finished: bool,
}

impl Rec {
    /// Full object id / key as hex.
    pub fn object(&mut self, id: &[u8]) {
        self.object_id = Some(hex::encode(id));
    }

    /// `"success"`, `"hit"`, `"miss"` or `"error"` (default `"success"`).
    pub fn outcome(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }

    /// Marks the RPC as failed with a protocol-level error that is not a gRPC
    /// status (malformed input, error payloads in the response message).
    pub fn fail(&mut self, message: impl Into<String>) {
        self.outcome = "error";
        self.error = Some(message.into());
    }

    fn push_payload(&mut self, label: &str, digest: &str, len: u64) {
        self.bytes += len;
        self.payloads.push(PayloadRef {
            id: digest.to_owned(),
            label: label.to_owned(),
            bytes: len,
        });
    }

    /// Stores `data` as a deduplicated payload and returns its digest.
    pub fn payload(&mut self, label: &str, data: &[u8]) -> String {
        if data.len() <= INLINE_PAYLOAD {
            let digest = blake3::hash(data).to_hex().to_string();
            self.push_payload(label, &digest, data.len() as u64);
            if !self.inline.iter().any(|(d, _)| *d == digest) {
                self.inline.push((digest.clone(), data.to_vec()));
            }
            return digest;
        }
        match blocking(|| self.journal.store_payload(data)) {
            Ok(digest) => {
                self.push_payload(label, &digest, data.len() as u64);
                digest
            }
            Err(e) => {
                self.payload_error.get_or_insert(e);
                String::new()
            }
        }
    }

    /// [`Rec::payload`] for the blob of CAS object `id`. Ids are content
    /// derived, so once an object's payload is stored, later hits on it reuse
    /// the digest instead of re-hashing (possibly hundreds of MiB of) bytes.
    pub fn object_payload(&mut self, label: &str, id: &[u8], data: &[u8]) -> String {
        if data.len() <= INLINE_PAYLOAD {
            // Cheap to hash, and an inline blob's presence is not checkable here.
            return self.payload(label, data);
        }
        let known = self.journal.known.lock().get(id).cloned();
        if let Some((digest, len)) = known {
            if len == data.len() as u64 && self.journal.payload_path(&digest).is_ok_and(|p| p.exists()) {
                self.push_payload(label, &digest, len);
                return digest;
            }
        }
        let digest = self.payload(label, data);
        self.remember(id, &digest, data.len());
        digest
    }

    /// Notes that CAS object `id` has the blob stored as payload `digest`
    /// (call after an upload, once the object id is known).
    pub fn remember(&self, id: &[u8], digest: &str, len: usize) {
        if digest.is_empty() || len <= INLINE_PAYLOAD {
            return;
        }
        let mut known = self.journal.known.lock();
        if known.len() >= KNOWN_OBJECTS {
            known.clear();
        }
        known.insert(id.into(), (digest.to_owned(), len as u64));
    }

    /// Registers a `file_path` response, so it is deleted again if the RPC
    /// ends up failing or cancelled and the client never receives the path.
    pub fn track(&mut self, bytes: &CasBytes) {
        if let Some(cas_bytes::Contents::FilePath(p)) = &bytes.contents {
            self.files.push(PathBuf::from(p));
        }
    }

    /// Persists the record before the handler's `res` is returned. A recording
    /// failure turns a successful response into `Internal` (the failure is also
    /// stored in the transfer's `error` where possible); an RPC that already
    /// failed keeps its own status.
    pub async fn done<T>(mut self, res: Result<T, Status>) -> Result<T, Status> {
        let (row, mut failure) = self.finish(res.as_ref().err());
        let (tx, rx) = oneshot::channel();
        self.journal.submit(row, Ack::Async(tx));
        if let Err(e) = rx.await.unwrap_or_else(|_| Err("journal writer stopped".into())) {
            let msg = format!("recording failed: {e}");
            failure = Some(match failure {
                Some(prev) => format!("{prev}; {msg}"),
                None => msg,
            });
        }
        if let Some(msg) = &failure {
            tracing::warn!("{} {msg}", self.method);
        }
        match (failure, res) {
            (Some(msg), Ok(_)) => Err(Status::internal(msg)),
            (_, res) => {
                if res.is_ok() {
                    // Delivered: the files are the client's now.
                    self.files.clear();
                }
                res
            }
        }
    }

    /// Builds the row to persist; returns it with a description of any
    /// payload-recording failure.
    fn finish(&mut self, status: Option<&Status>) -> (Pending, Option<String>) {
        self.finished = true;
        let mut grpc_status = match status {
            Some(s) => {
                self.outcome = "error";
                self.error.get_or_insert_with(|| s.message().to_owned());
                grpc_name(s.code())
            }
            None => "OK".to_owned(),
        };
        let mut failure = None;
        if let Some(e) = self.payload_error.take() {
            let msg = format!("recording failed: {e:#}");
            self.outcome = "error";
            self.error = Some(match self.error.take() {
                Some(prev) => format!("{prev}; {msg}"),
                None => msg.clone(),
            });
            failure = Some(msg);
            if status.is_none() {
                grpc_status = "INTERNAL".into();
            }
        }
        let row = TransferRow {
            started: self.started,
            started_at: self.started_at,
            peer: std::mem::take(&mut self.peer),
            method: self.method,
            direction: self.direction,
            object_id: self.object_id.take(),
            outcome: self.outcome,
            bytes: self.bytes,
            grpc_status,
            error: self.error.take(),
            request: self.request.to_string(),
            response: self.response.to_string(),
            payloads: std::mem::take(&mut self.payloads),
            inline: std::mem::take(&mut self.inline),
            service: self.service.clone(),
        };
        (Pending::Transfer(Box::new(row)), failure)
    }
}

impl Drop for Rec {
    fn drop(&mut self) {
        if !self.finished {
            self.error = Some("request cancelled before completion".into());
            let (row, _) = self.finish(Some(&Status::cancelled("request cancelled before completion")));
            self.journal.submit(row, Ack::None);
        }
        for path in self.files.drain(..) {
            let _ = fs::remove_file(path);
        }
    }
}

/// Describes a `CASBytes` for the request/response JSON without its contents.
/// Attach the payload digest afterwards with `json["payload"] = ...`.
pub fn blob_json(bytes: Option<&CasBytes>) -> Value {
    match bytes.and_then(|b| b.contents.as_ref()) {
        Some(cas_bytes::Contents::Data(d)) => json!({ "kind": "data", "bytes": d.len() }),
        Some(cas_bytes::Contents::FilePath(p)) => json!({ "kind": "file_path", "file_path": p }),
        None => json!({ "kind": "none", "bytes": 0 }),
    }
}

/// Reference ids (full hex) for request/response JSON.
pub fn refs_json<'a>(ids: impl IntoIterator<Item = &'a [u8]>) -> Value {
    Value::Array(ids.into_iter().map(|i| Value::String(hex::encode(i))).collect())
}

/// Records each KV entry as a labelled payload (`<prefix>entry:<name>`, sorted
/// by name) and returns `[{name, bytes, payload}]` for the JSON body.
pub fn entries_json<'a>(
    rec: &mut Rec,
    prefix: &str,
    entries: impl IntoIterator<Item = (&'a String, &'a Vec<u8>)>,
) -> Value {
    let mut sorted: Vec<_> = entries.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    Value::Array(
        sorted
            .into_iter()
            .map(|(name, data)| {
                let digest = rec.payload(&format!("{prefix}entry:{name}"), data);
                json!({ "name": name, "bytes": data.len(), "payload": digest })
            })
            .collect(),
    )
}

/// See [`Journal::layer`].
pub struct JournalLayer(Arc<Journal>);

/// Span extension: the service a span (and everything inside it) belongs to.
struct SpanService(Arc<str>);

#[derive(Default)]
struct ServiceVisitor(Option<String>);

impl Visit for ServiceVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == SERVICE_SPAN_FIELD {
            self.0 = Some(format!("{value:?}").trim_matches('"').to_owned());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == SERVICE_SPAN_FIELD {
            self.0 = Some(value.to_owned());
        }
    }
}

#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: Map<String, Value>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = text;
        } else {
            self.fields.insert(field.name().to_owned(), Value::String(text));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else {
            self.fields
                .insert(field.name().to_owned(), Value::String(value.to_owned()));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for JournalLayer {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut v = ServiceVisitor::default();
        attrs.record(&mut v);
        if let (Some(service), Some(span)) = (v.0, ctx.span(id)) {
            span.extensions_mut().insert(SpanService(service.into()));
        }
    }

    /// Queues the line for the writer without waiting: logging must never
    /// stall the RPC that emits it (failures are reported on stderr).
    fn on_event(&self, event: &TracingEvent<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut v = FieldVisitor::default();
        event.record(&mut v);
        v.fields
            .insert("target".to_owned(), Value::String(meta.target().to_owned()));
        let peer = v.fields.get("peer").and_then(Value::as_str).map(str::to_owned);
        let service = ctx.event_scope(event).and_then(|scope| {
            scope
                .into_iter()
                .find_map(|span| span.extensions().get::<SpanService>().map(|s| s.0.clone()))
        });
        let row = EventRow {
            time: now_ms(),
            level: meta.level().as_str().to_owned(),
            kind: "log",
            message: v.message,
            peer,
            fields: Value::Object(v.fields).to_string(),
            service,
        };
        self.0.submit(Pending::Event(row), Ack::None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swiftcache-journal-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bytes_of(journal: &Journal, id: &str) -> Vec<u8> {
        hex::decode(journal.payload(id, 0, MAX_CHUNK).unwrap().hex).unwrap()
    }

    /// Concurrent RPCs are group-committed: every one is recorded exactly once,
    /// attributed to its service, with readable inline and file payloads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_records_commit_once_with_payloads() {
        let journal = Arc::new(Journal::open(&temp_root("concurrent")).unwrap());
        let recorder = journal.recorder(Some("svc-a"));
        let tasks: Vec<_> = (0..200u32)
            .map(|i| {
                let recorder = recorder.clone();
                tokio::spawn(async move {
                    let mut rec = recorder.begin("CAS.Save", "up", Peer::Pid(i));
                    rec.payload("small", format!("small-{i}").as_bytes());
                    rec.payload("large", &vec![i as u8; INLINE_PAYLOAD + 1 + i as usize]);
                    rec.done(Ok::<_, Status>(())).await.unwrap();
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        let page = journal
            .transfers(&RecordQuery {
                service_id: Some("svc-a".into()),
                limit: Some(200),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total, 200);
        assert_eq!(journal.summary().unwrap().transfers, 200);
        assert_eq!(journal.service_summaries().unwrap()["svc-a"].transfers, 200);
        for t in &page.items {
            let i: u32 = t.peer.trim_start_matches("pid:").parse().unwrap();
            assert_eq!(t.payloads.len(), 2);
            assert_eq!(bytes_of(&journal, &t.payloads[0].id), format!("small-{i}").as_bytes());
            let large = &t.payloads[1];
            assert_eq!(large.bytes as usize, INLINE_PAYLOAD + 1 + i as usize);
            assert_eq!(journal.payload(&large.id, 0, 1).unwrap().total, large.bytes);
        }
        let rpc_events = journal
            .events(&RecordQuery {
                kind: Some("rpc".into()),
                service_id: Some("svc-a".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rpc_events.total, 200);
    }

    /// A request dropped mid-flight is recorded as cancelled, and the response
    /// file it created for the client is removed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_rec_is_recorded_and_removes_response_file() {
        let root = temp_root("cancelled");
        let journal = Arc::new(Journal::open(&root).unwrap());
        let file = root.join("response.blob");
        fs::write(&file, b"partial").unwrap();
        let mut rec = journal.recorder(None).begin("CAS.Load", "down", Peer::Unknown);
        rec.track(&CasBytes {
            contents: Some(cas_bytes::Contents::FilePath(file.to_string_lossy().into_owned())),
        });
        drop(rec);
        journal.flush().unwrap();
        assert!(!file.exists());
        let page = journal.transfers(&RecordQuery::default()).unwrap();
        assert_eq!(page.items[0].grpc_status, "CANCELLED");
        assert_eq!(page.items[0].outcome, "error");
    }

    /// A successful response keeps its file: it belongs to the client now.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn successful_rec_keeps_response_file() {
        let root = temp_root("delivered");
        let journal = Arc::new(Journal::open(&root).unwrap());
        let file = root.join("response.blob");
        fs::write(&file, b"data").unwrap();
        let mut rec = journal.recorder(None).begin("CAS.Load", "down", Peer::Unknown);
        rec.track(&CasBytes {
            contents: Some(cas_bytes::Contents::FilePath(file.to_string_lossy().into_owned())),
        });
        rec.done(Ok::<_, Status>(())).await.unwrap();
        assert!(file.exists());
    }

    /// Journals from before per-service attribution gain the column once, even
    /// when several processes open them at the same moment.
    #[test]
    fn concurrent_opens_migrate_legacy_journal_once() {
        let root = temp_root("legacy");
        let conn = Connection::open(root.join("journal.sqlite3")).unwrap();
        conn.execute_batch(
            "CREATE TABLE transfers (id INTEGER PRIMARY KEY AUTOINCREMENT, started_at INTEGER NOT NULL, \
             duration_us INTEGER NOT NULL, peer TEXT NOT NULL, method TEXT NOT NULL, direction TEXT NOT NULL, \
             object_id TEXT, outcome TEXT NOT NULL, bytes INTEGER NOT NULL, grpc_status TEXT NOT NULL, \
             error TEXT, request TEXT NOT NULL, response TEXT NOT NULL); \
             CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, time INTEGER NOT NULL, level TEXT NOT NULL, \
             kind TEXT NOT NULL, message TEXT NOT NULL, peer TEXT, transfer_id INTEGER, fields TEXT NOT NULL); \
             INSERT INTO transfers (started_at, duration_us, peer, method, direction, outcome, bytes, grpc_status, request, response) \
             VALUES (1, 1, 'p', 'KV.GetValue', 'down', 'hit', 7, 'OK', '{}', '{}');",
        )
        .unwrap();
        drop(conn);
        let opens: Vec<_> = (0..4)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || Journal::open(&root).map(drop))
            })
            .collect();
        for open in opens {
            open.join().unwrap().unwrap();
        }
        let journal = Journal::open(&root).unwrap();
        let summary = journal.summary().unwrap();
        assert_eq!((summary.transfers, summary.kv_hit, summary.down_bytes), (1, 1, 7));
        assert_eq!(
            journal.transfers(&RecordQuery::default()).unwrap().items[0].service_id,
            None
        );
    }

    async fn record(
        journal: &Arc<Journal>,
        service: Option<&str>,
        method: &'static str,
        direction: &'static str,
        outcome: &'static str,
        data: &[u8],
    ) {
        let mut rec = journal.recorder(service).begin(method, direction, Peer::Unknown);
        rec.outcome(outcome);
        rec.payload("body", data);
        rec.done(Ok::<_, Status>(())).await.unwrap();
    }

    async fn seeded(name: &str) -> Arc<Journal> {
        let journal = Arc::new(Journal::open(&temp_root(name)).unwrap());
        record(&journal, Some("a"), "CAS.Save", "up", "success", b"aaaa").await;
        record(&journal, Some("a"), "KV.GetValue", "down", "hit", b"hit").await;
        record(&journal, Some("a"), "KV.GetValue", "down", "miss", b"").await;
        record(&journal, Some("a"), "CAS.Load", "down", "error", b"x").await;
        record(&journal, Some("b"), "CAS.Save", "up", "success", b"bbbbbb").await;
        record(&journal, None, "KV.GetValue", "down", "hit", b"cli").await;
        journal
            .event(Some("a"), "INFO", "service", "a started", json!({}))
            .unwrap();
        journal
            .event(Some("b"), "INFO", "service", "b started", json!({}))
            .unwrap();
        journal
    }

    fn by_service(journal: &Journal, kind: Option<&str>, service: &str) -> Page<Event> {
        journal
            .events(&RecordQuery {
                kind: kind.map(Into::into),
                service_id: Some(service.into()),
                ..Default::default()
            })
            .unwrap()
    }

    fn transfers_of(journal: &Journal, service: &str) -> Page<Transfer> {
        journal
            .transfers(&RecordQuery {
                service_id: Some(service.into()),
                ..Default::default()
            })
            .unwrap()
    }

    /// Clearing transfers resets only that service's statistics and records;
    /// its events survive unlinked, others are untouched, and new recording works.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clearing_transfers_resets_one_service_and_keeps_recording() {
        let journal = seeded("clear-transfers").await;
        let last_id = journal.transfers(&RecordQuery::default()).unwrap().items[0].id;
        let payload_of_a = transfers_of(&journal, "a").items[0].payloads[0].id.clone();
        let before = journal.summary().unwrap();
        let a = journal.service_summaries().unwrap()["a"].clone();
        assert_eq!((a.transfers, a.errors, a.kv_hit, a.kv_miss), (4, 1, 1, 1));

        journal.clear_history("a", HistoryScope::Transfers).unwrap();

        assert_eq!(transfers_of(&journal, "a").total, 0);
        assert_eq!(transfers_of(&journal, "b").total, 1);
        assert!(!journal.service_summaries().unwrap().contains_key("a"));
        assert_eq!(journal.service_summaries().unwrap()["b"].up_bytes, 6);
        let after = journal.summary().unwrap();
        assert_eq!(after.transfers, before.transfers - a.transfers);
        assert_eq!(
            (after.up_bytes, after.down_bytes),
            (before.up_bytes - a.up_bytes, before.down_bytes - a.down_bytes)
        );
        assert_eq!((after.errors, after.kv_hit, after.kv_miss), (0, 1, 0));
        // Unattributed (CLI) and other-service records stay.
        assert_eq!(journal.transfers(&RecordQuery::default()).unwrap().total, 2);
        // Events stay but no longer point at removed transfers.
        let rpc = by_service(&journal, Some("rpc"), "a");
        assert_eq!(rpc.total, 4);
        assert!(rpc.items.iter().all(|e| e.transfer_id.is_none()));
        assert_eq!(by_service(&journal, Some("service"), "a").total, 1);
        assert!(by_service(&journal, Some("rpc"), "b")
            .items
            .iter()
            .all(|e| e.transfer_id.is_some()));
        // Payload storage is deliberately retained.
        assert_eq!(bytes_of(&journal, &payload_of_a), b"x");

        record(&journal, Some("a"), "CAS.Save", "up", "success", b"again").await;
        let fresh = transfers_of(&journal, "a");
        assert_eq!(fresh.total, 1);
        assert!(fresh.items[0].id > last_id, "ids stay monotonic after a clear");
        assert_eq!(journal.service_summaries().unwrap()["a"].transfers, 1);
        assert_eq!(journal.summary().unwrap().transfers, after.transfers + 1);
    }

    /// Clearing events removes only that service's events: transfers, payloads
    /// and statistics stay visible.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clearing_events_keeps_transfers_and_statistics() {
        let journal = seeded("clear-events").await;
        let summary = journal.summary().unwrap();
        let b_events = by_service(&journal, None, "b").total;

        journal.clear_history("a", HistoryScope::Events).unwrap();

        assert_eq!(by_service(&journal, None, "a").total, 0);
        assert_eq!(by_service(&journal, None, "b").total, b_events);
        let transfers = transfers_of(&journal, "a");
        assert_eq!(transfers.total, 4);
        assert_eq!(bytes_of(&journal, &transfers.items[3].payloads[0].id), b"aaaa");
        assert_eq!(journal.summary().unwrap().transfers, summary.transfers);
        assert_eq!(journal.service_summaries().unwrap()["a"].transfers, 4);

        record(&journal, Some("a"), "CAS.Save", "up", "success", b"more").await;
        assert_eq!(by_service(&journal, Some("rpc"), "a").total, 1);
    }
}
