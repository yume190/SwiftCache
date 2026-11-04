import { useCallback, useEffect, useRef, useState, type KeyboardEvent } from 'react';
import { api, query, saveExport } from '../api';
import { Banner, CopyButton, JsonBlock, Pager, PayloadViewer } from '../components';
import { keepIfSame, useDebounced, usePoll } from '../hooks';
import {
  bytesParts,
  errorMessage,
  formatBytes,
  formatClock,
  formatDate,
  formatDateTime,
  formatDuration,
  formatInt,
} from '../format';
import type { AppEvent, Outcome, Page, Summary, Transfer } from '../types';
import { HistoryClear } from '../HistoryClear';
import { defineMessages, pick, useMessages } from '../i18n';

const M = defineMessages(
  {
    r15m: '最近 15 分鐘',
    r1h: '最近 1 小時',
    r24h: '最近 24 小時',
    r7d: '最近 7 天',
    rAll: '全部時間',
    success: '成功',
    hit: '命中',
    miss: '未命中',
    failed: '失敗',
    uploadData: '上傳資料',
    downloadData: '下載資料',
    uploadNote: 'Client → Server（累計傳輸）',
    downloadNote: 'Server → Client（累計傳輸）',
    kvHitRate: 'KV 查詢命中率',
    errors: '錯誤',
    errorUnit: '筆',
    errorsNote: '點選篩選失敗請求',
    allTransfers: '所有傳輸',
    pausedPill: '● 已凍結快照（擷取持續）',
    livePill: '● 持續記錄中',
    searchPlaceholder: '搜尋完整 CAS ID、KV key、來源或 method…',
    allDirections: '全部方向',
    dirUp: '↑ 上傳',
    dirDown: '↓ 下載',
    allResults: '全部結果',
    clearFilters: '清除篩選',
    resume: '回到最新並繼續更新',
    pause: '暫停並凍結清單',
    loadFailed: (e: string) => `無法載入傳輸明細：${e}`,
    colTime: '時間 / REQUEST',
    colOp: '操作',
    colObject: '物件 ID / KEY',
    source: '來源',
    colResult: '結果',
    size: '內容大小',
    duration: '耗時',
    noId: '未產生 ID',
    loadingList: '載入傳輸明細…',
    noMatch: '沒有符合篩選條件的傳輸。',
    noTransfers: '尚無傳輸記錄。啟動服務後，client 的每一次 CAS / KV 請求都會出現在這裡。',
    missNote: '未命中為正常查詢結果；不列入錯誤。',
    exporting: '匯出中…',
    exportFiltered: '匯出篩選結果',
    detailTitle: '傳輸詳細資料',
    pickTransfer: '選擇任一筆傳輸，查看請求 / 回應、payload 與關聯事件。',
    closeDetail: '關閉詳細資料',
    loading: '載入中…',
    tabOverview: '概覽',
    tabJson: '請求 / 回應',
    tabEvents: (n: number) => `關聯事件 ${n}`,
    startedAt: '開始時間',
    grpcStatus: 'gRPC 狀態',
    errorLabel: '錯誤',
    fullId: '完整 ID',
    copyId: '複製 ID',
    noObjectId: '此請求未產生物件 ID。',
    request: '請求',
    response: '回應',
    items: (n: number) => `${n} 項`,
    noLinked: '沒有與此傳輸關聯的事件。',
    exportOne: '匯出此筆詳細資料',
    rangeLabel: '時間範圍',
    retry: '重試',
    frozenNote: '正在瀏覽凍結的快照：新記錄不會插入，也不會讓分頁位移。回到最新即可看到新記錄。',
    notInList: '此傳輸不在目前清單頁中（可能被篩選條件排除、位於其他頁，或比凍結快照更新）。',
  },
  {
    r15m: 'Last 15 minutes',
    r1h: 'Last hour',
    r24h: 'Last 24 hours',
    r7d: 'Last 7 days',
    rAll: 'All time',
    success: 'Success',
    hit: 'Hit',
    miss: 'Miss',
    failed: 'Failed',
    uploadData: 'Data uploaded',
    downloadData: 'Data downloaded',
    uploadNote: 'Client → Server (total transferred)',
    downloadNote: 'Server → Client (total transferred)',
    kvHitRate: 'KV lookup hit rate',
    errors: 'Errors',
    errorUnit: '',
    errorsNote: 'Click to filter failed requests',
    allTransfers: 'All transfers',
    pausedPill: '● Frozen snapshot (capture continues)',
    livePill: '● Recording',
    searchPlaceholder: 'Search full CAS ID, KV key, source, or method…',
    allDirections: 'All directions',
    dirUp: '↑ Upload',
    dirDown: '↓ Download',
    allResults: 'All results',
    clearFilters: 'Clear filters',
    resume: 'Back to latest & resume',
    pause: 'Pause & freeze list',
    loadFailed: (e: string) => `Couldn’t load transfers: ${e}`,
    colTime: 'Time / REQUEST',
    colOp: 'Operation',
    colObject: 'Object ID / KEY',
    source: 'Source',
    colResult: 'Result',
    size: 'Size',
    duration: 'Duration',
    noId: 'No ID generated',
    loadingList: 'Loading transfers…',
    noMatch: 'No transfers match the filters.',
    noTransfers: 'No transfers yet. Once the service starts, every CAS / KV request from a client appears here.',
    missNote: 'A miss is a normal lookup result and isn’t counted as an error.',
    exporting: 'Exporting…',
    exportFiltered: 'Export filtered results',
    detailTitle: 'Transfer details',
    pickTransfer: 'Select a transfer to see its request / response, payload, and related events.',
    closeDetail: 'Close details',
    loading: 'Loading…',
    tabOverview: 'Overview',
    tabJson: 'Request / Response',
    tabEvents: (n: number) => `Related events ${n}`,
    startedAt: 'Started',
    grpcStatus: 'gRPC status',
    errorLabel: 'Error',
    fullId: 'Full ID',
    copyId: 'Copy ID',
    noObjectId: 'This request produced no object ID.',
    request: 'Request',
    response: 'Response',
    items: (n: number) => `${n} items`,
    noLinked: 'No events are linked to this transfer.',
    exportOne: 'Export this transfer',
    rangeLabel: 'Time range',
    retry: 'Retry',
    frozenNote: 'Browsing a frozen snapshot: new records won’t be inserted or shift these pages. Go back to latest to see them.',
    notInList: 'This transfer isn’t on the current list page (it may be filtered out, on another page, or newer than the frozen snapshot).',
  },
);

