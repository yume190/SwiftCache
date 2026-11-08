//! Transfers and Events pages: filtered/paged browsing, frozen snapshots, record inspector,
//! payload viewer, native exports and per-service history clears.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariant, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Disableable, Icon, IconName, Selectable, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::{Value, json};

use crate::api::query;
use crate::app::Workspace;
use crate::types::{
    AppEvent, Page, PageKind, PayloadChunk, Transfer, exact_bytes, format_bytes, format_duration, format_int,
    format_time,
};

const PAGE: usize = 50;
const EXPORT_PAGE: usize = 200;
const POLL: Duration = Duration::from_millis(1400);
const DEBOUNCE: Duration = Duration::from_millis(300);
const CHUNK: u64 = 4096;
const EXPORT_CHUNK: u64 = 65536;
const MAX_LINES: usize = 600;
const MAX_REFS: usize = 200;
const EXPAND_STEP: usize = 1000;
const KINDS: [&str; 4] = ["rpc", "service", "cleanup", "log"];
const LEVELS: [&str; 5] = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"];
const RANGES: [i64; 5] = [15 * 60_000, 60 * 60_000, 24 * 60 * 60_000, 7 * 24 * 60 * 60_000, 0];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Transfers,
    Events,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    TransferSearch,
    EventSearch,
    EventKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    Overview,
    Json,
    Payload,
    Events,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PayloadMode {
    Hex,
    Text,
}

/// Pinned newest id and clock; later pages and relative time ranges stop moving.
#[derive(Clone)]
struct Snapshot {
    max_id: i64,
    at: i64,
    /// Created implicitly by paging into history; ends when returning to the first page.
    auto: bool,
}

#[derive(Default)]
struct Meta {
    offset: usize,
    snap: Option<Snapshot>,
    visible_at: i64,
    stale: bool,
    seq: u64,
    task: Option<Task<()>>,
    last: Option<Instant>,
    error: Option<String>,
    exporting: bool,
    export_error: Option<String>,
    export_task: Option<Task<()>>,
}

impl Meta {
    fn invalidate(&mut self) {
        self.seq += 1;
        self.task = None;
        self.error = None;
        self.stale = true;
        self.last = None;
    }
}

#[derive(Clone, Default)]
struct TransferFilters {
    search: String,
    direction: String,
    service: String,
    outcome: String,
    range: i64,
}

impl TransferFilters {
    fn active(&self) -> bool {
        !self.search.is_empty()
            || !self.direction.is_empty()
            || !self.service.is_empty()
            || !self.outcome.is_empty()
            || self.range != 0
    }
    fn json(&self) -> Value {
        json!({"search":self.search,"direction":self.direction,"service":self.service,"outcome":self.outcome,"range":self.range})
    }
}

#[derive(Clone, Default)]
struct EventFilters {
    search: String,
    level: String,
    kind: String,
    range: i64,
}

impl EventFilters {
    fn active(&self) -> bool {
        !self.search.is_empty() || !self.level.is_empty() || !self.kind.is_empty() || self.range != 0
    }
    fn json(&self) -> Value {
        json!({"search":self.search,"level":self.level,"kind":self.kind,"range":self.range})
    }
}

struct PayloadView {
    index: usize,
    offset: u64,
    mode: PayloadMode,
    chunk: Option<PayloadChunk>,
    hex_lines: Vec<SharedString>,
    text_lines: Vec<SharedString>,
    error: Option<String>,
    exporting: bool,
    seq: u64,
    task: Option<Task<()>>,
    export_task: Option<Task<()>>,
}

impl PayloadView {
    fn new() -> Self {
        Self {
            index: 0,
            offset: 0,
            mode: PayloadMode::Hex,
            chunk: None,
            hex_lines: Vec::new(),
            text_lines: Vec::new(),
            error: None,
            exporting: false,
            seq: 0,
            task: None,
            export_task: None,
        }
    }
}

struct Detail {
    id: i64,
    transfer: Option<Transfer>,
    events: Vec<AppEvent>,
    error: Option<String>,
    tab: DetailTab,
    req_lines: Vec<SharedString>,
    resp_lines: Vec<SharedString>,
    payload: PayloadView,
    exporting: bool,
    tasks: Vec<Task<()>>,
    export_task: Option<Task<()>>,
}

struct SelectedEvent {
    event: AppEvent,
    lines: Vec<SharedString>,
    export_task: Option<Task<()>>,
}

pub struct RecordsState {
    transfers: Meta,
    events: Meta,
    transfers_data: Option<Page<Transfer>>,
    events_data: Option<Page<AppEvent>>,
    tf: TransferFilters,
    ef: EventFilters,
    t_search: Entity<InputState>,
    e_search: Entity<InputState>,
    e_kind: Entity<InputState>,
    placeholder_zh: Option<bool>,
    debounce: [Option<Task<()>>; 3],
    _subs: Vec<Subscription>,
    selected_transfer: Option<i64>,
    detail: Option<Detail>,
    detail_seq: u64,
    selected_event: Option<SelectedEvent>,
    link_error: Option<String>,
    link_task: Option<Task<()>>,
    /// Bumped whenever event selection changes, the inspector closes or history resets.
    link_seq: u64,
    /// Extra rows revealed beyond the initial cap, keyed by list/block id.
    expand: HashMap<String, usize>,
    focus: FocusHandle,
    t_scroll: ScrollHandle,
    e_scroll: ScrollHandle,
    /// Key of the open `<select>` look-alike dropdown, if any.
    select_open: Option<&'static str>,
}

impl RecordsState {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let t_search = cx.new(|cx| InputState::new(window, cx));
        let e_search = cx.new(|cx| InputState::new(window, cx));
        let e_kind = cx.new(|cx| InputState::new(window, cx));
        let mut subs = Vec::new();
        subs.push(cx.subscribe_in(
            &t_search,
            window,
            |this: &mut Workspace, state, ev: &InputEvent, _w, cx| {
                this.rec_input_event(Field::TransferSearch, state.read(cx).value().to_string(), ev, cx)
            },
        ));
        subs.push(cx.subscribe_in(
            &e_search,
            window,
            |this: &mut Workspace, state, ev: &InputEvent, _w, cx| {
                this.rec_input_event(Field::EventSearch, state.read(cx).value().to_string(), ev, cx)
            },
        ));
        subs.push(cx.subscribe_in(
            &e_kind,
            window,
            |this: &mut Workspace, state, ev: &InputEvent, _w, cx| {
                this.rec_input_event(Field::EventKind, state.read(cx).value().trim().to_string(), ev, cx)
            },
        ));
        Self {
            transfers: Meta::default(),
            events: Meta::default(),
            transfers_data: None,
            events_data: None,
            tf: TransferFilters::default(),
            ef: EventFilters::default(),
            t_search,
            e_search,
            e_kind,
            placeholder_zh: None,
            debounce: [None, None, None],
            _subs: subs,
            selected_transfer: None,
            detail: None,
            detail_seq: 0,
            selected_event: None,
            link_error: None,
            link_task: None,
            link_seq: 0,
            expand: HashMap::new(),
            focus: cx.focus_handle(),
            t_scroll: ScrollHandle::new(),
            e_scroll: ScrollHandle::new(),
            select_open: None,
        }
    }

    /// Drops rows, selection, snapshots and in-flight work for the previous connection. Filters persist.
    pub fn reset(&mut self) {
        self.reset_transfers_view();
        self.reset_events_view();
        self.link_error = None;
        self.select_open = None;
        self.link_task = None;
    }

    fn reset_transfers_view(&mut self) {
        let seq = self.transfers.seq + 1;
        self.transfers = Meta { seq, ..Meta::default() };
        self.transfers_data = None;
        self.selected_transfer = None;
        self.detail = None;
        self.detail_seq += 1;
    }

    fn reset_events_view(&mut self) {
        let seq = self.events.seq + 1;
        self.events = Meta { seq, ..Meta::default() };
        self.events_data = None;
        self.selected_event = None;
        self.link_seq += 1;
        self.link_task = None;
    }

    fn close_event(&mut self) {
        self.selected_event = None;
        self.link_seq += 1;
    }
}

// ---------------------------------------------------------------------------------------------
// helpers

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn clock(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%H:%M:%S%.3f").to_string())
        .unwrap_or_else(|| ms.to_string())
}

fn date(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn pad_id(id: i64) -> String {
    format!("#{id:06}")
}

fn method_label(m: &str) -> String {
    m.replace('.', " ")
}

fn pretty_lines(v: &Value) -> Vec<SharedString> {
    serde_json::to_string_pretty(v)
        .unwrap_or_default()
        .lines()
        .map(|l| SharedString::from(l.to_string()))
        .collect()
}

fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
    let raw = hex.as_bytes();
    if !raw.len().is_multiple_of(2) {
        return Err("payload hex length is odd".to_string());
    }
    raw.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|s| u8::from_str_radix(s, 16).ok())
                .ok_or_else(|| "payload hex is invalid".to_string())
        })
        .collect()
}

fn hex_lines(hex: &str, offset: u64) -> Result<Vec<SharedString>, String> {
    let bytes = decode_hex(hex)?;
    Ok(bytes
        .chunks(16)
        .enumerate()
        .map(|(row, chunk)| {
            let mut left = String::new();
            let mut ascii = String::new();
            for j in 0..16 {
                if let Some(b) = chunk.get(j) {
                    left.push_str(&format!("{b:02x} "));
                    ascii.push(if (0x20..0x7f).contains(b) { *b as char } else { '.' });
                } else {
                    left.push_str("   ");
                }
                if j == 7 {
                    left.push(' ');
                }
            }
            SharedString::from(format!("{:06x}  {left} | {ascii}", offset as usize + row * 16))
        })
        .collect())
}

fn same_transfers(a: &Page<Transfer>, b: &Page<Transfer>) -> bool {
    a.total == b.total
        && a.items.len() == b.items.len()
        && a.items.iter().zip(&b.items).all(|(x, y)| {
            x.id == y.id
                && x.outcome == y.outcome
                && x.bytes == y.bytes
                && x.duration_us == y.duration_us
                && x.error == y.error
                && x.payloads.len() == y.payloads.len()
        })
}

fn same_events(a: &Page<AppEvent>, b: &Page<AppEvent>) -> bool {
    a.total == b.total
        && a.items.len() == b.items.len()
        && a.items
            .iter()
            .zip(&b.items)
            .all(|(x, y)| x.id == y.id && x.message == y.message && x.transfer_id == y.transfer_id)
}

fn transfer_path(
    f: &TransferFilters,
    service_id: &str,
    at: i64,
    max_id: Option<i64>,
    limit: usize,
    offset: usize,
) -> String {
    query(
        "/api/transfers",
        &[
            ("search", f.search.clone()),
            ("direction", f.direction.clone()),
            ("service", f.service.clone()),
            ("outcome", f.outcome.clone()),
            (
                "since",
                if f.range > 0 {
                    (at - f.range).to_string()
                } else {
                    String::new()
                },
            ),
            ("service_id", service_id.to_string()),
            ("max_id", max_id.map(|v| v.to_string()).unwrap_or_default()),
            ("limit", limit.to_string()),
            ("offset", offset.to_string()),
        ],
    )
}

fn event_path(f: &EventFilters, service_id: &str, at: i64, max_id: Option<i64>, limit: usize, offset: usize) -> String {
    query(
        "/api/events",
        &[
            ("search", f.search.clone()),
            ("level", f.level.clone()),
            ("kind", f.kind.clone()),
            (
                "since",
                if f.range > 0 {
                    (at - f.range).to_string()
                } else {
                    String::new()
                },
            ),
            ("service_id", service_id.to_string()),
            ("max_id", max_id.map(|v| v.to_string()).unwrap_or_default()),
            ("limit", limit.to_string()),
            ("offset", offset.to_string()),
        ],
    )
}

/// Native save dialog first (cancel is a no-op), then build the contents, then write off the UI thread.
async fn save_json<F>(dialog: rfd::AsyncFileDialog, name: String, build: F) -> Result<bool, String>
where
    F: Future<Output = Result<Value, String>>,
{
    let Some(handle) = dialog
        .set_file_name(name)
        .add_filter("JSON", &["json"])
        .save_file()
        .await
    else {
        return Ok(false);
    };
    let value = build.await?;
    let text = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    let path = handle.path().to_path_buf();
    smol::unblock(move || std::fs::write(path, text))
        .await
        .map_err(|e| e.to_string())?;
    Ok(true)
}

type SelAct = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

struct Opt {
    label: SharedString,
    on: bool,
    act: SelAct,
}

