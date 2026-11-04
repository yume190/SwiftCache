import { useCallback, useEffect, useRef, useState, type KeyboardEvent } from 'react';
import { api, query, saveExport } from '../api';
import { Banner, CopyButton, JsonBlock, Pager } from '../components';
import { keepIfSame, useDebounced, usePoll } from '../hooks';
import { errorMessage, formatClock, formatDate, formatDateTime, formatInt } from '../format';
import { defineMessages, useMessages } from '../i18n';
import type { AppEvent, Page } from '../types';
import { HistoryClear } from '../HistoryClear';

const M = defineMessages(
  {
    r15m: '最近 15 分鐘',
    r1h: '最近 1 小時',
    r24h: '最近 24 小時',
    r7d: '最近 7 天',
    rAll: '全部時間',
    title: '事件',
    paused: '● 已凍結快照',
    live: '● 持續更新',
    searchPlaceholder: '搜尋訊息、來源或欄位內容…',
    allLevels: '全部層級',
    kindPlaceholder: '類型（全部）',
    clearFilters: '清除篩選',
    resume: '回到最新並繼續更新',
    pause: '暫停並凍結清單',
    loadFailed: (e: string) => `無法載入事件：${e}`,
    colTime: '時間',
    colLevel: '層級',
    colKind: '類型',
    colMessage: '訊息',
    colTransfer: '關聯傳輸',
    colSource: '來源',
    loading: '載入事件…',
    noMatch: '沒有符合篩選條件的事件。',
    noEvents: '尚無事件。',
    clickHint: '點選任一事件，可回看原始訊息及其關聯傳輸。',
    exporting: '匯出中…',
    exportFiltered: '匯出篩選結果',
    detailsTitle: '事件詳細資料',
    closeDetails: '關閉詳細資料',
    selectHint: '選擇任一事件，查看完整訊息與結構化欄位。',
    none: '無',
    gotoTransfer: (id: string) => `前往對應傳輸 #${id} ↗`,
    rawEvent: '原始事件 / 結構化欄位',
    copyJson: '複製 JSON',
    exportEvent: '匯出此事件',
    levelLabel: '層級',
    rangeLabel: '時間範圍',
    retry: '重試',
    frozenNote: '正在瀏覽凍結的快照：新事件不會插入，也不會讓分頁位移。回到最新即可看到新事件。',
    notInList: '此事件不在目前清單頁中（可能被篩選條件排除、位於其他頁，或比凍結快照更新）。',
  },
  {
    r15m: 'Last 15 minutes',
    r1h: 'Last hour',
    r24h: 'Last 24 hours',
    r7d: 'Last 7 days',
    rAll: 'All time',
    title: 'Events',
    paused: '● Frozen snapshot',
    live: '● Live',
    searchPlaceholder: 'Search messages, sources, or field contents…',
    allLevels: 'All levels',
    kindPlaceholder: 'Kind (all)',
    clearFilters: 'Clear filters',
    resume: 'Back to latest & resume',
    pause: 'Pause & freeze list',
    loadFailed: (e: string) => `Couldn’t load events: ${e}`,
    colTime: 'Time',
    colLevel: 'Level',
    colKind: 'Kind',
    colMessage: 'Message',
    colTransfer: 'Related transfer',
    colSource: 'Source',
    loading: 'Loading events…',
    noMatch: 'No events match the filters.',
    noEvents: 'No events yet.',
    clickHint: 'Select an event to see its raw message and related transfer.',
    exporting: 'Exporting…',
    exportFiltered: 'Export filtered results',
    detailsTitle: 'Event details',
    closeDetails: 'Close details',
    selectHint: 'Select an event to see its full message and structured fields.',
    none: 'None',
    gotoTransfer: (id: string) => `Go to transfer #${id} ↗`,
    rawEvent: 'Raw event / structured fields',
    copyJson: 'Copy JSON',
    exportEvent: 'Export this event',
    levelLabel: 'Level',
    rangeLabel: 'Time range',
    retry: 'Retry',
    frozenNote: 'Browsing a frozen snapshot: new events won’t be inserted or shift these pages. Go back to latest to see them.',
    notInList: 'This event isn’t on the current list page (it may be filtered out, on another page, or newer than the frozen snapshot).',
  },
);

const PAGE = 50;
const RANGES: { key: 'r15m' | 'r1h' | 'r24h' | 'r7d' | 'rAll'; ms: number }[] = [
  { key: 'r15m', ms: 15 * 60_000 },
  { key: 'r1h', ms: 60 * 60_000 },
  { key: 'r24h', ms: 24 * 60 * 60_000 },
  { key: 'r7d', ms: 7 * 24 * 60 * 60_000 },
  { key: 'rAll', ms: 0 },
];
const KINDS = ['rpc', 'service', 'cleanup', 'log'];