const PAGE = 50;
type RangeKey = 'r15m' | 'r1h' | 'r24h' | 'r7d' | 'rAll';
const RANGES: { label: RangeKey; ms: number }[] = [
  { label: 'r15m', ms: 15 * 60_000 },
  { label: 'r1h', ms: 60 * 60_000 },
  { label: 'r24h', ms: 24 * 60 * 60_000 },
  { label: 'r7d', ms: 7 * 24 * 60 * 60_000 },
  { label: 'rAll', ms: 0 },
];

// Locale-aware at read time; keeps the exported Record<Outcome, string> shape.
export const OUTCOME_LABEL: Record<Outcome, string> = {
  get success() {
    return pick(M).success;
  },
  get hit() {
    return pick(M).hit;
  },
  get miss() {
    return pick(M).miss;
  },
  get error() {
    return pick(M).failed;
  },
};
const methodLabel = (m: string) => m.replace('.', ' ');

interface Filters {
  search: string;
  direction: string;
  service: string;
  outcome: string;
  range: number;
}
const DEFAULT_FILTERS: Filters = { search: '', direction: '', service: '', outcome: '', range: 0 };

function filterParams(f: Filters, now = Date.now()) {
  return {
    search: f.search,
    direction: f.direction,
    service: f.service,
    outcome: f.outcome,
    since: f.range ? now - f.range : undefined,
  };
}