fn opt(
    label: impl Into<SharedString>,
    on: bool,
    act: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> Opt {
    Opt {
        label: label.into(),
        on,
        act: Rc::new(act),
    }
}

/// Record-page colors copied out of the global palette so listeners can borrow `cx` freely.
#[derive(Clone)]
struct Pal {
    panel: Hsla,
    text: Hsla,
    muted: Hsla,
    border: Hsla,
    accent: Hsla,
    accent_bg: Hsla,
    purple: Hsla,
    blue: Hsla,
    danger: Hsla,
    danger_bg: Hsla,
    danger_border: Hsla,
    danger_text: Hsla,
    warn_bg: Hsla,
    warn_text: Hsla,
    info_bg: Hsla,
    info_border: Hsla,
    info_text: Hsla,
    field: Hsla,
    subtle: Hsla,
    chip: Hsla,
    row_line: Hsla,
    code_bg: Hsla,
    code_text: Hsla,
    mono: SharedString,
    v_plain: ButtonCustomVariant,
    v_danger: ButtonCustomVariant,
    v_link: ButtonCustomVariant,
    v_icon: ButtonCustomVariant,
    v_tab_on: ButtonCustomVariant,
    v_tab_off: ButtonCustomVariant,
    v_seg_on: ButtonCustomVariant,
    v_seg_off: ButtonCustomVariant,
    v_menu: ButtonCustomVariant,
    v_menu_on: ButtonCustomVariant,
}

fn pal(cx: &App) -> Pal {
    let u = crate::ui::palette(cx);
    Pal {
        panel: u.panel,
        text: u.text,
        muted: u.muted,
        border: u.border,
        accent: u.accent,
        accent_bg: u.accent_bg,
        purple: u.purple,
        blue: u.blue,
        danger: u.danger,
        danger_bg: u.danger_bg,
        danger_border: u.danger_border,
        danger_text: u.danger_text,
        warn_bg: u.warn_bg,
        warn_text: u.warn_text,
        info_bg: u.info_bg,
        info_border: u.info_border,
        info_text: u.info_text,
        field: u.field,
        subtle: u.subtle,
        chip: u.chip,
        row_line: u.row_line,
        code_bg: u.code_bg,
        code_text: u.code_text,
        mono: cx.theme().mono_font_family.clone(),
        v_plain: ButtonCustomVariant::new(cx)
            .color(u.panel)
            .foreground(u.text)
            .hover(u.subtle)
            .active(u.subtle),
        v_danger: ButtonCustomVariant::new(cx)
            .color(u.panel)
            .foreground(u.danger)
            .hover(u.subtle)
            .active(u.subtle),
        v_link: ButtonCustomVariant::new(cx).foreground(u.accent),
        v_icon: ButtonCustomVariant::new(cx)
            .foreground(u.muted)
            .hover(u.chip)
            .active(u.chip),
        v_tab_on: ButtonCustomVariant::new(cx).foreground(u.accent),
        v_tab_off: ButtonCustomVariant::new(cx).foreground(u.muted),
        v_seg_on: ButtonCustomVariant::new(cx)
            .color(u.panel)
            .foreground(u.accent)
            .hover(u.panel)
            .active(u.panel),
        v_seg_off: ButtonCustomVariant::new(cx).foreground(u.muted),
        v_menu: ButtonCustomVariant::new(cx)
            .foreground(u.text)
            .hover(u.subtle)
            .active(u.subtle),
        v_menu_on: ButtonCustomVariant::new(cx)
            .foreground(u.accent)
            .hover(u.subtle)
            .active(u.subtle),
    }
}

const W650: FontWeight = FontWeight(650.);

/// `.card`
fn card(p: &Pal) -> Div {
    v_flex()
        .min_w(px(0.))
        .rounded(px(10.))
        .border_1()
        .border_color(p.border)
        .bg(p.panel)
}

/// `.card.panel`
fn panel(p: &Pal) -> Div {
    card(p).px(px(20.)).py(px(18.)).gap(px(14.))
}

/// Non-shrinking wrapper so only the table yields when the card hits its max height.
fn fx(e: impl IntoElement) -> Div {
    div().flex_none().child(e)
}

fn muted(p: &Pal, text: impl Into<SharedString>) -> Div {
    div().text_color(p.muted).child(text.into())
}

/// `.muted.small`
fn small_muted(p: &Pal, text: impl Into<SharedString>) -> Div {
    div().text_size(px(12.)).text_color(p.muted).child(text.into())
}

/// `.label`
fn label(p: &Pal, text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(p.muted)
        .child(text.into())
}

/// `h2`
fn heading(text: impl Into<SharedString>) -> Div {
    div().text_size(px(16.)).font_weight(W650).child(text.into())
}

/// `.mono` (12.5px unless a table cell / `.small` overrides it).
fn mono(p: &Pal, size: f32, text: impl Into<SharedString>) -> Div {
    div().font_family(p.mono.clone()).text_size(px(size)).child(text.into())
}

/// `.empty` / `.empty.small`
fn empty(p: &Pal, text: impl Into<SharedString>, small: bool) -> Div {
    let d = div().text_center().text_color(p.muted);
    if small {
        d.px(px(8.)).py(px(18.)).text_size(px(13.)).child(text.into())
    } else {
        d.px(px(16.)).py(px(40.)).child(text.into())
    }
}

/// `.pill.ok` / `.pill.idle`
fn pill(p: &Pal, ok: bool, text: impl Into<SharedString>) -> Div {
    let (bg, fg) = if ok { (p.accent_bg, p.accent) } else { (p.chip, p.muted) };
    div()
        .px(px(10.))
        .py(px(4.))
        .rounded(px(6.))
        .bg(bg)
        .text_color(fg)
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .whitespace_nowrap()
        .child(text.into())
}

/// `.tag`
fn tag(bg: Hsla, fg: Hsla, text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .px(px(8.))
        .py(px(2.))
        .rounded(px(5.))
        .bg(bg)
        .text_color(fg)
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .whitespace_nowrap()
        .child(text.into())
}

fn outcome_tag(p: &Pal, outcome: &str, text: impl Into<SharedString>) -> Div {
    let (bg, fg) = match outcome {
        "success" | "hit" => (p.accent_bg, p.accent),
        "miss" => (p.warn_bg, p.warn_text),
        "error" => (p.danger_bg, p.danger),
        _ => (p.chip, p.muted),
    };
    tag(bg, fg, text)
}

fn level_tag(p: &Pal, level: &str) -> Div {
    let (bg, fg) = match level.to_ascii_lowercase().as_str() {
        "info" => (p.accent_bg, p.accent),
        "warn" => (p.warn_bg, p.warn_text),
        "error" => (p.danger_bg, p.danger),
        _ => (p.chip, p.muted),
    };
    tag(bg, fg, level.to_string())
}

/// `.banner.error` / `.banner.info`; callers add the text and any inline link as children.
fn banner(p: &Pal, info: bool) -> Div {
    let (bg, bc, fg) = if info {
        (p.info_bg, p.info_border, p.info_text)
    } else {
        (p.danger_bg, p.danger_border, p.danger_text)
    };
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(6.))
        .px(px(14.))
        .py(px(10.))
        .rounded(px(8.))
        .border_1()
        .border_color(bc)
        .bg(bg)
        .text_color(fg)
        .text_size(px(13.))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Bk {
    Normal,
    Danger,
}

/// `.btn` (36px) / `.btn.small` (28px) / `.btn.danger`: native Button (focus, Enter/Space, a11y) with CSS geometry.
fn btn(
    p: &Pal,
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    small: bool,
    kind: Bk,
    enabled: bool,
    f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let (h, pad, fs) = if small { (28., 10., 12.) } else { (36., 16., 13.) };
    let text: SharedString = text.into();
    let danger = kind == Bk::Danger;
    Button::new(id)
        .custom(if danger { p.v_danger } else { p.v_plain })
        .accessibility_label(text.clone())
        .flex_none()
        .h(px(h))
        .px(px(pad))
        .border_1()
        .border_color(if danger { p.danger_border } else { p.border })
        .rounded(px(7.))
        .disabled(!enabled)
        .on_click(f)
        .child(
            div()
                .text_size(px(fs))
                .font_weight(FontWeight::SEMIBOLD)
                .whitespace_nowrap()
                .child(text),
        )
}

/// `.link`
fn link(
    p: &Pal,
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let text: SharedString = text.into();
    Button::new(id)
        .custom(p.v_link)
        .accessibility_label(text.clone())
        .h(px(20.))
        .px(px(0.))
        .on_click(f)
        .child(div().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).child(text))
}

/// `.icon-btn` (close)
fn icon_btn(p: &Pal, id: impl Into<ElementId>, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Button {
    Button::new(id)
        .custom(p.v_icon)
        .accessibility_label("Close")
        .px(px(6.))
        .py(px(2.))
        .rounded(px(5.))
        .on_click(f)
        .child(div().text_size(px(20.)).line_height(px(20.)).child("×"))
}

/// `.tabs button` (inspector underline tab)
fn tab_btn(
    p: &Pal,
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    on: bool,
    f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let text: SharedString = text.into();
    Button::new(id)
        .custom(if on { p.v_tab_on } else { p.v_tab_off })
        .role(Role::Tab)
        .selected(on)
        .accessibility_label(text.clone())
        .px(px(10.))
        .py(px(8.))
        .mb(px(-1.))
        .rounded_none()
        .border_0()
        .border_b_2()
        .border_color(if on { p.accent } else { hsla(0., 0., 0., 0.) })
        .on_click(f)
        .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(text))
}

fn tabs_bar(p: &Pal) -> Div {
    h_flex()
        .items_end()
        .gap(px(2.))
        .border_b_1()
        .border_color(p.border)
        .flex_wrap()
}

/// `.seg button`
fn seg_btn(
    p: &Pal,
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    on: bool,
    f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let text: SharedString = text.into();
    Button::new(id)
        .custom(if on { p.v_seg_on } else { p.v_seg_off })
        .selected(on)
        .accessibility_label(text.clone())
        .h(px(26.))
        .px(px(14.))
        .rounded(px(6.))
        // A custom variant paints only 20% of its color at rest; the selected segment needs the full panel.
        .when(on, |b| b.shadow_sm().bg(p.panel))
        .on_click(f)
        .child(div().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).child(text))
}

fn seg(p: &Pal) -> Div {
    h_flex().flex_none().p(px(3.)).gap(px(2.)).rounded(px(8.)).bg(p.chip)
}

/// One `<dt>/<dd>` pair of `.kv` (96px keys, 13px).
fn kv(p: &Pal, key: impl Into<SharedString>, value: impl IntoElement) -> Div {
    h_flex()
        .gap(px(12.))
        .items_start()
        .text_size(px(13.))
        .child(div().w(px(96.)).flex_none().text_color(p.muted).child(key.into()))
        .child(div().flex_1().min_w(px(0.)).child(value))
}

/// `.json` (subtle background) / `.hex` (code background); both are `white-space: pre` with a 340px cap.
fn code_block(p: &Pal, id: impl Into<ElementId>, lines: &[SharedString], hex: bool, shown: usize) -> Stateful<Div> {
    let (bg, fg, bc) = if hex {
        (p.code_bg, p.code_text, p.code_bg)
    } else {
        (p.subtle, p.text, p.border)
    };
    div()
        .id(id)
        .max_h(px(340.))
        .overflow_scroll()
        .p(px(12.))
        .rounded(px(8.))
        .border_1()
        .border_color(bc)
        .bg(bg)
        .text_color(fg)
        .font_family(p.mono.clone())
        .text_size(px(12.5))
        .line_height(relative(1.55))
        .child(v_flex().items_start().children(lines.iter().take(shown).map(|l| {
            let text = if l.is_empty() {
                SharedString::from(" ")
            } else {
                l.clone()
            };
            div().flex_none().whitespace_nowrap().child(text)
        })))
}

/// Text field styled as `.input` (36px, field background).
fn text_input(p: &Pal, state: &Entity<InputState>, bg: Hsla) -> Input {
    Input::new(state)
        .cleanable(true)
        .bg(bg)
        .border_color(p.border)
        .rounded(px(7.))
        .min_h(px(36.))
        .text_size(px(14.))
}

impl Workspace {
    fn rec_limit(&self, key: &str, initial: usize) -> usize {
        initial + self.records.expand.get(key).copied().unwrap_or(0)
    }

    /// "Show more / Show all" footer for a truncated list; every row stays reachable in the UI.
    fn rec_more(
        &self,
        p: &Pal,
        key: String,
        shown: usize,
        total: usize,
        noun: &'static str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if total <= shown {
            return None;
        }
        let remaining = total - shown;
        let step = EXPAND_STEP.min(remaining);
        let zh = self.rec_zh();
        let noun_text = match (noun, zh) {
            ("rows", true) => "項",
            ("rows", false) => "rows",
            (_, true) => "行",
            (_, false) => "lines",
        };
        let (k_step, k_all) = (key.clone(), key.clone());
        Some(
            h_flex()
                .gap(px(8.))
                .flex_wrap()
                .child(small_muted(
                    p,
                    format!("… {} {}", format_int(remaining as u64), noun_text),
                ))
                .child(btn(
                    p,
                    SharedString::from(format!("more-{key}")),
                    format!("{} +{}", self.t("顯示更多", "Show more"), format_int(step as u64)),
                    true,
                    Bk::Normal,
                    true,
                    cx.listener(move |this, _, _, cx| {
                        *this.records.expand.entry(k_step.clone()).or_insert(0) += step;
                        cx.notify();
                    }),
                ))
                .child(btn(
                    p,
                    SharedString::from(format!("all-{key}")),
                    self.t("全部顯示", "Show all"),
                    true,
                    Bk::Normal,
                    true,
                    cx.listener(move |this, _, _, cx| {
                        *this.records.expand.entry(k_all.clone()).or_insert(0) += remaining;
                        cx.notify();
                    }),
                ))
                .into_any_element(),
        )
    }

    /// JSON (`hex = false`) or hex/text (`hex = true`) block with the "show more" footer.
    fn rec_lines(&self, p: &Pal, key: &str, lines: &[SharedString], hex: bool, cx: &mut Context<Self>) -> AnyElement {
        let shown = lines.len().min(self.rec_limit(key, MAX_LINES));
        let more = self.rec_more(p, key.to_string(), shown, lines.len(), "lines", cx);
        v_flex()
            .gap(px(6.))
            .child(code_block(p, SharedString::from(key.to_string()), lines, hex, shown))
            .when_some(more, |d, m| d.child(m))
            .into_any_element()
    }

    /// Opens/closes the `<select>` look-alike `key`; the transparent backdrop below closes it on any outside press.
    fn rec_toggle_select(&mut self, key: &'static str, cx: &mut Context<Self>) {
        self.records.select_open = if self.records.select_open == Some(key) {
            None
        } else {
            Some(key)
        };
        cx.notify();
    }

    /// Backdrop + option list for the open dropdown `key` (empty when closed). Absolute below its relative parent.
    fn rec_menu(
        &self,
        p: &Pal,
        key: &'static str,
        opts: Vec<Opt>,
        min_w: f32,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if self.records.select_open != Some(key) {
            return Vec::new();
        }
        let backdrop = deferred(
            div()
                .absolute()
                .top(px(-4000.))
                .left(px(-4000.))
                .w(px(8000.))
                .h(px(8000.))
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.records.select_open = None;
                        cx.notify();
                    }),
                ),
        )
        .with_priority(1);
        let menu = deferred(
            v_flex()
                .absolute()
                .top(px(40.))
                .left(px(0.))
                .min_w(px(min_w))
                .p(px(4.))
                .gap(px(1.))
                .rounded(px(8.))
                .border_1()
                .border_color(p.border)
                .bg(p.panel)
                .shadow_lg()
                .occlude()
                .children(opts.into_iter().enumerate().map(|(i, o)| {
                    let act = o.act.clone();
                    Button::new((key, i))
                        .custom(if o.on { p.v_menu_on } else { p.v_menu })
                        .selected(o.on)
                        .accessibility_label(o.label.clone())
                        .w_full()
                        .justify_start()
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(px(5.))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.records.select_open = None;
                            act(this, window, cx);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .text_size(px(13.))
                                .whitespace_nowrap()
                                .when(o.on, |d| d.font_weight(FontWeight::SEMIBOLD))
                                .child(o.label),
                        )
                })),
        )
        .with_priority(2);
        vec![backdrop.into_any_element(), menu.into_any_element()]
    }

    /// `.input.select`: like a native `<select>`, sized to its widest option (the others are laid out at zero height)
    /// with a trailing chevron.
    fn rec_select(&self, p: &Pal, key: &'static str, opts: Vec<Opt>, cx: &mut Context<Self>) -> Div {
        let current = opts.iter().find(|o| o.on).map(|o| o.label.clone()).unwrap_or_default();
        let open = self.records.select_open == Some(key);
        let sizer = v_flex().children(opts.iter().map(|o| {
            div()
                .whitespace_nowrap()
                .when(!o.on, |d| d.h(px(0.)).overflow_hidden())
                .child(o.label.clone())
        }));
        let menu = self.rec_menu(p, key, opts, 170., cx);
        div()
            .relative()
            .flex_none()
            .child(
                Button::new(key)
                    .custom(p.v_plain)
                    .accessibility_label(current)
                    .min_h(px(36.))
                    .px(px(12.))
                    .border_1()
                    .border_color(if open { p.accent } else { p.border })
                    .rounded(px(7.))
                    .on_click(cx.listener(move |this, _, _, cx| this.rec_toggle_select(key, cx)))
                    .child(
                        h_flex()
                            .gap(px(10.))
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(sizer)
                            .child(Icon::new(IconName::ChevronDown).size(px(14.)).text_color(p.text)),
                    ),
            )
            .children(menu)
    }
}