interface Filters {
  search: string;
  level: string;
  kind: string;
  range: number;
}
const DEFAULT_FILTERS: Filters = { search: '', level: '', kind: '', range: 0 };

function params(f: Filters, now = Date.now()) {
  return { search: f.search, level: f.level, kind: f.kind, since: f.range ? now - f.range : undefined };
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

export function EventsPage({
  active,
  serviceId,
  serviceName,
  locked,
  focus,
  onOpenTransfer,
  onClearBusy,
  onHistoryCleared,
}: {
  active: boolean;
  serviceId: string;
  serviceName: string;
  /** A service operation or history clear is in flight, or the daemon is unreachable. */
  locked: boolean;
  focus: { event: AppEvent; nonce: number } | null;
  onOpenTransfer: (id: number) => void;
  onClearBusy: (busy: boolean) => void;
  onHistoryCleared: () => void;
}) {
  const [filters, setFilters] = useState<Filters>(DEFAULT_FILTERS);
  const [searchInput, setSearchInput] = useState('');
  const [offset, setOffset] = useState(0);
  const [snapState, setSnapState] = useState<Snapshot | null>(null);
  const [stale, setStale] = useState(false);
  const visibleAt = useRef(Date.now());
  const [data, setData] = useState<Page<AppEvent> | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<AppEvent | null>(null);
  const [exporting, setExporting] = useState(false);
  const [epoch, setEpoch] = useState(0);
  const [exportError, setExportError] = useState<string | null>(null);
  const seq = useRef(0);

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
    if (focus) setSelected(focus.event);
  }, [focus]);

  // Another service's rows, selection and paging must not carry over; filters persist.
  useEffect(() => {
    seq.current++;
    setData(null);
    setError(null);
    setOffset(0);
    setSnapState(null);
    setSelected(null);
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
      const page = await api<Page<AppEvent>>(
        'GET',
        `/api/events${query({ ...params(filters, at), service_id: serviceId, max_id: snap?.maxId, limit: PAGE, offset })}`,
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

  // Reuse the boundary and clock of the displayed page so adjacent history pages cannot overlap.
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

  const run = async (job: () => Promise<void>) => {
    setExporting(true);
    setExportError(null);
    try {
      await job();
    } catch (e) {
      setExportError(errorMessage(e));
    } finally {
      setExporting(false);
    }
  };

  const exportAll = () =>
    run(async () => {
      const items: AppEvent[] = [];
      let total = Infinity;
      let maxId: number | undefined = snap?.maxId;
      const snapshotFilters = params(filters, snap?.at);
      while (items.length < total) {
        const page = await api<Page<AppEvent>>('GET', `/api/events${query({ ...snapshotFilters, service_id: serviceId, max_id: maxId, limit: 200, offset: items.length })}`);
        if (maxId === undefined) maxId = page.items[0]?.id;
        total = page.total;
        if (page.items.length === 0) break;
        items.push(...page.items);
      }
      await saveExport('swiftcache-events.json', JSON.stringify({ exported_at: Date.now(), filters, total: items.length, items }, null, 2));
    });

  const cleared = () => {
    seq.current++;
    setOffset(0);
    setSnapState(null);
    setSelected(null);
    setExportError(null);
    setEpoch((n) => n + 1);
    onHistoryCleared();
  };

  const filtered = filters.search || filters.level || filters.kind || filters.range;
  const t = useMessages(M);

  return (
    <div className={`split${selected === null ? ' no-inspector' : ''}`}>
      <section className="card panel record-list events-list">
        <div className="row between">
          <div className="row gap">
            <h2>{t.title}</h2>
            <span className="muted">{data ? formatInt(data.total) : '…'}</span>
          </div>
          <div className="row gap">
            <span className={`pill ${snap ? 'idle' : 'ok'}`}>{snap ? t.paused : t.live}</span>
            <HistoryClear scope="events" serviceId={serviceId} serviceName={serviceName} locked={locked} onBusyChange={onClearBusy} onCleared={cleared} />
          </div>
        </div>
        <input className="input" placeholder={t.searchPlaceholder} aria-label={t.searchPlaceholder} value={searchInput} onChange={(e) => setSearchInput(e.target.value)} />
        <div className="row gap wrap">
          <select className="input select" aria-label={t.levelLabel} value={filters.level} onChange={(e) => update({ level: e.target.value })}>
            <option value="">{t.allLevels}</option>
            {['ERROR', 'WARN', 'INFO', 'DEBUG', 'TRACE'].map((l) => (
              <option key={l}>{l}</option>
            ))}
          </select>
          <input
            className="input kind-input"
            list="event-kinds"
            placeholder={t.kindPlaceholder}
            aria-label={t.kindPlaceholder}
            value={filters.kind}
            onChange={(e) => update({ kind: e.target.value.trim() })}
          />
          <datalist id="event-kinds">
            {KINDS.map((k) => (
              <option key={k} value={k} />
            ))}
          </datalist>
          <select className="input select" aria-label={t.rangeLabel} value={filters.range} onChange={(e) => update({ range: Number(e.target.value) })}>
            {RANGES.map((r) => (
              <option key={r.ms} value={r.ms}>
                {t[r.key]}
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
        {selected === null && <p className="muted small">{t.selectHint}</p>}
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
                <th>{t.colLevel}</th>
                <th>{t.colKind}</th>
                <th>{t.colMessage}</th>
                <th>{t.colTransfer}</th>
                <th>{t.colSource}</th>
              </tr>
            </thead>
            <tbody>
              {data?.items.map((ev, i) => (
                <tr
                  key={ev.id}
                  data-record-id={ev.id}
                  className={ev.id === selected?.id ? 'selected' : ''}
                  aria-current={ev.id === selected?.id ? 'true' : undefined}
                  tabIndex={data.items.some((x) => x.id === selected?.id) ? (ev.id === selected?.id ? 0 : -1) : i === 0 ? 0 : -1}
                  onClick={() => setSelected(ev)}
                  onKeyDown={(e) => rowKeys(e, () => setSelected(ev))}
                >
                  <td>
                    <div className="mono">{formatClock(ev.time)}</div>
                    <div className="muted small mono">{formatDate(ev.time)}</div>
                  </td>
                  <td>
                    <span className={`tag lvl-${ev.level.toLowerCase()}`}>{ev.level}</span>
                  </td>
                  <td>{ev.kind}</td>
                  <td className="msg">{ev.message}</td>
                  <td className="mono">{ev.transfer_id !== null ? `#${String(ev.transfer_id).padStart(6, '0')}` : <span className="muted">—</span>}</td>
                  <td className="mono">{ev.peer ?? <span className="muted">server</span>}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {!data && !error && <div className="empty">{t.loading}</div>}
          {data && data.items.length === 0 && <div className="empty">{filtered ? t.noMatch : t.noEvents}</div>}
        </div>
        <div className="row between">
          <span className="muted small">{t.clickHint}</span>
          <button type="button" className="btn small" disabled={exporting || !data || data.total === 0} onClick={exportAll}>
            {exporting ? t.exporting : t.exportFiltered}
          </button>
        </div>
        {exportError && <Banner>{exportError}</Banner>}
        {data && <Pager offset={offset} limit={PAGE} total={data.total} disabled={stale} onChange={goPage} />}
      </section>

      {selected && (
      <aside className="card panel inspector" onKeyDown={(e) => { if (e.key === 'Escape') { e.stopPropagation(); document.querySelector<HTMLElement>(`#panel-events tr[data-record-id="${selected.id}"]`)?.focus(); setSelected(null); } }}>
        <div className="row between">
          <h2>{t.detailsTitle}</h2>
            <button type="button" className="icon-btn" aria-label={t.closeDetails} onClick={() => { document.querySelector<HTMLElement>(`#panel-events tr[data-record-id="${selected.id}"]`)?.focus(); setSelected(null); }}>
              ×
            </button>
        </div>
        {selected && data && !data.items.some((x) => x.id === selected.id) && <Banner kind="info">{t.notInList}</Banner>}
            <div className="row gap wrap">
              <span className={`tag lvl-${selected.level.toLowerCase()}`}>{selected.level}</span>
              <span className="mono muted">{formatDateTime(selected.time)}</span>
            </div>
            <dl className="kv">
              <dt>{t.colKind}</dt>
              <dd>{selected.kind}</dd>
              <dt>{t.colMessage}</dt>
              <dd className="break">{selected.message}</dd>
              <dt>{t.colSource}</dt>
              <dd className="mono">{selected.peer ?? 'server'}</dd>
              <dt>{t.colTransfer}</dt>
              <dd>
                {selected.transfer_id !== null ? (
                  <button type="button" className="link" onClick={() => onOpenTransfer(selected.transfer_id!)}>
                    {t.gotoTransfer(String(selected.transfer_id).padStart(6, '0'))}
                  </button>
                ) : (
                  <span className="muted">{t.none}</span>
                )}
              </dd>
            </dl>
            <div className="row between">
              <span className="label">{t.rawEvent}</span>
              <div className="row gap">
                <CopyButton text={JSON.stringify(selected, null, 2)} label={t.copyJson} />
                <button
                  type="button"
                  className="btn small"
                  onClick={() => void run(async () => void (await saveExport(`swiftcache-event-${selected.id}.json`, JSON.stringify(selected, null, 2))))}
                >
                  {t.exportEvent}
                </button>
              </div>
            </div>
            <JsonBlock value={selected} />
      </aside>
      )}
    </div>
  );
}