// A frozen view pins the newest visible id and the clock, so later pages and relative ranges stop moving.
// `auto` snapshots come from paging into history and end when the user returns to the first page.
interface Snapshot {
  serviceId: string;
  maxId: number;
  at: number;
  auto: boolean;
}

function rowKeys(e: KeyboardEvent<HTMLTableRowElement>, select: () => void) {
  if (e.target !== e.currentTarget) return;
  if (e.key === 'Enter' || e.key === ' ') {
    e.preventDefault();
    select();
  } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    const next = e.key === 'ArrowDown' ? e.currentTarget.nextElementSibling : e.currentTarget.previousElementSibling;
    if (next instanceof HTMLElement) {
      e.preventDefault();
      next.focus();
    }
  }
}

function StatCard({ title, value, unit, note, color, onClick }: { title: string; value: string; unit: string; note: string; color: string; onClick?: () => void }) {
  const body = (
    <>
      <div className="stat-title">{title}</div>
      <div className="stat-value" style={{ color }}>
        {value}
        <small>{unit}</small>
      </div>
      <div className="stat-note">{note}</div>
    </>
  );
  return onClick ? (
    <button type="button" className="card stat clickable" onClick={onClick}>
      {body}
    </button>
  ) : (
    <div className="card stat">{body}</div>
  );
}