impl Workspace {
    fn rec_zh(&self) -> bool {
        self.t("1", "0") == "1"
    }

    fn rec_meta(&mut self, tab: Tab) -> &mut Meta {
        match tab {
            Tab::Transfers => &mut self.records.transfers,
            Tab::Events => &mut self.records.events,
        }
    }

    // -----------------------------------------------------------------------------------------
    // loading

    /// Loads the active records tab. Called by the parent poll; throttled and snapshot-aware.
    pub fn load_records(&mut self, cx: &mut Context<Self>) {
        match self.active_page {
            PageKind::Transfers => self.rec_load(Tab::Transfers, false, cx),
            PageKind::Events => self.rec_load(Tab::Events, false, cx),
            _ => {}
        }
    }

    fn rec_load(&mut self, tab: Tab, force: bool, cx: &mut Context<Self>) {
        let Some(service_id) = self.selected_id.clone() else {
            return;
        };
        let has_data = match tab {
            Tab::Transfers => self.records.transfers_data.is_some(),
            Tab::Events => self.records.events_data.is_some(),
        };
        {
            let meta = self.rec_meta(tab);
            if !force {
                if meta.task.is_some() {
                    return;
                }
                // A frozen snapshot only reloads on explicit navigation, never on the poll.
                if meta.snap.is_some() && has_data {
                    return;
                }
                if has_data && meta.last.is_some_and(|at| at.elapsed() < POLL) {
                    return;
                }
            }
        }
        let generation = self.generation;
        let api = self.api.clone();
        let (at, max_id, offset) = {
            let meta = self.rec_meta(tab);
            meta.seq += 1;
            meta.last = Some(Instant::now());
            (
                meta.snap.as_ref().map(|s| s.at).unwrap_or_else(now_ms),
                meta.snap.as_ref().map(|s| s.max_id),
                meta.offset,
            )
        };
        let seq = self.rec_meta(tab).seq;
        match tab {
            Tab::Transfers => {
                let path = transfer_path(&self.records.tf, &service_id, at, max_id, PAGE, offset);
                self.records.transfers.task = Some(cx.spawn(async move |this, cx| {
                    let result = api.request::<Page<Transfer>>("GET", &path, None).await;
                    let _ = this.update(cx, |this, cx| {
                        if this.generation != generation
                            || this.selected_id.as_deref() != Some(service_id.as_str())
                            || this.records.transfers.seq != seq
                        {
                            return;
                        }
                        this.records.transfers.task = None;
                        this.records.transfers.last = Some(Instant::now());
                        this.records.transfers.stale = false;
                        match result {
                            Ok(page) => {
                                this.records.transfers.error = None;
                                this.records.transfers.visible_at = at;
                                if this
                                    .records
                                    .transfers_data
                                    .as_ref()
                                    .is_none_or(|d| !same_transfers(d, &page))
                                {
                                    this.records.transfers_data = Some(page);
                                }
                            }
                            Err(e) => this.records.transfers.error = Some(e),
                        }
                        cx.notify();
                    });
                }));
            }
            Tab::Events => {
                let path = event_path(&self.records.ef, &service_id, at, max_id, PAGE, offset);
                self.records.events.task = Some(cx.spawn(async move |this, cx| {
                    let result = api.request::<Page<AppEvent>>("GET", &path, None).await;
                    let _ = this.update(cx, |this, cx| {
                        if this.generation != generation
                            || this.selected_id.as_deref() != Some(service_id.as_str())
                            || this.records.events.seq != seq
                        {
                            return;
                        }
                        this.records.events.task = None;
                        this.records.events.last = Some(Instant::now());
                        this.records.events.stale = false;
                        match result {
                            Ok(page) => {
                                this.records.events.error = None;
                                this.records.events.visible_at = at;
                                if this.records.events_data.as_ref().is_none_or(|d| !same_events(d, &page)) {
                                    this.records.events_data = Some(page);
                                }
                            }
                            Err(e) => this.records.events.error = Some(e),
                        }
                        cx.notify();
                    });
                }));
            }
        }
        cx.notify();
    }

    /// Query changed: rows are dropped and reloaded from the new boundary.
    fn rec_reload_fresh(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.rec_meta(tab).invalidate();
        match tab {
            Tab::Transfers => self.records.transfers_data = None,
            Tab::Events => self.records.events_data = None,
        }
        self.rec_load(tab, true, cx);
    }

    /// Filtering returns to the first page; an implicit history snapshot ends with it.
    fn rec_filters_changed(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let meta = self.rec_meta(tab);
        meta.offset = 0;
        if meta.snap.as_ref().is_some_and(|s| s.auto) {
            meta.snap = None;
        }
        self.rec_reload_fresh(tab, cx);
    }

    fn rec_first_id(&self, tab: Tab) -> i64 {
        match tab {
            Tab::Transfers => self
                .records
                .transfers_data
                .as_ref()
                .and_then(|d| d.items.first())
                .map(|t| t.id)
                .unwrap_or(0),
            Tab::Events => self
                .records
                .events_data
                .as_ref()
                .and_then(|d| d.items.first())
                .map(|e| e.id)
                .unwrap_or(0),
        }
    }

    fn rec_ready(&self, tab: Tab) -> bool {
        match tab {
            Tab::Transfers => self.records.transfers_data.is_some() && !self.records.transfers.stale,
            Tab::Events => self.records.events_data.is_some() && !self.records.events.stale,
        }
    }

    /// Reuses the boundary and clock of the displayed page, including records that arrived before the next poll.
    fn rec_freeze(&mut self, tab: Tab, auto: bool, next_offset: usize, cx: &mut Context<Self>) {
        if !self.rec_ready(tab) {
            return;
        }
        let max_id = self.rec_first_id(tab);
        let meta = self.rec_meta(tab);
        meta.snap = Some(Snapshot {
            max_id,
            at: meta.visible_at,
            auto,
        });
        meta.offset = next_offset;
        self.rec_reload_fresh(tab, cx);
    }

    fn rec_resume(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let meta = self.rec_meta(tab);
        meta.snap = None;
        meta.offset = 0;
        self.rec_reload_fresh(tab, cx);
    }

    fn rec_go_page(&mut self, tab: Tab, offset: usize, cx: &mut Context<Self>) {
        if !self.rec_ready(tab) {
            return;
        }
        let (snap, auto) = {
            let m = self.rec_meta(tab);
            (m.snap.is_some(), m.snap.as_ref().is_some_and(|s| s.auto))
        };
        if offset == 0 && auto {
            self.rec_resume(tab, cx);
        } else if offset > 0 && !snap {
            self.rec_freeze(tab, true, offset, cx);
        } else {
            self.rec_meta(tab).offset = offset;
            self.rec_reload_fresh(tab, cx);
        }
    }

    // -----------------------------------------------------------------------------------------
    // filter inputs

    fn rec_input_event(&mut self, field: Field, value: String, ev: &InputEvent, cx: &mut Context<Self>) {
        match ev {
            InputEvent::Change => {
                let idx = field as usize;
                self.records.debounce[idx] = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(DEBOUNCE).await;
                    let _ = this.update(cx, |this, cx| {
                        this.records.debounce[idx] = None;
                        this.rec_apply_input(field, value, cx);
                    });
                }));
            }
            InputEvent::PressEnter { .. } => {
                self.records.debounce[field as usize] = None;
                self.rec_apply_input(field, value, cx);
            }
            _ => {}
        }
    }

    fn rec_apply_input(&mut self, field: Field, value: String, cx: &mut Context<Self>) {
        let (tab, slot) = match field {
            Field::TransferSearch => (Tab::Transfers, &mut self.records.tf.search),
            Field::EventSearch => (Tab::Events, &mut self.records.ef.search),
            Field::EventKind => (Tab::Events, &mut self.records.ef.kind),
        };
        if *slot == value {
            return;
        }
        *slot = value;
        self.rec_filters_changed(tab, cx);
    }

    fn rec_set_tf(&mut self, f: impl FnOnce(&mut TransferFilters), cx: &mut Context<Self>) {
        f(&mut self.records.tf);
        self.rec_filters_changed(Tab::Transfers, cx);
    }

    fn rec_set_ef(&mut self, f: impl FnOnce(&mut EventFilters), cx: &mut Context<Self>) {
        f(&mut self.records.ef);
        self.rec_filters_changed(Tab::Events, cx);
    }

    fn rec_clear_filters(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        for d in self.records.debounce.iter_mut() {
            *d = None;
        }
        match tab {
            Tab::Transfers => {
                self.records.tf = TransferFilters::default();
                self.records.t_search.update(cx, |s, cx| s.set_value("", window, cx));
            }
            Tab::Events => {
                self.records.ef = EventFilters::default();
                self.records.e_search.update(cx, |s, cx| s.set_value("", window, cx));
                self.records.e_kind.update(cx, |s, cx| s.set_value("", window, cx));
            }
        }
        self.rec_filters_changed(tab, cx);
    }

    fn rec_sync_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let zh = self.rec_zh();
        if self.records.placeholder_zh == Some(zh) {
            return;
        }
        self.records.placeholder_zh = Some(zh);
        let t = |a: &'static str, b: &'static str| if zh { a } else { b };
        self.records.t_search.update(cx, |s, cx| {
            s.set_placeholder(
                t(
                    "搜尋完整 CAS ID、KV key、來源或 method…",
                    "Search full CAS ID, KV key, source, or method…",
                ),
                window,
                cx,
            )
        });
        self.records.e_search.update(cx, |s, cx| {
            s.set_placeholder(
                t(
                    "搜尋訊息、來源或欄位內容…",
                    "Search messages, sources, or field contents…",
                ),
                window,
                cx,
            )
        });
        self.records.e_kind.update(cx, |s, cx| {
            s.set_placeholder(t("類型（全部）", "Kind (all)"), window, cx)
        });
    }

    // -----------------------------------------------------------------------------------------
    // selection

    fn rec_focus_list(&self, window: &mut Window, cx: &mut App) {
        self.records.focus.focus(window, cx);
    }

    fn rec_key(&mut self, tab: Tab, key: &str, cx: &mut Context<Self>) {
        match key {
            "down" | "up" => {
                let down = key == "down";
                match tab {
                    Tab::Transfers => {
                        let Some(items) = self
                            .records
                            .transfers_data
                            .as_ref()
                            .map(|d| d.items.iter().map(|t| t.id).collect::<Vec<_>>())
                        else {
                            return;
                        };
                        if items.is_empty() {
                            return;
                        }
                        let cur = self
                            .records
                            .selected_transfer
                            .and_then(|id| items.iter().position(|x| *x == id));
                        let next = match (cur, down) {
                            (None, _) => 0,
                            (Some(i), true) => (i + 1).min(items.len() - 1),
                            (Some(i), false) => i.saturating_sub(1),
                        };
                        self.records.t_scroll.scroll_to_item(next);
                        self.select_transfer(items[next], cx);
                    }
                    Tab::Events => {
                        let Some(items) = self.records.events_data.as_ref().map(|d| d.items.clone()) else {
                            return;
                        };
                        if items.is_empty() {
                            return;
                        }
                        let cur = self
                            .records
                            .selected_event
                            .as_ref()
                            .and_then(|s| items.iter().position(|x| x.id == s.event.id));
                        let next = match (cur, down) {
                            (None, _) => 0,
                            (Some(i), true) => (i + 1).min(items.len() - 1),
                            (Some(i), false) => i.saturating_sub(1),
                        };
                        self.records.e_scroll.scroll_to_item(next);
                        self.select_event(items[next].clone(), cx);
                    }
                }
            }
            "escape" => match tab {
                Tab::Transfers => self.close_transfer(cx),
                Tab::Events => {
                    self.records.close_event();
                    cx.notify();
                }
            },
            _ => {}
        }
    }

    fn close_transfer(&mut self, cx: &mut Context<Self>) {
        self.records.selected_transfer = None;
        self.records.detail = None;
        self.records.detail_seq += 1;
        cx.notify();
    }

    fn select_transfer(&mut self, id: i64, cx: &mut Context<Self>) {
        self.records.link_error = None;
        self.records.selected_transfer = Some(id);
        self.load_detail(id, cx);
        cx.notify();
    }

    fn load_detail(&mut self, id: i64, cx: &mut Context<Self>) {
        self.records.expand.clear();
        self.records.detail_seq += 1;
        let seq = self.records.detail_seq;
        let generation = self.generation;
        let service_id = self.selected_id.clone();
        let mut detail = Detail {
            id,
            transfer: None,
            events: Vec::new(),
            error: None,
            tab: DetailTab::Overview,
            req_lines: Vec::new(),
            resp_lines: Vec::new(),
            payload: PayloadView::new(),
            exporting: false,
            tasks: Vec::new(),
            export_task: None,
        };
        let valid = move |this: &Workspace| {
            this.generation == generation
                && this.selected_id == service_id
                && this.records.detail_seq == seq
                && this.records.detail.as_ref().is_some_and(|d| d.id == id)
        };
        let api = self.api.clone();
        let valid_t = valid.clone();
        detail.tasks.push(cx.spawn(async move |this, cx| {
            let result = api
                .request::<Transfer>("GET", &format!("/api/transfers/{id}"), None)
                .await;
            let _ = this.update(cx, |this, cx| {
                if !valid_t(this) {
                    return;
                }
                let first_payload = {
                    let d = this.records.detail.as_mut().unwrap();
                    match result {
                        Ok(t) => {
                            d.req_lines = pretty_lines(&t.request);
                            d.resp_lines = pretty_lines(&t.response);
                            let has = !t.payloads.is_empty();
                            d.transfer = Some(t);
                            has
                        }
                        Err(e) => {
                            d.error = Some(e);
                            false
                        }
                    }
                };
                if first_payload {
                    this.load_payload_chunk(cx);
                }
                cx.notify();
            });
        }));
        let api = self.api.clone();
        let path = query(
            "/api/events",
            &[("transfer_id", id.to_string()), ("limit", "200".to_string())],
        );
        detail.tasks.push(cx.spawn(async move |this, cx| {
            let result = api.request::<Page<AppEvent>>("GET", &path, None).await;
            let _ = this.update(cx, |this, cx| {
                if !valid(this) {
                    return;
                }
                let d = this.records.detail.as_mut().unwrap();
                match result {
                    Ok(p) => d.events = p.items.into_iter().filter(|e| e.transfer_id == Some(id)).collect(),
                    Err(e) => {
                        d.error.get_or_insert(e);
                    }
                }
                cx.notify();
            });
        }));
        self.records.detail = Some(detail);
    }

    fn select_event(&mut self, event: AppEvent, cx: &mut Context<Self>) {
        self.records.link_seq += 1;
        self.records.expand.clear();
        self.records.link_error = None;
        let lines = serde_json::to_value(&event)
            .map(|v| pretty_lines(&v))
            .unwrap_or_default();
        self.records.selected_event = Some(SelectedEvent {
            event,
            lines,
            export_task: None,
        });
        cx.notify();
    }

    /// Cross-link: show a transfer in the Transfers tab.
    pub fn open_transfer(&mut self, id: i64, cx: &mut Context<Self>) {
        self.active_page = PageKind::Transfers;
        self.select_transfer(id, cx);
        self.rec_load(Tab::Transfers, false, cx);
    }

    /// Cross-link: show an event (by id) in the Events tab.
    pub fn open_event(&mut self, event_id: i64, cx: &mut Context<Self>) {
        let Some(service_id) = self.selected_id.clone() else {
            return;
        };
        self.active_page = PageKind::Events;
        self.records.link_error = None;
        self.rec_load(Tab::Events, false, cx);
        self.records.link_seq += 1;
        let link_seq = self.records.link_seq;
        let generation = self.generation;
        let api = self.api.clone();
        let zh = self.rec_zh();
        let path = query(
            "/api/events",
            &[
                ("service_id", service_id.clone()),
                ("max_id", event_id.to_string()),
                ("limit", "1".to_string()),
            ],
        );
        self.records.link_task = Some(cx.spawn(async move |this, cx| {
            let result = api.request::<Page<AppEvent>>("GET", &path, None).await;
            let _ = this.update(cx, |this, cx| {
                if this.records.link_seq != link_seq
                    || this.generation != generation
                    || this.selected_id.as_deref() != Some(service_id.as_str())
                {
                    return;
                }
                this.records.link_task = None;
                match result {
                    Ok(page) => match page.items.into_iter().find(|e| e.id == event_id) {
                        Some(ev) => this.select_event(ev, cx),
                        None => {
                            this.records.link_error = Some(if zh {
                                format!("找不到事件 #{event_id}（可能已被清除）。")
                            } else {
                                format!("Event #{event_id} was not found (it may have been cleared).")
                            })
                        }
                    },
                    Err(e) => this.records.link_error = Some(e),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    // -----------------------------------------------------------------------------------------
    // payload viewer

    fn load_payload_chunk(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.records.detail.as_mut() else { return };
        let Some(t) = d.transfer.as_ref() else { return };
        let Some(p) = t.payloads.get(d.payload.index) else {
            return;
        };
        let (id, offset) = (p.id.clone(), d.payload.offset);
        d.payload.seq += 1;
        d.payload.chunk = None;
        d.payload.error = None;
        let (seq, transfer_id) = (d.payload.seq, d.id);
        let generation = self.generation;
        let detail_seq = self.records.detail_seq;
        let api = self.api.clone();
        let path = query(
            &format!("/api/payload/{}", urlencode(&id)),
            &[("offset", offset.to_string()), ("limit", CHUNK.to_string())],
        );
        let task = cx.spawn(async move |this, cx| {
            let result = api.request::<PayloadChunk>("GET", &path, None).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.records.detail_seq != detail_seq {
                    return;
                }
                let Some(d) = this
                    .records
                    .detail
                    .as_mut()
                    .filter(|d| d.id == transfer_id && d.payload.seq == seq)
                else {
                    return;
                };
                d.payload.task = None;
                match result.and_then(|c| {
                    let hex = hex_lines(&c.hex, c.offset)?;
                    Ok((c, hex))
                }) {
                    Ok((c, hex)) => {
                        d.payload.text_lines = c.text.split('\n').map(|l| SharedString::from(l.to_string())).collect();
                        d.payload.hex_lines = hex;
                        d.payload.chunk = Some(c);
                    }
                    Err(e) => d.payload.error = Some(e),
                }
                cx.notify();
            });
        });
        if let Some(d) = self.records.detail.as_mut() {
            d.payload.task = Some(task);
        }
        cx.notify();
    }

    fn export_payload(&mut self, window: &Window, cx: &mut Context<Self>) {
        let zh = self.rec_zh();
        let Some(d) = self.records.detail.as_mut() else { return };
        if d.payload.exporting {
            return;
        }
        let Some(p) = d
            .transfer
            .as_ref()
            .and_then(|t| t.payloads.get(d.payload.index))
            .cloned()
        else {
            return;
        };
        d.payload.exporting = true;
        d.payload.error = None;
        let (transfer_id, detail_seq, generation) = (d.id, self.records.detail_seq, self.generation);
        let api = self.api.clone();
        let name = format!("payload-{}.json", p.id.chars().take(16).collect::<String>());
        let dialog = rfd::AsyncFileDialog::new().set_parent(window);
        let task = cx.spawn(async move |this, cx| {
            let result = save_json(dialog, name, async {
                let mut hex = String::new();
                let mut off = 0u64;
                while off < p.bytes {
                    let path = query(
                        &format!("/api/payload/{}", urlencode(&p.id)),
                        &[("offset", off.to_string()), ("limit", EXPORT_CHUNK.to_string())],
                    );
                    let c = api.request::<PayloadChunk>("GET", &path, None).await?;
                    if c.hex.is_empty() {
                        break;
                    }
                    off += (c.hex.len() / 2) as u64;
                    hex.push_str(&c.hex);
                }
                Ok(json!({"id":p.id,"label":p.label,"bytes":p.bytes,"hex":hex}))
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.records.detail_seq != detail_seq {
                    return;
                }
                let Some(d) = this.records.detail.as_mut().filter(|d| d.id == transfer_id) else {
                    return;
                };
                d.payload.exporting = false;
                d.payload.export_task = None;
                match result {
                    Ok(true) => this.notice(
                        if zh {
                            "已匯出 payload".into()
                        } else {
                            "Payload exported".into()
                        },
                        cx,
                    ),
                    Ok(false) => {}
                    Err(e) => d.payload.error = Some(e),
                }
                cx.notify();
            });
        });
        d.payload.export_task = Some(task);
        cx.notify();
    }

    // -----------------------------------------------------------------------------------------
    // exports

    fn export_transfer(&mut self, window: &Window, cx: &mut Context<Self>) {
        let zh = self.rec_zh();
        let Some(d) = self.records.detail.as_mut() else { return };
        if d.exporting {
            return;
        }
        d.exporting = true;
        let (id, detail_seq, generation) = (d.id, self.records.detail_seq, self.generation);
        let api = self.api.clone();
        let dialog = rfd::AsyncFileDialog::new().set_parent(window);
        d.export_task = Some(cx.spawn(async move |this, cx| {
            let result = save_json(dialog, format!("swiftcache-transfer-{id}.json"), async {
                api.request::<Value>("GET", &format!("/api/transfers/{id}/export"), None)
                    .await
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.records.detail_seq != detail_seq {
                    return;
                }
                let Some(d) = this.records.detail.as_mut().filter(|d| d.id == id) else {
                    return;
                };
                d.exporting = false;
                d.export_task = None;
                match result {
                    Ok(true) => this.notice(
                        if zh {
                            "已匯出傳輸詳細資料".into()
                        } else {
                            "Transfer exported".into()
                        },
                        cx,
                    ),
                    Ok(false) => {}
                    Err(e) => d.error = Some(e),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn export_event(&mut self, window: &Window, cx: &mut Context<Self>) {
        let zh = self.rec_zh();
        let Some(sel) = self.records.selected_event.as_mut() else {
            return;
        };
        if sel.export_task.is_some() {
            return;
        }
        let ev = sel.event.clone();
        let id = ev.id;
        let generation = self.generation;
        let dialog = rfd::AsyncFileDialog::new().set_parent(window);
        sel.export_task = Some(cx.spawn(async move |this, cx| {
            let result = save_json(dialog, format!("swiftcache-event-{id}.json"), async {
                serde_json::to_value(&ev).map_err(|e| e.to_string())
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                if let Some(s) = this.records.selected_event.as_mut().filter(|s| s.event.id == id) {
                    s.export_task = None;
                }
                match result {
                    Ok(true) => this.notice(
                        if zh {
                            "已匯出事件".into()
                        } else {
                            "Event exported".into()
                        },
                        cx,
                    ),
                    Ok(false) => {}
                    Err(e) => this.records.events.export_error = Some(e),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Exports every record matching the filters, respecting the frozen boundary and clock.
    fn export_filtered(&mut self, tab: Tab, window: &Window, cx: &mut Context<Self>) {
        let Some(service_id) = self.selected_id.clone() else {
            return;
        };
        let zh = self.rec_zh();
        let generation = self.generation;
        let api = self.api.clone();
        let tf = self.records.tf.clone();
        let ef = self.records.ef.clone();
        let meta = self.rec_meta(tab);
        if meta.exporting {
            return;
        }
        meta.exporting = true;
        meta.export_error = None;
        let snap = meta.snap.clone();
        let at = snap.as_ref().map(|s| s.at).unwrap_or_else(now_ms);
        let name = match tab {
            Tab::Transfers => "swiftcache-transfers.json",
            Tab::Events => "swiftcache-events.json",
        };
        let sid = service_id.clone();
        let dialog = rfd::AsyncFileDialog::new().set_parent(window);
        let task = cx.spawn(async move |this, cx| {
            let result = save_json(dialog, name.to_string(), async {
                // Pin the newest id first so records arriving while paging cannot shift later pages.
                let mut max_id = snap.as_ref().map(|s| s.max_id);
                let build = |max_id: Option<i64>, limit: usize, offset: usize| match tab {
                    Tab::Transfers => transfer_path(&tf, &sid, at, max_id, limit, offset),
                    Tab::Events => event_path(&ef, &sid, at, max_id, limit, offset),
                };
                if max_id.is_none() {
                    let probe = api.request::<Value>("GET", &build(None, 1, 0), None).await?;
                    // An empty probe pins an empty snapshot (id 0), so later arrivals cannot shift pages.
                    max_id = Some(probe["items"].get(0).and_then(|i| i["id"].as_i64()).unwrap_or(0));
                }
                let mut items: Vec<Value> = Vec::new();
                let mut total = u64::MAX;
                while (items.len() as u64) < total {
                    let page = api
                        .request::<Value>("GET", &build(max_id, EXPORT_PAGE, items.len()), None)
                        .await?;
                    total = page["total"].as_u64().unwrap_or(0);
                    let batch = page["items"].as_array().cloned().unwrap_or_default();
                    if batch.is_empty() {
                        break;
                    }
                    items.extend(batch);
                }
                let filters = match tab {
                    Tab::Transfers => tf.json(),
                    Tab::Events => ef.json(),
                };
                Ok(json!({"exported_at": now_ms(), "filters": filters, "total": items.len(), "items": items}))
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.selected_id.as_deref() != Some(service_id.as_str()) {
                    return;
                }
                let meta = this.rec_meta(tab);
                meta.exporting = false;
                meta.export_task = None;
                match result {
                    Ok(true) => this.notice(
                        if zh {
                            "已匯出篩選結果".into()
                        } else {
                            "Filtered results exported".into()
                        },
                        cx,
                    ),
                    Ok(false) => {}
                    Err(e) => this.rec_meta(tab).export_error = Some(e),
                }
                cx.notify();
            });
        });
        self.rec_meta(tab).export_task = Some(task);
        cx.notify();
    }

    // -----------------------------------------------------------------------------------------
    // history clear

    fn rec_open_clear(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.selected_id.is_none() {
            return;
        }
        let name = self
            .selected_service()
            .map(|s| crate::types::service_label(&s.config))
            .unwrap_or_default();
        let zh = self.rec_zh();
        let (title, body) = match (tab, zh) {
            (Tab::Transfers, true) => (
                "清除傳輸明細？",
                "這會刪除此連線的所有傳輸明細（不受目前篩選或時間範圍限制），並將上傳、下載、錯誤與 KV 命中率統計歸零。事件紀錄會保留，但指向已刪除傳輸的連結會被移除。\n已儲存的 payload、CAS / KV 快取與連線設定會保留。",
            ),
            (Tab::Events, true) => (
                "清除事件紀錄？",
                "這會刪除此連線的所有事件紀錄（不受目前篩選或時間範圍限制）。傳輸明細、流量統計與快取內容不受影響。\n傳輸明細、統計、已儲存的 payload、快取與連線設定都會保留。",
            ),
            (Tab::Transfers, false) => (
                "Clear transfers?",
                "This deletes all transfer records of this connection (regardless of the current filters or time range) and resets its upload, download, error, and KV hit-rate statistics. Events are kept, but their links to the deleted transfers are removed.\nSaved payloads, the CAS / KV cache, and connection settings are kept.",
            ),
            (Tab::Events, false) => (
                "Clear events?",
                "This deletes all events of this connection (regardless of the current filters or time range). Transfers, traffic statistics, and cache contents are not affected.\nTransfers, statistics, saved payloads, the cache, and connection settings are kept.",
            ),
        };
        let note = if zh {
            "服務會持續執行，清除期間或之後產生的新紀錄仍會出現。已儲存的 payload 檔案不會被刪除。此操作無法復原。取消不會修改任何資料。"
        } else {
            "The service keeps running, and new records created during or after the clear still appear. Saved payload files are not deleted. This cannot be undone. Canceling doesn’t change any data."
        };
        let description: SharedString = format!("{name}\n\n{body}\n\n{note}").into();
        let ok = if zh { "清除" } else { "Clear" };
        let cancel = if zh { "取消" } else { "Cancel" };
        let me = cx.entity();
        let target = self.selected_id.clone().unwrap_or_default();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let me = me.clone();
            let target = target.clone();
            alert
                .title(title)
                .description(description.clone())
                .ok_text(ok)
                .cancel_text(cancel)
                .show_cancel(true)
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    me.update(cx, |this, cx| this.rec_clear(tab, target.clone(), cx));
                    true
                })
        });
    }

    fn rec_clear(&mut self, tab: Tab, service_id: String, cx: &mut Context<Self>) {
        // The dialog's target is the service it was opened for; refuse if the selection moved or vanished.
        if self.selected_id.as_deref() != Some(service_id.as_str()) {
            return;
        }
        let zh = self.rec_zh();
        let generation = self.generation;
        let api = self.api.clone();
        let scope = match tab {
            Tab::Transfers => "transfers",
            Tab::Events => "events",
        };
        let sid = service_id.clone();
        self.perform(
            cx,
            async move {
                api.request::<Value>(
                    "POST",
                    "/api/history/clear",
                    Some(json!({"service": sid, "scope": scope})),
                )
                .await
                .map(|_| ())
            },
            move |this: &mut Workspace, result: Result<(), String>, cx: &mut Context<Workspace>| {
                if result.is_err() {
                    return;
                }
                this.refresh_status(cx);
                if tab == Tab::Events {
                    this.refresh_cleanup_history(cx);
                }
                if this.generation != generation || this.selected_id.as_deref() != Some(service_id.as_str()) {
                    return;
                }
                match tab {
                    Tab::Transfers => {
                        // Event links to deleted transfers are gone server-side; drop cached copies as well.
                        this.records.reset_transfers_view();
                        this.records.reset_events_view();
                    }
                    Tab::Events => {
                        this.records.reset_events_view();
                        // Linked-event lists in an open transfer inspector are stale now.
                        if let Some(id) = this.records.selected_transfer {
                            this.load_detail(id, cx);
                        }
                    }
                }
                this.records.link_error = None;
                this.rec_load(Tab::Transfers, true, cx);
                if this.active_page == PageKind::Events {
                    this.rec_load(Tab::Events, true, cx);
                }
                this.notice(
                    match (tab, zh) {
                        (Tab::Transfers, true) => "已清除傳輸明細並重設統計".to_string(),
                        (Tab::Events, true) => "已清除事件紀錄".to_string(),
                        (Tab::Transfers, false) => "Transfers cleared and statistics reset".to_string(),
                        (Tab::Events, false) => "Events cleared".to_string(),
                    },
                    cx,
                );
            },
        );
    }

    // -----------------------------------------------------------------------------------------
    // rendering: shared pieces

    fn rec_pager(&self, tab: Tab, total: u64, p: &Pal, cx: &mut Context<Self>) -> Div {
        let zh = self.rec_zh();
        let (offset, ready) = match tab {
            Tab::Transfers => (self.records.transfers.offset, self.rec_ready(tab)),
            Tab::Events => (self.records.events.offset, self.rec_ready(tab)),
        };
        let from = if total == 0 { 0 } else { offset + 1 };
        let to = (offset + PAGE).min(total as usize);
        let pages = (total as usize).div_ceil(PAGE).max(1);
        let page = offset / PAGE + 1;
        let shown = if zh {
            format!(
                "顯示 {}–{} / 共 {} 筆",
                format_int(from as u64),
                format_int(to as u64),
                format_int(total)
            )
        } else {
            format!(
                "Showing {}–{} of {}",
                format_int(from as u64),
                format_int(to as u64),
                format_int(total)
            )
        };
        let page_of = if zh {
            format!("第 {page} / {pages} 頁")
        } else {
            format!("Page {page} of {pages}")
        };
        let go = |label: &'static str, id: &'static str, target: usize, enabled: bool, cx: &mut Context<Self>| {
            btn(
                p,
                id,
                label,
                true,
                Bk::Normal,
                enabled && ready,
                cx.listener(move |this, _, _, cx| this.rec_go_page(tab, target, cx)),
            )
        };
        let (first, prev, next) = if zh {
            ("第一頁", "上一頁", "下一頁")
        } else {
            ("First page", "Previous", "Next")
        };
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .text_size(px(12.))
            .child(div().text_color(p.muted).child(shown))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(go(first, "pg-first", 0, offset > 0, cx))
                    .child(go(prev, "pg-prev", offset.saturating_sub(PAGE), offset > 0, cx))
                    .child(div().text_color(p.muted).child(page_of))
                    .child(go(next, "pg-next", offset + PAGE, offset + PAGE < total as usize, cx)),
            )
    }

    fn rec_freeze_row(
        &self,
        tab: Tab,
        p: &Pal,
        t: &dyn Fn(&'static str, &'static str) -> &'static str,
        cx: &mut Context<Self>,
    ) -> (AnyElement, bool) {
        let snap = match tab {
            Tab::Transfers => self.records.transfers.snap.is_some(),
            Tab::Events => self.records.events.snap.is_some(),
        };
        let ready = self.rec_ready(tab);
        let b = btn(
            p,
            "freeze",
            if snap {
                t("回到最新並繼續更新", "Back to latest & resume")
            } else {
                t("暫停並凍結清單", "Pause & freeze list")
            },
            false,
            Bk::Normal,
            snap || ready,
            cx.listener(move |this, _, _, cx| {
                let snapped = this.rec_meta(tab).snap.is_some();
                if snapped {
                    this.rec_resume(tab, cx);
                } else {
                    this.rec_freeze(tab, false, 0, cx);
                }
            }),
        );
        (b.into_any_element(), snap)
    }

    fn rec_link_banner(&self, p: &Pal, cx: &mut Context<Self>) -> Option<AnyElement> {
        let e = self.records.link_error.as_ref()?;
        Some(
            banner(p, false)
                .justify_between()
                .child(div().flex_1().min_w(px(0.)).child(e.clone()))
                .child(icon_btn(
                    p,
                    "link-dismiss",
                    cx.listener(|this, _, _, cx| {
                        this.records.link_error = None;
                        cx.notify();
                    }),
                ))
                .into_any_element(),
        )
    }

    /// The `<select aria-label="Time range">` of both filter rows.
    fn rec_range_select(
        &self,
        p: &Pal,
        tab: Tab,
        current: i64,
        t: &dyn Fn(&'static str, &'static str) -> &'static str,
        cx: &mut Context<Self>,
    ) -> Div {
        let opts = RANGES
            .iter()
            .map(|ms| {
                let ms = *ms;
                let text = match ms {
                    900_000 => t("最近 15 分鐘", "Last 15 minutes"),
                    3_600_000 => t("最近 1 小時", "Last hour"),
                    86_400_000 => t("最近 24 小時", "Last 24 hours"),
                    604_800_000 => t("最近 7 天", "Last 7 days"),
                    _ => t("全部時間", "All time"),
                };
                opt(text, current == ms, move |this, _, cx| match tab {
                    Tab::Transfers => this.rec_set_tf(|f| f.range = ms, cx),
                    Tab::Events => this.rec_set_ef(|f| f.range = ms, cx),
                })
            })
            .collect();
        self.rec_select(
            p,
            if tab == Tab::Transfers {
                "sel-t-range"
            } else {
                "sel-e-range"
            },
            opts,
            cx,
        )
    }

    // -----------------------------------------------------------------------------------------
    // Transfers page

    pub fn render_transfers(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.rec_sync_placeholders(window, cx);
        let zh = self.rec_zh();
        let t = move |a: &'static str, b: &'static str| if zh { a } else { b };
        let p = pal(cx);
        let vp = window.viewport_size();
        let narrow = vp.width < px(1320.);
        let (vw, vh) = (f32::from(vp.width), f32::from(vp.height));
        let summary = self.selected_service().map(|s| s.summary.clone());

        // `.card.stat`
        let stat_body = |title: &'static str, value: String, unit: String, note: String, color: Hsla| -> Div {
            v_flex()
                .w_full()
                .items_start()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.muted)
                        .child(title),
                )
                .child(
                    h_flex()
                        .items_end()
                        .child(
                            div()
                                .text_size(px(29.))
                                .font_weight(W650)
                                .text_color(color)
                                .child(value),
                        )
                        .when(!unit.is_empty(), |d| {
                            d.child(div().ml(px(6.)).text_size(px(12.)).text_color(p.muted).child(unit))
                        }),
                )
                .child(div().text_size(px(11.)).text_color(p.muted).child(note))
        };
        let stat = |id: &'static str,
                    title: &'static str,
                    value: String,
                    unit: String,
                    note: String,
                    color: Hsla|
         -> Stateful<Div> {
            div()
                .id(id)
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .px(px(18.))
                .py(px(16.))
                .rounded(px(10.))
                .border_1()
                .border_color(p.border)
                .bg(p.panel)
                .child(stat_body(title, value, unit, note, color))
        };
        let bytes_parts = |b: u64| -> (String, String) {
            let s = format_bytes(b);
            match s.split_once(' ') {
                Some((a, u)) => (a.to_string(), u.to_string()),
                None => (s, String::new()),
            }
        };
        let stats = summary.map(|s| {
            let kv_total = s.kv_hit + s.kv_miss;
            let (up, up_u) = bytes_parts(s.up_bytes);
            let (down, down_u) = bytes_parts(s.down_bytes);
            h_flex()
                .items_stretch()
                .gap(px(if vw < 1180. { 10. } else { 16. }))
                .child(stat(
                    "stat-up",
                    t("上傳資料", "Data uploaded"),
                    up,
                    up_u,
                    t("Client → Server（累計傳輸）", "Client → Server (total transferred)").to_string(),
                    p.purple,
                ))
                .child(stat(
                    "stat-down",
                    t("下載資料", "Data downloaded"),
                    down,
                    down_u,
                    t("Server → Client（累計傳輸）", "Server → Client (total transferred)").to_string(),
                    p.blue,
                ))
                .child(stat(
                    "stat-kv",
                    t("KV 查詢命中率", "KV lookup hit rate"),
                    if kv_total > 0 {
                        format!("{:.1}", s.kv_hit as f64 / kv_total as f64 * 100.0)
                    } else {
                        "—".to_string()
                    },
                    if kv_total > 0 { "%".to_string() } else { String::new() },
                    format!("{} hit / {} miss", format_int(s.kv_hit), format_int(s.kv_miss)),
                    p.accent,
                ))
                .child(
                    Button::new("stat-err")
                        .custom(
                            ButtonCustomVariant::new(cx)
                                .color(p.panel)
                                .foreground(p.text)
                                .hover(p.subtle)
                                .active(p.subtle),
                        )
                        .accessibility_label(t("篩選失敗請求", "Filter failed requests"))
                        .flex_1()
                        .min_w(px(0.))
                        .h_auto()
                        .justify_start()
                        .items_start()
                        .px(px(18.))
                        .py(px(16.))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(p.border)
                        .on_click(
                            cx.listener(|this, _, _, cx| this.rec_set_tf(|f| f.outcome = "error".to_string(), cx)),
                        )
                        .child(stat_body(
                            t("錯誤", "Errors"),
                            format_int(s.errors),
                            t("筆", "").to_string(),
                            t("點選篩選失敗請求", "Click to filter failed requests").to_string(),
                            p.danger,
                        )),
                )
        });

        let tf = self.records.tf.clone();
        let filtered = tf.active();
        let (freeze_btn, frozen) = self.rec_freeze_row(Tab::Transfers, &p, &t, cx);
        let meta = &self.records.transfers;
        let data = self.records.transfers_data.as_ref();
        let sel = self.records.selected_transfer;

        let dir_sel = self.rec_select(
            &p,
            "sel-t-dir",
            [
                ("", t("全部方向", "All directions")),
                ("up", t("↑ 上傳", "↑ Upload")),
                ("down", t("↓ 下載", "↓ Download")),
            ]
            .into_iter()
            .map(|(v, l)| {
                opt(l, tf.direction == v, move |this, _, cx| {
                    this.rec_set_tf(|f| f.direction = v.to_string(), cx)
                })
            })
            .collect(),
            cx,
        );
        let svc_sel = self.rec_select(
            &p,
            "sel-t-svc",
            [("", "CAS / KV"), ("CAS", "CAS"), ("KV", "KV")]
                .into_iter()
                .map(|(v, l)| {
                    opt(l, tf.service == v, move |this, _, cx| {
                        this.rec_set_tf(|f| f.service = v.to_string(), cx)
                    })
                })
                .collect(),
            cx,
        );
        let out_sel = self.rec_select(
            &p,
            "sel-t-out",
            [
                ("", t("全部結果", "All results")),
                ("success", t("成功", "Success")),
                ("hit", t("命中", "Hit")),
                ("miss", t("未命中", "Miss")),
                ("error", t("失敗", "Failed")),
            ]
            .into_iter()
            .map(|(v, l)| {
                opt(l, tf.outcome == v, move |this, _, cx| {
                    this.rec_set_tf(|f| f.outcome = v.to_string(), cx)
                })
            })
            .collect(),
            cx,
        );
        let range_sel = self.rec_range_select(&p, Tab::Transfers, tf.range, &t, cx);

        // Columns share spare width in proportion to their language-specific content widths.
        // Object ID is the only column that may shrink below its preferred width.
        let widths: [f32; 6] = if zh {
            [169., 114., 126., 64., 72., 67.]
        } else {
            [169., 114., 126., 89., 72., 71.]
        };
        let cell = |i: usize| {
            let w = widths[i];
            let c = div().px(px(12.)).flex_basis(px(w)).flex_grow(w);
            if i == 2 {
                c.flex_shrink_1().min_w(px(0.))
            } else {
                c.flex_shrink_0()
            }
        };

        let count = data.map_or(0, |d| d.items.len());
        let rows: Vec<AnyElement> = data
            .map(|d| {
                d.items
                    .iter()
                    .enumerate()
                    .map(|(i, tr)| {
                        let id = tr.id;
                        let selected = sel == Some(id);
                        let subtle = p.subtle;
                        let up = tr.direction == "up";
                        h_flex()
                            .id(("tr", id as u64))
                            .flex_none()
                            .py(px(8.))
                            .text_size(px(13.))
                            .when(i + 1 < count, |d| d.border_b_1().border_color(p.row_line))
                            .cursor_pointer()
                            .when(selected, |d| d.bg(p.accent_bg))
                            .when(!selected, move |d| d.hover(move |s| s.bg(subtle)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.rec_focus_list(window, cx);
                                this.select_transfer(id, cx);
                            }))
                            .child(
                                cell(0).child(
                                    v_flex().child(mono(&p, 12.5, clock(tr.started_at))).child(
                                        mono(&p, 12., format!("{} · {}", date(tr.started_at), pad_id(id)))
                                            .text_color(p.muted),
                                    ),
                                ),
                            )
                            .child(
                                cell(1).child(
                                    v_flex()
                                        .child(
                                            h_flex()
                                                .gap(px(4.))
                                                .child(
                                                    div()
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(if up { p.purple } else { p.blue })
                                                        .child(if up { "↑" } else { "↓" }),
                                                )
                                                .child(method_label(&tr.method)),
                                        )
                                        .child(mono(&p, 12., tr.peer.clone()).text_color(p.muted).truncate()),
                                ),
                            )
                            .child(
                                cell(2).child(match &tr.object_id {
                                    Some(o) => {
                                        mono(&p, 13., format!("{}…", o.chars().take(12).collect::<String>())).truncate()
                                    }
                                    None => mono(&p, 13., t("未產生 ID", "No ID generated"))
                                        .text_color(p.muted)
                                        .truncate(),
                                }),
                            )
                            .child(cell(3).child(h_flex().child(outcome_tag(
                                &p,
                                &tr.outcome,
                                outcome_label(&t, &tr.outcome),
                            ))))
                            .child(cell(4).text_right().child(if tr.bytes > 0 {
                                format_bytes(tr.bytes)
                            } else {
                                "—".to_string()
                            }))
                            .child(cell(5).text_right().child(format_duration(tr.duration_us)))
                            .into_any_element()
                    })
                    .collect()
            })
            .unwrap_or_default();

        let header = h_flex()
            .flex_none()
            .py(px(9.))
            .bg(p.subtle)
            .border_b_1()
            .border_color(p.border)
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.muted)
            .whitespace_nowrap()
            .child(cell(0).child(t("時間 / REQUEST", "Time / REQUEST")))
            .child(cell(1).child(t("操作", "Operation")))
            .child(cell(2).child(t("物件 ID / KEY", "Object ID / KEY")))
            .child(cell(3).child(t("結果", "Result")))
            .child(cell(4).text_right().child(t("內容大小", "Size")))
            .child(cell(5).text_right().child(t("耗時", "Duration")));

        let total = data.map(|d| d.total).unwrap_or(0);
        let empty_msg = if data.is_none() {
            if meta.error.is_some() {
                None
            } else {
                Some(t("載入傳輸明細…", "Loading transfers…").to_string())
            }
        } else if data.is_some_and(|d| d.items.is_empty()) {
            Some(if filtered {
                t("沒有符合篩選條件的傳輸。", "No transfers match the filters.").to_string()
            } else {
                t(
                    "尚無傳輸記錄。啟動服務後，client 的每一次 CAS / KV 請求都會出現在這裡。",
                    "No transfers yet. Once the service starts, every CAS / KV request from a client appears here.",
                )
                .to_string()
            })
        } else {
            None
        };
        let not_in_list = sel.is_some() && data.is_some_and(|d| !d.items.iter().any(|x| Some(x.id) == sel));

        let table = v_flex()
            .min_h(px(160.))
            .flex_shrink(1.)
            .rounded(px(8.))
            .border_1()
            .border_color(p.border)
            .overflow_hidden()
            .child(header)
            .child(
                v_flex()
                    .id("transfers-rows")
                    .track_focus(&self.records.focus)
                    .track_scroll(&self.records.t_scroll)
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                        this.rec_key(Tab::Transfers, ev.keystroke.key.as_str(), cx);
                    }))
                    .flex_shrink(1.)
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .when(meta.stale, |d| d.opacity(0.6))
                    .children(rows)
                    .when_some(empty_msg, |d, m| d.child(empty(&p, m, false))),
            );

        let list_max = (vh - 310.).max(480.);
        let list = panel(&p)
            .max_h(px(list_max))
            .when(!narrow, |d| d.flex_1())
            .child(fx(
                h_flex()
                    .justify_between()
                    .gap(px(12.))
                    .child(h_flex().gap(px(8.)).child(heading(t("所有傳輸", "All transfers"))).child(muted(&p, data.map(|d| format_int(d.total)).unwrap_or_else(|| "…".to_string()))))
                    .child(
                        h_flex().gap(px(8.)).child(pill(&p, !frozen, if frozen { t("● 已凍結快照（擷取持續）", "● Frozen snapshot (capture continues)") } else { t("● 持續記錄中", "● Recording") })).child(btn(
                            &p,
                            "clear-transfers",
                            t("清除傳輸明細", "Clear transfers"),
                            true,
                            Bk::Danger,
                            !self.busy,
                            cx.listener(|this, _, window, cx| this.rec_open_clear(Tab::Transfers, window, cx)),
                        )),
                    ),
            ))
            .child(fx(text_input(&p, &self.records.t_search, p.field)))
            .child(fx(
                h_flex()
                    .gap(px(8.))
                    .flex_wrap()
                    .child(dir_sel)
                    .child(svc_sel)
                    .child(out_sel)
                    .child(range_sel)
                    .when(filtered, |d| {
                        d.child(btn(&p, "clear-filters", t("清除篩選", "Clear filters"), true, Bk::Normal, true, cx.listener(|this, _, window, cx| this.rec_clear_filters(Tab::Transfers, window, cx))))
                    })
                    .child(div().flex_1())
                    .child(freeze_btn),
            ))
            .when(frozen, |d| {
                d.child(fx(banner(&p, true).child(t("正在瀏覽凍結的快照：新記錄不會插入，也不會讓分頁位移。回到最新即可看到新記錄。", "Browsing a frozen snapshot: new records won’t be inserted or shift these pages. Go back to latest to see them."))))
            })
            .when(sel.is_none(), |d| d.child(fx(small_muted(&p, t("選擇任一筆傳輸，查看請求 / 回應、payload 與關聯事件。", "Select a transfer to see its request / response, payload, and related events.")))))
            .when_some(self.rec_link_banner(&p, cx), |d, b| d.child(fx(b)))
            .when_some(meta.error.clone(), |d, e| {
                d.child(fx(banner(&p, false)
                    .child(if zh { format!("無法載入傳輸明細：{e}") } else { format!("Couldn’t load transfers: {e}") })
                    .child(link(&p, "transfers-retry", t("重試", "Retry"), cx.listener(|this, _, _, cx| this.rec_reload_fresh(Tab::Transfers, cx))))))
            })
            .child(table)
            .child(fx(
                h_flex().justify_between().gap(px(12.)).child(small_muted(&p, t("未命中為正常查詢結果；不列入錯誤。", "A miss is a normal lookup result and isn’t counted as an error."))).child(btn(
                    &p,
                    "export-transfers",
                    if meta.exporting { t("匯出中…", "Exporting…") } else { t("匯出篩選結果", "Export filtered results") },
                    true,
                    Bk::Normal,
                    !(meta.exporting || total == 0),
                    cx.listener(|this, _, window, cx| this.export_filtered(Tab::Transfers, window, cx)),
                )),
            ))
            .when_some(meta.export_error.clone(), |d, e| d.child(fx(banner(&p, false).child(e))))
            .when(data.is_some(), |d| d.child(fx(self.rec_pager(Tab::Transfers, total, &p, cx))));

        let inspector = self
            .records
            .detail
            .as_ref()
            .map(|_| self.render_transfer_detail(&p, &t, narrow, vh, not_in_list, cx));

        let split = if narrow {
            v_flex().gap(px(18.))
        } else {
            h_flex().gap(px(18.)).items_start()
        };
        v_flex()
            .w_full()
            .gap(px(18.))
            .when_some(stats, |d, s| d.child(s))
            .child(split.child(list).when_some(inspector, |d, i| d.child(i)))
            .into_any_element()
    }

    fn render_transfer_detail(
        &self,
        p: &Pal,
        t: &dyn Fn(&'static str, &'static str) -> &'static str,
        narrow: bool,
        vh: f32,
        not_in_list: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let d = self.records.detail.as_ref().expect("detail");
        let mut inner = v_flex().gap(px(14.)).px(px(20.)).py(px(18.)).min_w(px(0.)).child(
            h_flex()
                .justify_between()
                .gap(px(12.))
                .child(heading(t("傳輸詳細資料", "Transfer details")))
                .child(icon_btn(
                    p,
                    "detail-close",
                    cx.listener(|this, _, window, cx| {
                        this.rec_focus_list(window, cx);
                        this.close_transfer(cx);
                    }),
                )),
        );
        if let Some(e) = &d.error {
            inner = inner.child(banner(p, false).child(e.clone()).child(link(
                p,
                "detail-retry",
                t("重試", "Retry"),
                cx.listener(|this, _, _, cx| {
                    if let Some(id) = this.records.selected_transfer {
                        this.load_detail(id, cx);
                        cx.notify();
                    }
                }),
            )));
        }
        if not_in_list {
            inner = inner.child(banner(p, true).child(t("此傳輸不在目前清單頁中（可能被篩選條件排除、位於其他頁，或比凍結快照更新）。", "This transfer isn’t on the current list page (it may be filtered out, on another page, or newer than the frozen snapshot).")));
        }
        let Some(tr) = d.transfer.as_ref() else {
            if d.error.is_none() {
                inner = inner.child(empty(p, t("載入中…", "Loading…"), true));
            }
            return inspector_shell(p, "transfer-inspector", narrow, vh, inner);
        };
        let up = tr.direction == "up";
        inner = inner.child(
            h_flex()
                .gap(px(8.))
                .flex_wrap()
                .child(mono(p, 12.5, pad_id(tr.id)).font_weight(FontWeight::BOLD))
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(if up { p.purple } else { p.blue })
                        .child(if up {
                            t("↑ 上傳", "↑ Upload")
                        } else {
                            t("↓ 下載", "↓ Download")
                        }),
                )
                .child(div().child(method_label(&tr.method)))
                .child(outcome_tag(p, &tr.outcome, outcome_label(t, &tr.outcome))),
        );
        let tabs = [
            (DetailTab::Overview, t("概覽", "Overview").to_string()),
            (DetailTab::Json, t("請求 / 回應", "Request / Response").to_string()),
            (DetailTab::Payload, format!("Payload {}", tr.payloads.len())),
            (
                DetailTab::Events,
                if self.rec_zh() {
                    format!("關聯事件 {}", d.events.len())
                } else {
                    format!("Related events {}", d.events.len())
                },
            ),
        ];
        inner = inner.child(
            tabs_bar(p).children(tabs.into_iter().enumerate().map(|(i, (tab, text))| {
                tab_btn(
                    p,
                    ("detail-tab", i),
                    text,
                    d.tab == tab,
                    cx.listener(move |this, _, _, cx| {
                        if let Some(d) = this.records.detail.as_mut() {
                            d.tab = tab;
                        }
                        cx.notify();
                    }),
                )
            })),
        );

        let body: AnyElement = match d.tab {
            DetailTab::Overview => {
                let mut dl = v_flex()
                    .gap(px(9.))
                    .child(kv(
                        p,
                        t("開始時間", "Started"),
                        mono(p, 12.5, format_time(tr.started_at)),
                    ))
                    .child(kv(
                        p,
                        t("耗時", "Duration"),
                        mono(p, 12.5, format_duration(tr.duration_us)),
                    ))
                    .child(kv(p, t("來源", "Source"), mono(p, 12.5, tr.peer.clone())))
                    .child(kv(
                        p,
                        t("gRPC 狀態", "gRPC status"),
                        mono(p, 12.5, tr.grpc_status.clone()),
                    ))
                    .child(kv(p, t("內容大小", "Size"), mono(p, 12.5, exact_bytes(tr.bytes))));
                if let Some(e) = &tr.error {
                    dl = dl.child(kv(p, t("錯誤", "Error"), div().text_color(p.danger).child(e.clone())));
                }
                let mut v = v_flex().gap(px(12.)).min_w(px(0.)).child(dl);
                v = match &tr.object_id {
                    Some(o) => {
                        let copy = o.clone();
                        v.child(
                            v_flex()
                                .gap(px(6.))
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .gap(px(12.))
                                        .child(label(p, t("完整 ID", "Full ID")))
                                        .child(btn(
                                            p,
                                            "copy-id",
                                            t("複製 ID", "Copy ID"),
                                            true,
                                            Bk::Normal,
                                            true,
                                            cx.listener(move |this, _, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                                this.notice(
                                                    if this.rec_zh() {
                                                        "已複製 ID".to_string()
                                                    } else {
                                                        "ID copied".to_string()
                                                    },
                                                    cx,
                                                );
                                            }),
                                        )),
                                )
                                .child(
                                    div()
                                        .px(px(12.))
                                        .py(px(10.))
                                        .border_1()
                                        .border_color(p.border)
                                        .rounded(px(7.))
                                        .bg(p.field)
                                        .font_family(p.mono.clone())
                                        .text_size(px(12.5))
                                        .child(o.clone()),
                                ),
                        )
                    }
                    None => v.child(small_muted(
                        p,
                        t("此請求未產生物件 ID。", "This request produced no object ID."),
                    )),
                };
                for (si, (side, value)) in [
                    (t("請求", "Request"), &tr.request),
                    (t("回應", "Response"), &tr.response),
                ]
                .into_iter()
                .enumerate()
                {
                    for key in ["references", "refs", "entries"] {
                        let Some(items) = value.get(key).and_then(|v| v.as_array()) else {
                            continue;
                        };
                        let ref_key = format!("refs-{si}-{key}");
                        let shown = items.len().min(self.rec_limit(&ref_key, MAX_REFS));
                        let more = self.rec_more(p, ref_key.clone(), shown, items.len(), "rows", cx);
                        v = v.child(
                            v_flex()
                                .gap(px(6.))
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .gap(px(12.))
                                        .child(label(p, format!("{side} {key}")))
                                        .child(small_muted(
                                            p,
                                            if self.rec_zh() {
                                                format!("{} 項", items.len())
                                            } else {
                                                format!("{} items", items.len())
                                            },
                                        )),
                                )
                                .child(
                                    v_flex()
                                        .gap(px(4.))
                                        .font_family(p.mono.clone())
                                        .text_size(px(12.5))
                                        .children(items.iter().take(shown).enumerate().map(|(i, it)| {
                                            let text = match it {
                                                Value::String(s) => s.clone(),
                                                other => other.to_string(),
                                            };
                                            h_flex()
                                                .gap(px(6.))
                                                .items_start()
                                                .child(
                                                    div()
                                                        .flex_none()
                                                        .text_color(p.muted)
                                                        .child(format!("{:02}", i + 1)),
                                                )
                                                .child(div().flex_1().min_w(px(0.)).child(text))
                                        })),
                                )
                                .when_some(more, |x, m| x.child(m)),
                        );
                    }
                }
                v.into_any_element()
            }
            DetailTab::Json => {
                let copy = |text: &'static str, id: &'static str, lines: Vec<SharedString>, cx: &mut Context<Self>| {
                    btn(
                        p,
                        id,
                        text,
                        true,
                        Bk::Normal,
                        true,
                        cx.listener(move |this, _, _, cx| {
                            let text = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                            this.notice(
                                if this.rec_zh() {
                                    "已複製 JSON".to_string()
                                } else {
                                    "JSON copied".to_string()
                                },
                                cx,
                            );
                        }),
                    )
                };
                v_flex()
                    .gap(px(12.))
                    .min_w(px(0.))
                    .child(
                        h_flex()
                            .justify_between()
                            .gap(px(12.))
                            .child(label(p, "Request"))
                            .child(copy(t("複製 JSON", "Copy JSON"), "copy-req", d.req_lines.clone(), cx)),
                    )
                    .child(self.rec_lines(p, "json-req", &d.req_lines, false, cx))
                    .child(
                        h_flex()
                            .justify_between()
                            .gap(px(12.))
                            .child(label(p, "Response"))
                            .child(copy(t("複製 JSON", "Copy JSON"), "copy-resp", d.resp_lines.clone(), cx)),
                    )
                    .child(self.rec_lines(p, "json-resp", &d.resp_lines, false, cx))
                    .into_any_element()
            }
            DetailTab::Payload => self.render_payload(p, t, tr, d, cx),
            DetailTab::Events => {
                if d.events.is_empty() {
                    empty(
                        p,
                        t("沒有與此傳輸關聯的事件。", "No events are linked to this transfer."),
                        true,
                    )
                    .into_any_element()
                } else {
                    v_flex()
                        .gap(px(6.))
                        .children(d.events.iter().map(|ev| {
                            let eid = ev.id;
                            Button::new(("linked", eid as u64))
                                .custom(p.v_plain)
                                .accessibility_label(ev.message.clone())
                                .w_full()
                                .h_auto()
                                .justify_start()
                                .px(px(10.))
                                .py(px(8.))
                                .border_1()
                                .border_color(p.border)
                                .rounded(px(7.))
                                .on_click(cx.listener(move |this, _, _, cx| this.open_event(eid, cx)))
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap(px(10.))
                                        .text_size(px(12.5))
                                        .child(level_tag(p, &ev.level))
                                        .child(
                                            div()
                                                .flex_none()
                                                .font_family(p.mono.clone())
                                                .text_color(p.muted)
                                                .child(clock(ev.time)),
                                        )
                                        .child(div().flex_1().min_w(px(0.)).truncate().child(ev.message.clone())),
                                )
                        }))
                        .into_any_element()
                }
            }
        };
        inner = inner.child(body).child(h_flex().justify_end().child(btn(
            p,
            "export-transfer",
            if d.exporting {
                t("匯出中…", "Exporting…")
            } else {
                t("匯出此筆詳細資料", "Export this transfer")
            },
            false,
            Bk::Normal,
            !d.exporting,
            cx.listener(|this, _, window, cx| this.export_transfer(window, cx)),
        )));
        inspector_shell(p, "transfer-inspector", narrow, vh, inner)
    }

    fn render_payload(
        &self,
        p: &Pal,
        t: &dyn Fn(&'static str, &'static str) -> &'static str,
        tr: &Transfer,
        d: &Detail,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if tr.payloads.is_empty() {
            return empty(
                p,
                t(
                    "這筆傳輸沒有 payload（未命中、錯誤或僅有 metadata）。",
                    "This transfer has no payload (miss, error, or metadata only).",
                ),
                true,
            )
            .into_any_element();
        }
        let pv = &d.payload;
        let cur = &tr.payloads[pv.index.min(tr.payloads.len() - 1)];
        let total = pv.chunk.as_ref().map(|c| c.total).unwrap_or(cur.bytes);
        let shown = pv.chunk.as_ref().map(|c| (c.hex.len() / 2) as u64).unwrap_or(0);
        let start = pv.chunk.as_ref().map(|c| c.offset).unwrap_or(pv.offset);
        let zh = self.rec_zh();
        let mut v = v_flex().gap(px(12.)).min_w(px(0.));
        v = v.child(
            v_flex()
                .id("payload-list")
                .gap(px(4.))
                .max_h(px(150.))
                .overflow_y_scroll()
                .children(tr.payloads.iter().enumerate().map(|(i, pl)| {
                    let active = i == pv.index;
                    Button::new(("payload-item", i))
                        .custom(if active {
                            ButtonCustomVariant::new(cx)
                                .color(p.accent_bg)
                                .foreground(p.text)
                                .hover(p.accent_bg)
                                .active(p.accent_bg)
                        } else {
                            p.v_plain
                        })
                        .selected(active)
                        .accessibility_label(pl.label.clone())
                        .flex_none()
                        .w_full()
                        .h_auto()
                        .px(px(10.))
                        .py(px(7.))
                        .border_1()
                        .border_color(if active { p.accent } else { p.border })
                        .rounded(px(7.))
                        .when(active, |b| b.bg(p.accent_bg))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(d) = this.records.detail.as_mut() {
                                d.payload.index = i;
                                d.payload.offset = 0;
                            }
                            this.load_payload_chunk(cx);
                        }))
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .gap(px(10.))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .font_family(p.mono.clone())
                                        .text_size(px(12.5))
                                        .truncate()
                                        .child(pl.label.clone()),
                                )
                                .child(div().flex_none().text_color(p.muted).child(format_bytes(pl.bytes))),
                        )
                })),
        );
        v = v.child(
            h_flex()
                .justify_between()
                .gap(px(12.))
                .child(
                    seg(p)
                        .child(seg_btn(
                            p,
                            "pm-hex",
                            if zh { "十六進位" } else { "Hex" },
                            pv.mode == PayloadMode::Hex,
                            cx.listener(|this, _, _, cx| {
                                if let Some(d) = this.records.detail.as_mut() {
                                    d.payload.mode = PayloadMode::Hex;
                                }
                                cx.notify();
                            }),
                        ))
                        .child(seg_btn(
                            p,
                            "pm-text",
                            t("文字", "Text"),
                            pv.mode == PayloadMode::Text,
                            cx.listener(|this, _, _, cx| {
                                if let Some(d) = this.records.detail.as_mut() {
                                    d.payload.mode = PayloadMode::Text;
                                }
                                cx.notify();
                            }),
                        )),
                )
                .child(btn(
                    p,
                    "export-payload",
                    if pv.exporting {
                        t("匯出中…", "Exporting…")
                    } else {
                        t("匯出此 payload", "Export this payload")
                    },
                    true,
                    Bk::Normal,
                    !pv.exporting,
                    cx.listener(|this, _, window, cx| this.export_payload(window, cx)),
                )),
        );
        v = v.child(
            mono(
                p,
                12.,
                if zh {
                    format!("摘要 {}", cur.id)
                } else {
                    format!("digest {}", cur.id)
                },
            )
            .text_color(p.muted)
            .truncate(),
        );
        if let Some(e) = &pv.error {
            v = v.child(banner(p, false).child(e.clone()).child(link(
                p,
                "payload-retry",
                t("重試", "Retry"),
                cx.listener(|this, _, _, cx| this.load_payload_chunk(cx)),
            )));
        } else if pv.chunk.is_none() {
            v = v.child(empty(p, t("載入 payload…", "Loading payload…"), true));
        }
        if pv.chunk.is_some() {
            v = match pv.mode {
                PayloadMode::Hex => v.child(self.rec_lines(
                    p,
                    &format!("payload-hex-{}-{}", pv.index, pv.offset),
                    &pv.hex_lines,
                    true,
                    cx,
                )),
                PayloadMode::Text => v.child(self.rec_lines(
                    p,
                    &format!("payload-text-{}-{}", pv.index, pv.offset),
                    &pv.text_lines,
                    true,
                    cx,
                )),
            };
        }
        let range = if zh {
            format!(
                "{}–{} B / 完整 {} B",
                format_int(start),
                format_int(start + shown),
                format_int(total)
            )
        } else {
            format!(
                "{}–{} B of {} B",
                format_int(start),
                format_int(start + shown),
                format_int(total)
            )
        };
        let prev_disabled = pv.offset == 0;
        let next_disabled = pv.offset + CHUNK >= total;
        v.child(
            h_flex()
                .justify_between()
                .gap(px(12.))
                .child(small_muted(p, range))
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(btn(
                            p,
                            "payload-prev",
                            t("上一段", "Previous chunk"),
                            true,
                            Bk::Normal,
                            !prev_disabled,
                            cx.listener(|this, _, _, cx| {
                                if let Some(d) = this.records.detail.as_mut() {
                                    d.payload.offset = d.payload.offset.saturating_sub(CHUNK);
                                }
                                this.load_payload_chunk(cx);
                            }),
                        ))
                        .child(btn(
                            p,
                            "payload-next",
                            t("下一段", "Next chunk"),
                            true,
                            Bk::Normal,
                            !next_disabled,
                            cx.listener(|this, _, _, cx| {
                                if let Some(d) = this.records.detail.as_mut() {
                                    d.payload.offset += CHUNK;
                                }
                                this.load_payload_chunk(cx);
                            }),
                        )),
                ),
        )
        .into_any_element()
    }

    // -----------------------------------------------------------------------------------------
    // Events page

    pub fn render_events(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.rec_sync_placeholders(window, cx);
        let zh = self.rec_zh();
        let t = move |a: &'static str, b: &'static str| if zh { a } else { b };
        let p = pal(cx);
        let vp = window.viewport_size();
        let narrow = vp.width < px(1320.);
        let vh = f32::from(vp.height);
        let ef = self.records.ef.clone();
        let filtered = ef.active();
        let (freeze_btn, frozen) = self.rec_freeze_row(Tab::Events, &p, &t, cx);
        let meta = &self.records.events;
        let data = self.records.events_data.as_ref();
        let sel_id = self.records.selected_event.as_ref().map(|s| s.event.id);

        let level_sel = self.rec_select(
            &p,
            "sel-e-level",
            std::iter::once(("", t("全部層級", "All levels")))
                .chain(LEVELS.iter().map(|l| (*l, *l)))
                .map(|(v, l)| {
                    opt(l, ef.level == v, move |this, _, cx| {
                        this.rec_set_ef(|f| f.level = v.to_string(), cx)
                    })
                })
                .collect(),
            cx,
        );
        let kind_opts: Vec<Opt> = std::iter::once(("", t("類型（全部）", "Kind (all)")))
            .chain(KINDS.iter().map(|k| (*k, *k)))
            .map(|(v, l)| {
                opt(l, ef.kind == v, move |this, window, cx| {
                    this.records.debounce[Field::EventKind as usize] = None;
                    this.records.e_kind.update(cx, |s, cx| s.set_value(v, window, cx));
                    this.rec_set_ef(|f| f.kind = v.to_string(), cx);
                })
            })
            .collect();
        let kind_menu = self.rec_menu(&p, "sel-e-kind", kind_opts, 150., cx);
        let kind_field = div()
            .relative()
            .flex_none()
            .w(px(150.))
            .child(
                text_input(&p, &self.records.e_kind, p.panel).suffix(
                    Button::new("kind-caret")
                        .custom(p.v_icon)
                        .accessibility_label(t("事件類型", "Event kinds"))
                        .px(px(4.))
                        .on_click(cx.listener(|this, _, _, cx| this.rec_toggle_select("sel-e-kind", cx)))
                        .child(Icon::new(IconName::ChevronDown).size(px(14.)).text_color(p.text)),
                ),
            )
            .children(kind_menu);
        let range_sel = self.rec_range_select(&p, Tab::Events, ef.range, &t, cx);

        // Language-specific preferred widths for fixed columns. Message takes the rest;
        // the other columns never wrap.
        let widths: [f32; 6] = if zh {
            [124., 75., 74., 0., 86., 102.]
        } else {
            [120., 73., 72., 0., 118., 99.]
        };
        let cell = |i: usize| match i {
            3 => div().px(px(12.)).flex_1().min_w(px(220.)),
            _ => div().px(px(12.)).w(px(widths[i])).flex_none().whitespace_nowrap(),
        };

        let count = data.map_or(0, |d| d.items.len());
        let rows: Vec<AnyElement> = data
            .map(|d| {
                d.items
                    .iter()
                    .enumerate()
                    .map(|(i, ev)| {
                        let id = ev.id;
                        let event = ev.clone();
                        let selected = sel_id == Some(id);
                        let subtle = p.subtle;
                        h_flex()
                            .id(("ev", id as u64))
                            .flex_none()
                            .py(px(8.))
                            .text_size(px(13.))
                            .when(i + 1 < count, |d| d.border_b_1().border_color(p.row_line))
                            .cursor_pointer()
                            .when(selected, |d| d.bg(p.accent_bg))
                            .when(!selected, move |d| d.hover(move |s| s.bg(subtle)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.rec_focus_list(window, cx);
                                this.select_event(event.clone(), cx);
                            }))
                            .child(
                                cell(0).child(
                                    v_flex()
                                        .child(mono(&p, 12.5, clock(ev.time)))
                                        .child(mono(&p, 12., date(ev.time)).text_color(p.muted)),
                                ),
                            )
                            .child(cell(1).child(h_flex().child(level_tag(&p, &ev.level))))
                            .child(cell(2).child(ev.kind.clone()))
                            .child(cell(3).child(ev.message.clone()))
                            .child(cell(4).child(match ev.transfer_id {
                                Some(tid) => mono(&p, 13., pad_id(tid)),
                                None => mono(&p, 13., "—").text_color(p.muted),
                            }))
                            .child(cell(5).child(match &ev.peer {
                                Some(peer) => mono(&p, 13., peer.clone()).truncate(),
                                None => mono(&p, 13., "server").text_color(p.muted),
                            }))
                            .into_any_element()
                    })
                    .collect()
            })
            .unwrap_or_default();

        let header = h_flex()
            .flex_none()
            .py(px(9.))
            .bg(p.subtle)
            .border_b_1()
            .border_color(p.border)
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.muted)
            .whitespace_nowrap()
            .child(cell(0).child(t("時間", "Time")))
            .child(cell(1).child(t("層級", "Level")))
            .child(cell(2).child(t("類型", "Kind")))
            .child(cell(3).child(t("訊息", "Message")))
            .child(cell(4).child(t("關聯傳輸", "Related transfer")))
            .child(cell(5).child(t("來源", "Source")));

        let total = data.map(|d| d.total).unwrap_or(0);
        let empty_msg = if data.is_none() {
            if meta.error.is_some() {
                None
            } else {
                Some(t("載入事件…", "Loading events…").to_string())
            }
        } else if data.is_some_and(|d| d.items.is_empty()) {
            Some(if filtered {
                t("沒有符合篩選條件的事件。", "No events match the filters.").to_string()
            } else {
                t("尚無事件。", "No events yet.").to_string()
            })
        } else {
            None
        };

        let table = v_flex()
            .min_h(px(160.))
            .flex_shrink(1.)
            .rounded(px(8.))
            .border_1()
            .border_color(p.border)
            .overflow_hidden()
            .child(header)
            .child(
                v_flex()
                    .id("events-rows")
                    .track_focus(&self.records.focus)
                    .track_scroll(&self.records.e_scroll)
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                        this.rec_key(Tab::Events, ev.keystroke.key.as_str(), cx);
                    }))
                    .flex_shrink(1.)
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .when(meta.stale, |d| d.opacity(0.6))
                    .children(rows)
                    .when_some(empty_msg, |d, m| d.child(empty(&p, m, false))),
            );

        let list_max = (vh - 190.).max(450.);
        let list = panel(&p)
            .max_h(px(list_max))
            .when(!narrow, |d| d.flex_1())
            .child(fx(
                h_flex()
                    .justify_between()
                    .gap(px(12.))
                    .child(h_flex().gap(px(8.)).child(heading(t("事件", "Events"))).child(muted(&p, data.map(|d| format_int(d.total)).unwrap_or_else(|| "…".to_string()))))
                    .child(
                        h_flex().gap(px(8.)).child(pill(&p, !frozen, if frozen { t("● 已凍結快照", "● Frozen snapshot") } else { t("● 持續更新", "● Live") })).child(btn(
                            &p,
                            "clear-events",
                            t("清除事件紀錄", "Clear events"),
                            true,
                            Bk::Danger,
                            !self.busy,
                            cx.listener(|this, _, window, cx| this.rec_open_clear(Tab::Events, window, cx)),
                        )),
                    ),
            ))
            .child(fx(text_input(&p, &self.records.e_search, p.field)))
            .child(fx(
                h_flex()
                    .gap(px(8.))
                    .flex_wrap()
                    .child(level_sel)
                    .child(kind_field)
                    .child(range_sel)
                    .when(filtered, |d| {
                        d.child(btn(&p, "clear-filters", t("清除篩選", "Clear filters"), true, Bk::Normal, true, cx.listener(|this, _, window, cx| this.rec_clear_filters(Tab::Events, window, cx))))
                    })
                    .child(div().flex_1())
                    .child(freeze_btn),
            ))
            .when(frozen, |d| {
                d.child(fx(banner(&p, true).child(t("正在瀏覽凍結的快照：新事件不會插入，也不會讓分頁位移。回到最新即可看到新事件。", "Browsing a frozen snapshot: new events won’t be inserted or shift these pages. Go back to latest to see them."))))
            })
            .when(sel_id.is_none(), |d| d.child(fx(small_muted(&p, t("選擇任一事件，查看完整訊息與結構化欄位。", "Select an event to see its full message and structured fields.")))))
            .when_some(self.rec_link_banner(&p, cx), |d, b| d.child(fx(b)))
            .when_some(meta.error.clone(), |d, e| {
                d.child(fx(banner(&p, false)
                    .child(if zh { format!("無法載入事件：{e}") } else { format!("Couldn’t load events: {e}") })
                    .child(link(&p, "events-retry", t("重試", "Retry"), cx.listener(|this, _, _, cx| this.rec_reload_fresh(Tab::Events, cx))))))
            })
            .child(table)
            .child(fx(
                h_flex().justify_between().gap(px(12.)).child(small_muted(&p, t("點選任一事件，可回看原始訊息及其關聯傳輸。", "Select an event to see its raw message and related transfer."))).child(btn(
                    &p,
                    "export-events",
                    if meta.exporting { t("匯出中…", "Exporting…") } else { t("匯出篩選結果", "Export filtered results") },
                    true,
                    Bk::Normal,
                    !(meta.exporting || total == 0),
                    cx.listener(|this, _, window, cx| this.export_filtered(Tab::Events, window, cx)),
                )),
            ))
            .when_some(meta.export_error.clone(), |d, e| d.child(fx(banner(&p, false).child(e))))
            .when(data.is_some(), |d| d.child(fx(self.rec_pager(Tab::Events, total, &p, cx))));

        let inspector = self.records.selected_event.as_ref().map(|s| {
            let ev = &s.event;
            let not_in_list = data.is_some_and(|d| !d.items.iter().any(|x| x.id == ev.id));
            let lines = s.lines.clone();
            let transfer_link = match ev.transfer_id {
                Some(tid) => link(&p, "goto-transfer", if zh { format!("前往對應傳輸 {} ↗", pad_id(tid)) } else { format!("Go to transfer {} ↗", pad_id(tid)) }, cx.listener(move |this, _, _, cx| this.open_transfer(tid, cx))).into_any_element(),
                None => muted(&p, t("無", "None")).into_any_element(),
            };
            let inner = v_flex()
                .gap(px(14.))
                .px(px(20.))
                .py(px(18.))
                .min_w(px(0.))
                .child(h_flex().justify_between().gap(px(12.)).child(heading(t("事件詳細資料", "Event details"))).child(icon_btn(&p, "event-close", cx.listener(|this, _, window, cx| {
                    this.rec_focus_list(window, cx);
                    this.records.close_event();
                    cx.notify();
                }))))
                .when(not_in_list, |d| {
                    d.child(banner(&p, true).child(t("此事件不在目前清單頁中（可能被篩選條件排除、位於其他頁，或比凍結快照更新）。", "This event isn’t on the current list page (it may be filtered out, on another page, or newer than the frozen snapshot).")))
                })
                .child(h_flex().gap(px(8.)).flex_wrap().child(level_tag(&p, &ev.level)).child(mono(&p, 12.5, format_time(ev.time)).text_color(p.muted)))
                .child(
                    v_flex()
                        .gap(px(9.))
                        .child(kv(&p, t("類型", "Kind"), ev.kind.clone()))
                        .child(kv(&p, t("訊息", "Message"), ev.message.clone()))
                        .child(kv(&p, t("來源", "Source"), mono(&p, 12.5, ev.peer.clone().unwrap_or_else(|| "server".to_string()))))
                        .child(kv(&p, t("關聯傳輸", "Related transfer"), transfer_link)),
                )
                .child(
                    h_flex()
                        .justify_between()
                        .gap(px(12.))
                        .flex_wrap()
                        .child(label(&p, t("原始事件 / 結構化欄位", "Raw event / structured fields")))
                        .child(
                            h_flex()
                                .gap(px(8.))
                                .child(btn(&p, "copy-event", t("複製 JSON", "Copy JSON"), true, Bk::Normal, true, cx.listener({
                                    let lines = lines.clone();
                                    move |this, _, _, cx| {
                                        let text = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
                                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                                        this.notice(if this.rec_zh() { "已複製 JSON".to_string() } else { "JSON copied".to_string() }, cx);
                                    }
                                })))
                                .child(btn(&p, "export-event", t("匯出此事件", "Export this event"), true, Bk::Normal, s.export_task.is_none(), cx.listener(|this, _, window, cx| this.export_event(window, cx)))),
                        ),
                )
                .child(self.rec_lines(&p, "event-json", &lines, false, cx));
            inspector_shell(&p, "event-inspector", narrow, vh, inner)
        });

        let split = if narrow {
            v_flex().gap(px(18.))
        } else {
            h_flex().gap(px(18.)).items_start()
        };
        v_flex()
            .w_full()
            .child(split.child(list).when_some(inspector, |d, i| d.child(i)))
            .into_any_element()
    }
}