export function TransfersPage({
  active,
  summary,
  serviceId,
  focus,
  serviceName,
  locked,
  onOpenEvent,
  onClearBusy,
  onHistoryCleared,
}: {
  active: boolean;
  summary: Summary;
  serviceId: string;
  serviceName: string;
  focus: { id: number; nonce: number } | null;
  /** A service operation or history clear is in flight, or the daemon is unreachable. */
  locked: boolean;
  onOpenEvent: (e: AppEvent) => void;
  onClearBusy: (busy: boolean) => void;
  onHistoryCleared: () => void;
}) {
  const [filters, setFilters] = useState<Filters>(DEFAULT_FILTERS);
  const [searchInput, setSearchInput] = useState('');
  const [offset, setOffset] = useState(0);
  const [snapState, setSnapState] = useState<Snapshot | null>(null);
  const [stale, setStale] = useState(false);
  const visibleAt = useRef(Date.now());
  const [data, setData] = useState<Page<Transfer> | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [exporting, setExporting] = useState(false);
  const [epoch, setEpoch] = useState(0);
  const [exportError, setExportError] = useState<string | null>(null);
  const seq = useRef(0);
  const t = useMessages(M);

  const snap = snapState?.serviceId === serviceId ? snapState : null;

  // Filtering or searching returns to the first page; an implicit history snapshot ends with it.
  const resetPaging = () => {
    seq.current++;
    setOffset(0);
    setSnapState((s) => (s?.auto ? null : s));
  };

  useDebounced(searchInput, 300, (v) => {
    if (filters.search === v) return;
    setFilters((f) => ({ ...f, search: v }));
    resetPaging();
  });

  useEffect(() => {
    if (focus) setSelectedId(focus.id);
  }, [focus]);

  // Another service's rows, selection and paging must not carry over; filters persist.
  useEffect(() => {
    seq.current++;
    setData(null);
    setError(null);
    setOffset(0);
    setSnapState(null);
    setSelectedId(null);
    setExportError(null);
  }, [serviceId]);

  useEffect(() => {
    setData(null);
    setError(null);
    setStale(true);
  }, [filters, offset, snap, serviceId, epoch]);

  const load = useCallback(async () => {
    const mine = ++seq.current;
    const at = snap?.at ?? Date.now();
    try {
      const page = await api<Page<Transfer>>(
        'GET',
        `/api/transfers${query({ ...filterParams(filters, at), service_id: serviceId, max_id: snap?.maxId, limit: PAGE, offset })}`,
      );
      if (mine !== seq.current) return;
      visibleAt.current = at;
      setData((cur) => (cur === null ? page : keepIfSame(cur, page)));
      setError(null);
      setStale(false);
    } catch (e) {
      if (mine === seq.current) {
        setError(errorMessage(e));
        setStale(false);
      }
    }
  }, [filters, offset, serviceId, snap, epoch]);

  usePoll(load, 1500, active && !snap, [load]);
  useEffect(() => {
    if (active && snap) void load();
  }, [active, snap, load]);

  const update = (patch: Partial<Filters>) => {
    setFilters((f) => ({ ...f, ...patch }));
    resetPaging();
  };

  // Reuse the boundary and clock of the displayed page, including records that arrived before the next poll.
  const freeze = (auto: boolean, nextOffset = 0) => {
    if (!data || stale) return;
    seq.current++;
    setSnapState({ serviceId, maxId: data.items[0]?.id ?? 0, at: visibleAt.current, auto });
    setOffset(nextOffset);
  };

  const resume = () => {
    seq.current++;
    setSnapState(null);
    setOffset(0);
  };

  const goPage = (o: number) => {
    if (stale) return;
    if (o === 0 && snap?.auto) resume();
    else if (o > 0 && !snap) void freeze(true, o);
    else setOffset(o);
  };

  const exportFiltered = async () => {
    setExporting(true);
    setExportError(null);
    try {
      const params = filterParams(filters, snap?.at);
      const items: Transfer[] = [];
      let total = Infinity;
      let maxId: number | undefined = snap?.maxId;
      while (items.length < total) {
        const page = await api<Page<Transfer>>('GET', `/api/transfers${query({ ...params, service_id: serviceId, max_id: maxId, limit: 200, offset: items.length })}`);
        if (maxId === undefined) maxId = page.items[0]?.id;
        total = page.total;
        if (page.items.length === 0) break;
        items.push(...page.items);
      }
      await saveExport('swiftcache-transfers.json', JSON.stringify({ exported_at: Date.now(), filters, total: items.length, items }, null, 2));
    } catch (e) {
      setExportError(errorMessage(e));
    } finally {
      setExporting(false);
    }
  };

  const cleared = () => {
    seq.current++;
    setOffset(0);
    setSnapState(null);
    setSelectedId(null);
    setExportError(null);
    setEpoch((n) => n + 1);
    onHistoryCleared();
  };

  const [up, upUnit] = bytesParts(summary.up_bytes);
  const [down, downUnit] = bytesParts(summary.down_bytes);
  const kvTotal = summary.kv_hit + summary.kv_miss;
  const filtered = filters.search || filters.direction || filters.service || filters.outcome || filters.range;

  return (
    <div className="stack gap-lg">
      <div className="stats">
        <StatCard title={t.uploadData} value={up} unit={upUnit} color="var(--purple)" note={t.uploadNote} />
        <StatCard title={t.downloadData} value={down} unit={downUnit} color="var(--blue)" note={t.downloadNote} />
        <StatCard
          title={t.kvHitRate}
          value={kvTotal ? ((summary.kv_hit / kvTotal) * 100).toFixed(1) : '—'}
          unit={kvTotal ? '%' : ''}
          color="var(--accent)"
          note={`${formatInt(summary.kv_hit)} hit / ${formatInt(summary.kv_miss)} miss`}
        />
        <StatCard
          title={t.errors}
          value={formatInt(summary.errors)}
          unit={t.errorUnit}
          color="var(--danger)"
          note={t.errorsNote}
          onClick={() => update({ outcome: 'error' })}
        />
      </div>

      <div className={`split${selectedId === null ? ' no-inspector' : ''}`}>
        <section className="card panel record-list">
          <div className="row between">
            <div className="row gap">
              <h2>{t.allTransfers}</h2>
              <span className="muted">{data ? formatInt(data.total) : '…'}</span>
            </div>
            <div className="row gap">
              <span className={`pill ${snap ? 'idle' : 'ok'}`}>{snap ? t.pausedPill : t.livePill}</span>
              <HistoryClear scope="transfers" serviceId={serviceId} serviceName={serviceName} locked={locked} onBusyChange={onClearBusy} onCleared={cleared} />
            </div>
          </div>
          <input
            className="input"
            placeholder={t.searchPlaceholder}
            aria-label={t.searchPlaceholder}
            value={searchInput}
            onChange={(e) => setSearchInput(e.target.value)}
          />
          <div className="row gap wrap">
            <select className="input select" aria-label={t.allDirections} value={filters.direction} onChange={(e) => update({ direction: e.target.value })}>
              <option value="">{t.allDirections}</option>
              <option value="up">{t.dirUp}</option>
              <option value="down">{t.dirDown}</option>
            </select>
            <select className="input select" aria-label="CAS / KV" value={filters.service} onChange={(e) => update({ service: e.target.value })}>
              <option value="">CAS / KV</option>
              <option value="CAS">CAS</option>
              <option value="KV">KV</option>
            </select>
            <select className="input select" aria-label={t.allResults} value={filters.outcome} onChange={(e) => update({ outcome: e.target.value })}>
              <option value="">{t.allResults}</option>
              <option value="success">{t.success}</option>
              <option value="hit">{t.hit}</option>
              <option value="miss">{t.miss}</option>
              <option value="error">{t.failed}</option>
            </select>
            <select className="input select" aria-label={t.rangeLabel} value={filters.range} onChange={(e) => update({ range: Number(e.target.value) })}>
              {RANGES.map((r) => (
                <option key={r.ms} value={r.ms}>
                  {t[r.label]}
                </option>
              ))}
            </select>
            {filtered ? (
              <button
                type="button"
                className="btn small"
                onClick={() => {
                  setFilters(DEFAULT_FILTERS);
                  setSearchInput('');
                  resetPaging();
                }}
              >
                {t.clearFilters}
              </button>
            ) : null}
            <span className="spacer" />
            <button type="button" className="btn" disabled={!snap && (!data || stale)} onClick={() => (snap ? resume() : freeze(false))}>
              {snap ? t.resume : t.pause}
            </button>
          </div>
          {snap && <Banner kind="info">{t.frozenNote}</Banner>}
          {selectedId === null && <p className="muted small">{t.pickTransfer}</p>}

          {error && (
            <Banner>
              {t.loadFailed(error)}{' '}
              <button type="button" className="link" onClick={() => void load()}>
                {t.retry}
              </button>
            </Banner>
          )}
          <div className="table-wrap records-table" aria-busy={stale}>
            <table className="table">
              <thead>
                <tr>
                  <th>{t.colTime}</th>
                  <th>{t.colOp}</th>
                  <th>{t.colObject}</th>
                  <th>{t.colResult}</th>
                  <th className="num">{t.size}</th>
                  <th className="num">{t.duration}</th>
                </tr>
              </thead>
              <tbody>
                {data?.items.map((tr, i) => (
                  <tr
                    key={tr.id}
                    data-record-id={tr.id}
                    className={tr.id === selectedId ? 'selected' : ''}
                    aria-current={tr.id === selectedId ? 'true' : undefined}
                    tabIndex={data.items.some((x) => x.id === selectedId) ? (tr.id === selectedId ? 0 : -1) : i === 0 ? 0 : -1}
                    onClick={() => setSelectedId(tr.id)}
                    onKeyDown={(e) => rowKeys(e, () => setSelectedId(tr.id))}
                  >
                    <td>
                      <div className="mono">{formatClock(tr.started_at)}</div>
                      <div className="muted small mono">
                        {formatDate(tr.started_at)} · #{String(tr.id).padStart(6, '0')}
                      </div>
                    </td>
                    <td>
                      <span className={`dir ${tr.direction}`}>{tr.direction === 'up' ? '↑' : '↓'}</span> {methodLabel(tr.method)}
                      <div className="muted small mono" title={t.source}>{tr.peer}</div>
                    </td>
                    <td className="mono">{tr.object_id ? `${tr.object_id.slice(0, 12)}…` : <span className="muted">{t.noId}</span>}</td>
                    <td>
                      <span className={`tag ${tr.outcome}`}>{OUTCOME_LABEL[tr.outcome]}</span>
                    </td>
                    <td className="num">{tr.bytes ? formatBytes(tr.bytes) : '—'}</td>
                    <td className="num">{formatDuration(tr.duration_us)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            {!data && !error && <div className="empty">{t.loadingList}</div>}
            {data && data.items.length === 0 && (
              <div className="empty">{filtered ? t.noMatch : t.noTransfers}</div>
            )}
          </div>
          <div className="row between">
            <span className="muted small">{t.missNote}</span>
            <button type="button" className="btn small" disabled={exporting || !data || data.total === 0} onClick={exportFiltered}>
              {exporting ? t.exporting : t.exportFiltered}
            </button>
          </div>
          {exportError && <Banner>{exportError}</Banner>}
          {data && <Pager offset={offset} limit={PAGE} total={data.total} disabled={stale} onChange={goPage} />}
        </section>

        {selectedId !== null && (
        <Inspector
          id={selectedId}
          notInList={selectedId !== null && !!data && !data.items.some((x) => x.id === selectedId)}
          onClose={() => { document.querySelector<HTMLElement>(`#panel-transfers tr[data-record-id="${selectedId}"]`)?.focus(); setSelectedId(null); }}
          onOpenEvent={onOpenEvent}
        />
        )}
      </div>
    </div>
  );
}

type Tab = 'overview' | 'json' | 'payload' | 'events';

function listItems(source: unknown, key: string): unknown[] | null {
  if (source === null || typeof source !== 'object') return null;
  const v = Reflect.get(source, key);
  return Array.isArray(v) ? v : null;
}

function Inspector({ id, notInList, onClose, onOpenEvent }: { id: number; notInList: boolean; onClose: () => void; onOpenEvent: (e: AppEvent) => void }) {
  const [transfer, setTransfer] = useState<Transfer | null>(null);
  const [events, setEvents] = useState<AppEvent[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>('overview');
  const [exporting, setExporting] = useState(false);
  const [retryKey, setRetryKey] = useState(0);
  const t = useMessages(M);

  useEffect(() => {
    setTransfer(null);
    setEvents([]);
    setError(null);
    setTab('overview');
    let live = true;
    api<Transfer>('GET', `/api/transfers/${id}`).then(
      (t) => live && setTransfer(t),
      (e) => live && setError(errorMessage(e)),
    );
    api<Page<AppEvent>>('GET', `/api/events${query({ transfer_id: id, limit: 200 })}`).then(
      (p) => live && setEvents(p.items.filter((ev) => ev.transfer_id === id)),
      (e) => live && setError((prev) => prev ?? errorMessage(e)),
    );
    return () => {
      live = false;
    };
  }, [id, retryKey]);

  const exportOne = async () => {
    setExporting(true);
    try {
      const full = await api<unknown>('GET', `/api/transfers/${id}/export`);
      await saveExport(`swiftcache-transfer-${id}.json`, JSON.stringify(full, null, 2));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setExporting(false);
    }
  };

  return (
    <aside className="card panel inspector" onKeyDown={(e) => { if (e.key === 'Escape') { e.stopPropagation(); onClose(); } }}>
      <div className="row between">
        <h2>{t.detailTitle}</h2>
        <button type="button" className="icon-btn" aria-label={t.closeDetail} onClick={onClose}>
          ×
        </button>
      </div>
      {error && <Banner>{error} <button type="button" className="link" onClick={() => setRetryKey((key) => key + 1)}>{t.retry}</button></Banner>}
      {notInList && <Banner kind="info">{t.notInList}</Banner>}
      {!transfer && !error && <div className="empty small">{t.loading}</div>}
      {transfer && (
        <>
          <div className="row gap wrap">
            <strong className="mono">#{String(transfer.id).padStart(6, '0')}</strong>
            <span className={`dir ${transfer.direction}`}>{transfer.direction === 'up' ? t.dirUp : t.dirDown}</span>
            <span>{methodLabel(transfer.method)}</span>
            <span className={`tag ${transfer.outcome}`}>{OUTCOME_LABEL[transfer.outcome]}</span>
          </div>
          <div className="tabs">
            {(
              [
                ['overview', t.tabOverview],
                ['json', t.tabJson],
                ['payload', `Payload ${transfer.payloads.length}`],
                ['events', t.tabEvents(events.length)],
              ] as [Tab, string][]
            ).map(([k, label]) => (
              <button type="button" key={k} className={tab === k ? 'on' : ''} aria-pressed={tab === k} onClick={() => setTab(k)}>
                {label}
              </button>
            ))}
          </div>

          {tab === 'overview' && (
            <div className="stack">
              <dl className="kv">
                <dt>{t.startedAt}</dt>
                <dd className="mono">{formatDateTime(transfer.started_at)}</dd>
                <dt>{t.duration}</dt>
                <dd className="mono">{formatDuration(transfer.duration_us)}</dd>
                <dt>{t.source}</dt>
                <dd className="mono">{transfer.peer}</dd>
                <dt>{t.grpcStatus}</dt>
                <dd className="mono">{transfer.grpc_status}</dd>
                <dt>{t.size}</dt>
                <dd className="mono">{formatBytes(transfer.bytes)} ({formatInt(transfer.bytes)} bytes)</dd>
                {transfer.error && (
                  <>
                    <dt>{t.errorLabel}</dt>
                    <dd className="danger-text">{transfer.error}</dd>
                  </>
                )}
              </dl>
              {transfer.object_id ? (
                <div className="stack tight">
                  <div className="row between">
                    <span className="label">{t.fullId}</span>
                    <CopyButton text={transfer.object_id} label={t.copyId} />
                  </div>
                  <div className="idbox mono">{transfer.object_id}</div>
                </div>
              ) : (
                <div className="muted small">{t.noObjectId}</div>
              )}
              {(['request', 'response'] as const).flatMap((side) =>
                (['references', 'refs', 'entries'] as const).map((key) => {
                  const items = listItems(transfer[side], key);
                  if (!items) return null;
                  return (
                    <div key={`${side}-${key}`} className="stack tight">
                      <div className="row between">
                        <span className="label">
                          {side === 'request' ? t.request : t.response} {key}
                        </span>
                        <span className="muted small">{t.items(items.length)}</span>
                      </div>
                      <ol className="refs mono">
                        {items.map((it, i) => (
                          <li key={i}>
                            <span className="muted">{String(i + 1).padStart(2, '0')}</span>{' '}
                            <span className="break">{typeof it === 'string' ? it : JSON.stringify(it)}</span>
                          </li>
                        ))}
                      </ol>
                    </div>
                  );
                }),
              )}
            </div>
          )}
          {tab === 'json' && (
            <div className="stack">
              <div className="label">Request</div>
              <JsonBlock value={transfer.request} />
              <div className="label">Response</div>
              <JsonBlock value={transfer.response} />
            </div>
          )}
          {tab === 'payload' && <PayloadViewer key={transfer.id} payloads={transfer.payloads} />}
          {tab === 'events' &&
            (events.length === 0 ? (
              <div className="empty small">{t.noLinked}</div>
            ) : (
              <ul className="linked">
                {events.map((ev) => (
                  <li key={ev.id}>
                    <button type="button" onClick={() => onOpenEvent(ev)}>
                      <span className={`tag lvl-${ev.level.toLowerCase()}`}>{ev.level}</span>
                      <span className="mono muted">{formatClock(ev.time)}</span>
                      <span className="ellipsis">{ev.message}</span>
                    </button>
                  </li>
                ))}
              </ul>
            ))}

          <div className="row end">
            <button type="button" className="btn" disabled={exporting} onClick={exportOne}>
              {exporting ? t.exporting : t.exportOne}
            </button>
          </div>
        </>
      )}
    </aside>
  );
}