/// `.card.panel.inspector`: 420px column that scrolls within the viewport; below 1320px it stacks under the list.
fn inspector_shell(p: &Pal, id: &'static str, narrow: bool, vh: f32, inner: Div) -> AnyElement {
    div()
        .id(id)
        .min_w(px(0.))
        .rounded(px(10.))
        .border_1()
        .border_color(p.border)
        .bg(p.panel)
        .when(!narrow, |d| {
            d.w(px(420.))
                .flex_none()
                .max_h(px((vh - 56.).max(0.)))
                .overflow_y_scroll()
        })
        .child(inner)
        .into_any_element()
}

fn outcome_label(t: &dyn Fn(&'static str, &'static str) -> &'static str, outcome: &str) -> String {
    match outcome {
        "success" => t("成功", "Success").to_string(),
        "hit" => t("命中", "Hit").to_string(),
        "miss" => t("未命中", "Miss").to_string(),
        "error" => t("失敗", "Failed").to_string(),
        other => other.to_string(),
    }
}

/// Percent-encodes a path segment (payload ids are hex digests, but never trust the daemon blindly).
fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

pub fn smoke_snapshot(state: &RecordsState) -> Value {
    json!({"transfer_total":state.transfers_data.as_ref().map(|p|p.total),"transfer_ids":state.transfers_data.as_ref().map(|p|p.items.iter().map(|t|t.id).collect::<Vec<_>>()),"transfer_error":state.transfers.error,"event_total":state.events_data.as_ref().map(|p|p.total),"event_error":state.events.error,"selected_transfer":state.selected_transfer,"detail":state.detail.as_ref().map(|d|json!({"transfer":d.transfer,"error":d.error,"req_lines":d.req_lines.len(),"response_lines":d.resp_lines.len(),"events":d.events.len(),"chunk":d.payload.chunk,"payload_error":d.payload.error})),"selected_event":state.selected_event.as_ref().map(|e|e.event.id),"frozen":state.transfers.snap.as_ref().map(|s|s.max_id)})
}
