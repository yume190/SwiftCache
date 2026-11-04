import { useCallback, useEffect, useRef, useState } from 'react';
import { api, query } from '../api';
import { Banner, Check, Modal } from '../components';
import { errorMessage, formatBytes, formatDateTime, formatInt, formatShortDateTime } from '../format';
import { keepIfSame, usePoll } from '../hooks';
import { defineMessages, useMessages } from '../i18n';
import type { AppEvent, ClearPlan, ClearResult, Page, ServiceStatus, Usage } from '../types';

const M = defineMessages(
  {
    partObjects: 'CAS objects',
    partKv: 'Action cache',
    partTmp: '暫存寫入',
    noService: '請先儲存服務，並在側欄選擇後再檢視其快取。',
    failed: '失敗',
    succeeded: '成功',
    reclaimedLabel: '實際回收',
    reclaimedWithFiles: (bytes: string, files: string) => `${bytes} · ${files} 個檔案`,
    outcomeLabel: '操作結果',
    restartedAfter: '完成後已重啟服務',
    stoppedAfter: '完成後保持停止',
    localPathsLabel: '本機路徑',
    pathSep: '、',
    errorLabel: '錯誤',
    distribution: '快取資料分布',
    scanning: '掃描中…',
    rescan: '重新掃描用量',
    scanFailed: (err: string) => `無法掃描用量：${err}`,
    scanningUsage: '掃描用量中…',
    files: (n: string) => `${n} 個檔案`,
    partFiles: (n: string) => `${n} 個`,
    recordsLabel: '紀錄（獨立保存）',
    diskFree: '磁碟可用',
    scannedAt: '掃描時間',
    usageNote: '用量以掃描時的磁碟佔用為準；與累計上傳／下載流量分別計算。',
    manualClear: '手動清除',
    manualClearHint: '先選範圍，再查看完整路徑與影響；不會在此直接刪除。',
    serverClearNote: '清除會刪除此 Server 的 store。',
    proxyClearNote: '清除只影響此 Proxy 的本機快取 store；上游資料保留。遠端管理目前不提供。',
    serverCache: 'Server 快取',
    proxyCache: 'Proxy 本機快取',
    approx: (size: string) => `約 ${size}`,
    usageScanning: '用量掃描中',
    scopeInfo: (usage: string) => `objects/ + kv/ + tmp/ · ${usage} · CAS 與 KV 一併清空`,
    localScope: '本機建置快取與產物（選填）',
    noLocalPaths: '尚未設定本機路徑。可到「服務設定」的「本機建置快取路徑」新增。',
    keepNote: '保留：事件紀錄、傳輸明細、已保存的 payload 與服務設定。若勾選本機建置項目，下次建置會重新產生對應產物。',
    lastCleared: (bytes: string, files: string, restarted: boolean) =>
      `上次清除：回收 ${bytes}（${files} 個檔案），${restarted ? '服務已重啟' : '服務保持停止'}。`,
    reviewAndClear: '檢視範圍並清除…',
    mechanism: '清除機制',
    currentManual: '目前：手動清除',
    autoCleanup: '自動清理',
    notProvided: '未提供',
    quota: '容量上限 / quota',
    notImplemented: '尚未實作',
    ttl: '過期時間 / TTL',
    eviction: '淘汰策略 / LRU、GC',
    remoteMgmt: '遠端管理',
    remoteMgmtValue: '不提供（gRPC 僅含 CAS / KV）',
    uiFlow: 'UI 清除流程：暫停服務、等待請求完成後再清空。可選擇完成後重啟，恢復接收快取請求。',
    recentClears: '最近清除紀錄',
    collapse: '收合',
    viewAll: (n: number) => `查看全部（${n}）→`,
    noHistory: '尚無清除紀錄。',
    loading: '載入中…',
    historyNote: '每次清除記錄時間、範圍、結果與回收容量。清除失敗時保留原因，並標示服務目前狀態。',
    confirmTitle: '清除確認',
    confirmHeading: '清空 Server 快取？',
    confirmBody: '將刪除 CAS 物件與 action cache。下次查詢可能未命中並重新上傳。',
    confirmHeadingProxy: '清空 Proxy 本機快取？',
    confirmBodyProxy: '僅刪除此 Proxy 的本機 CAS 物件與 action cache；上游資料保留，之後的查詢會再從上游讀取。',
    planFailed: (err: string) => `無法取得清除計畫：${err}`,
    computingScope: '計算清除範圍…',
    deleteScope: '刪除範圍',
    estimated: '預計回收',
    estimatedApprox: (size: string) => `約 ${size}`,
    estimatedNote: '此為目前快照估計；最終回收容量以服務停止後的清除結果為準。',
    nonePicked: '未選擇（不會刪除任何本機建置快取）',
    keptData: '保留資料',
    keptDataValue: '事件紀錄、傳輸明細、已保存的 payload、設定',
    pausedDuring: '清除期間此服務暫時無法處理快取請求',
    alreadyStopped: '服務目前已停止',
    step1: '停止接收新請求，等待正在處理的請求完成',
    step2: (withPaths: boolean) => `一併清空 objects、kv 與暫存資料${withPaths ? '，以及所選本機路徑' : ''}`,
    step3: (restart: boolean) => `寫入清除結果；依下方選項${restart ? '重新啟動服務' : '保持服務停止'}`,
    restartAfter: '完成後重新啟動服務',
    cancelNote: '取消不會修改資料。清除結果與回收容量會保留在事件紀錄中。',
    clearFailed: (err: string) => `清除失敗：${err}。`,
    failedState: (running: boolean): string => (running ? '服務目前運行中。' : '服務目前已停止，詳情見事件紀錄。'),
    retryClear: '重試清除',
    serviceBusy: (busy: string) => `服務${busy === 'starting' ? '啟動中' : busy === 'stopping' ? '停止中' : '清除中'}，完成前無法使用清除操作。`,
    cancel: '取消',
    clearing: '清除中…',
    stopAndClear: '停止服務並清空快取',
    clearCache: '清空快取',
  },
  {
    partObjects: 'CAS objects',
    partKv: 'Action cache',
    partTmp: 'Temporary writes',
    noService: 'Save a service first, then pick it in the sidebar to see its cache.',
    failed: 'Failed',
    succeeded: 'Succeeded',
    reclaimedLabel: 'Reclaimed',
    reclaimedWithFiles: (bytes: string, files: string) => `${bytes} · ${files} files`,
    outcomeLabel: 'Result',
    restartedAfter: 'Service restarted afterward',
    stoppedAfter: 'Service left stopped afterward',
    localPathsLabel: 'Local paths',
    pathSep: ', ',
    errorLabel: 'Error',
    distribution: 'Cache distribution',
    scanning: 'Scanning…',
    rescan: 'Rescan usage',
    scanFailed: (err: string) => `Couldn’t scan usage: ${err}`,
    scanningUsage: 'Scanning usage…',
    files: (n: string) => `${n} files`,
    partFiles: (n: string) => `${n} files`,
    recordsLabel: 'Records (stored separately)',
    diskFree: 'Disk available',
    scannedAt: 'Scanned at',
    usageNote: 'Usage reflects disk space at scan time and is counted separately from cumulative upload/download traffic.',
    manualClear: 'Manual clear',
    manualClearHint: 'Choose a scope, then review full paths and impact. Nothing is deleted here.',
    serverClearNote: 'Clearing deletes this Server’s store.',
    proxyClearNote: 'Clearing affects only this Proxy’s local cache store; the upstream keeps its data. Remote management isn’t available.',
    serverCache: 'Server cache',
    proxyCache: 'Proxy local cache',
    approx: (size: string) => `About ${size}`,
    usageScanning: 'Scanning usage',
    scopeInfo: (usage: string) => `objects/ + kv/ + tmp/ · ${usage} · CAS and KV are cleared together`,
    localScope: 'Local build cache and artifacts (optional)',
    noLocalPaths: 'No local paths configured. Add one under “Service settings” › “Local build cache paths”.',
    keepNote: 'Kept: event log, transfer details, saved payloads, and service settings. If you select local build items, the next build regenerates them.',
    lastCleared: (bytes: string, files: string, restarted: boolean) =>
      `Last clear: reclaimed ${bytes} (${files} files); service ${restarted ? 'restarted' : 'left stopped'}.`,
    reviewAndClear: 'Review scope and clear…',
    mechanism: 'Clearing mechanism',
    currentManual: 'Current: manual clear',
    autoCleanup: 'Automatic cleanup',
    notProvided: 'Not available',
    quota: 'Size limit / quota',
    notImplemented: 'Not implemented yet',
    ttl: 'Expiry / TTL',
    eviction: 'Eviction policy / LRU, GC',
    remoteMgmt: 'Remote management',
    remoteMgmtValue: 'Not available (gRPC covers CAS / KV only)',
    uiFlow: 'Clearing from the UI: pause the service, wait for requests to finish, then clear. You can restart afterward to resume accepting cache requests.',
    recentClears: 'Recent clears',
    collapse: 'Collapse',
    viewAll: (n: number) => `View all (${n}) →`,
    noHistory: 'No clears yet.',
    loading: 'Loading…',
    historyNote: 'Each clear records its time, scope, result, and reclaimed size. If a clear fails, the reason is kept along with the current service state.',
    confirmTitle: 'Confirm clear',
    confirmHeading: 'Clear the Server cache?',
    confirmBody: 'This deletes CAS objects and the action cache. The next lookup may miss and re-upload.',
    confirmHeadingProxy: 'Clear the Proxy local cache?',
    confirmBodyProxy: 'This deletes only this Proxy’s local CAS objects and action cache. The upstream keeps its data; later lookups are read through from it again.',
    planFailed: (err: string) => `Couldn’t get the clear plan: ${err}`,
    computingScope: 'Calculating scope…',
    deleteScope: 'To be deleted',
    estimated: 'Estimated reclaim',
    estimatedApprox: (size: string) => `About ${size}`,
    estimatedNote: 'Based on a current snapshot; the final reclaimed size is the result reported after the service stops.',
    nonePicked: 'None selected (no local build cache will be deleted)',
    keptData: 'Kept data',
    keptDataValue: 'Event log, transfer details, saved payloads, settings',
    pausedDuring: 'This service can’t handle cache requests while clearing',
    alreadyStopped: 'The service is currently stopped',
    step1: 'Stop accepting new requests and wait for in-flight requests to finish',
    step2: (withPaths: boolean) => `Clear objects, kv, and temporary data${withPaths ? ', plus the selected local paths' : ''}`,
    step3: (restart: boolean) => `Record the result; then ${restart ? 'restart the service' : 'leave the service stopped'} per the option below`,
    restartAfter: 'Restart the service when done',
    cancelNote: 'Canceling doesn’t change any data. The result and reclaimed size are kept in the event log.',
    clearFailed: (err: string) => `Clear failed: ${err}.`,
    failedState: (running: boolean): string => (running ? 'The service is currently running.' : 'The service is currently stopped; see the event log for details.'),
    retryClear: 'Retry clear',
    serviceBusy: (busy: string) => `The service is ${busy === 'starting' ? 'starting' : busy === 'stopping' ? 'stopping' : 'clearing'}; clear actions are unavailable until it finishes.`,
    cancel: 'Cancel',
    clearing: 'Clearing…',
    stopAndClear: 'Stop service and clear cache',
    clearCache: 'Clear cache',
  },
);

const PARTS = [
  { key: 'objects', name: 'partObjects', dir: 'objects/', color: 'var(--accent)' },
  { key: 'kv', name: 'partKv', dir: 'kv/', color: 'var(--purple)' },
  { key: 'tmp', name: 'partTmp', dir: 'tmp/', color: 'var(--blue)' },
] as const;

function field(ev: AppEvent, key: string): unknown {
  return ev.fields && typeof ev.fields === 'object' ? Reflect.get(ev.fields, key) : undefined;
}

function CleanupItem({ ev }: { ev: AppEvent }) {
  const t = useMessages(M);
  const failed = ev.level === 'ERROR' || ev.level === 'WARN' || field(ev, 'ok') === false;
  const reclaimed = field(ev, 'reclaimed_bytes');
  const removed = field(ev, 'removed_files');
  const restarted = field(ev, 'restarted');
  const paths = field(ev, 'local_paths');
  const err = field(ev, 'error');
  return (
    <div className="cleanup">
      <div className="row gap">
        <span className={`tag ${failed ? 'error' : 'success'}`}>{failed ? t.failed : t.succeeded}</span>
        <span className="muted small">{formatShortDateTime(ev.time)} · {ev.message}</span>
      </div>
      <dl className="kv tight">
        <dt>{t.reclaimedLabel}</dt>
        <dd>
          {typeof reclaimed === 'number'
            ? typeof removed === 'number'
              ? t.reclaimedWithFiles(formatBytes(reclaimed), formatInt(removed))
              : formatBytes(reclaimed)
            : '—'}
        </dd>
        <dt>{t.outcomeLabel}</dt>
        <dd>{typeof restarted === 'boolean' ? (restarted ? t.restartedAfter : t.stoppedAfter) : '—'}</dd>
        {Array.isArray(paths) && paths.length > 0 && (
          <>
            <dt>{t.localPathsLabel}</dt>
            <dd className="mono break">{paths.map(String).join(t.pathSep)}</dd>
          </>
        )}
        {typeof err === 'string' && err && (
          <>
            <dt>{t.errorLabel}</dt>
            <dd className="danger-text break">{err}</dd>
          </>
        )}
      </dl>
    </div>
  );
}

type CachePageProps = {
  active: boolean;
  /** The selected, saved service; null when none is selected or it is still an unsaved draft. */
  service: ServiceStatus | null;
  usage: Usage | null;
  usageError: string | null;
  scanning: boolean;
  onRescan: () => void;
  onCleared: (u: Usage) => void;
  onStatusChanged: () => void;
  /** True only while a clear request is in flight, so the parent can lock header actions and service switching. */
  onBusyChange?: (busy: boolean) => void;
};

// Keyed by service so picked paths, confirmation, history expansion and the last result never carry across services.
export function CachePage(props: CachePageProps) {
  return <CachePageBody key={props.service?.id ?? ''} {...props} />;
}

function CachePageBody({ active, service, usage, usageError, scanning, onRescan, onCleared, onStatusChanged, onBusyChange }: CachePageProps) {
  const t = useMessages(M);
  const cfg = service?.config;
  const serve = cfg?.mode === 'serve';
  const serviceBusy = service?.busy != null;
  const localPaths = (cfg?.local_cache_paths ?? []).map((p) => p.trim()).filter(Boolean);
  const [picked, setPicked] = useState<string[]>([]);
  const [confirming, setConfirming] = useState(false);
  const [history, setHistory] = useState<AppEvent[] | null>(null);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [allHistory, setAllHistory] = useState(false);
  const [lastResult, setLastResult] = useState<ClearResult | null>(null);
  // Latest callbacks for requests that complete after later renders; `alive` is false once this view is unmounted.
  const cb = useRef({ onCleared, onStatusChanged, onBusyChange });
  cb.current = { onCleared, onStatusChanged, onBusyChange };
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  // Drop selections that are no longer configured paths.
  useEffect(() => {
    setPicked((p) => p.filter((x) => localPaths.includes(x)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [(cfg?.local_cache_paths ?? []).join('\n')]);

  const serviceId = service?.id;
  // A different service has its own cleanup history.
  const historySeq = useRef(0);
  useEffect(() => {
    historySeq.current++;
    setHistory(null);
    setHistoryError(null);
  }, [serviceId]);

  const loadHistory = useCallback(async () => {
    const mine = ++historySeq.current;
    try {
      const page = await api<Page<AppEvent>>('GET', `/api/events${query({ kind: 'cleanup', service_id: serviceId, limit: 50 })}`);
      if (mine !== historySeq.current) return;
      setHistory((cur) => (cur === null ? page.items : keepIfSame(cur, page.items)));
      setHistoryError(null);
    } catch (e) {
      if (mine === historySeq.current) setHistoryError(errorMessage(e));
    }
  }, [serviceId]);

  usePoll(loadHistory, 10_000, active, [loadHistory]);

  const total = usage ? Math.max(usage.total_bytes, 1) : 1;
  const shownHistory = history ? (allHistory ? history : history.slice(0, 3)) : [];

  if (!service) {
    return (
      <section className="card panel">
        <div className="empty small">{t.noService}</div>
      </section>
    );
  }

  return (
    <div className="cache-grid">
      <div className="stack gap-lg">
        <section className="card panel">
          <div className="row between">
            <h2>{t.distribution}</h2>
            <button type="button" className="btn small" disabled={scanning || serviceBusy} onClick={onRescan}>
              {scanning ? t.scanning : t.rescan}
            </button>
          </div>
          {usageError && <Banner>{t.scanFailed(usageError)}</Banner>}
          {!usage && !usageError && <div className="empty small">{t.scanningUsage}</div>}
          {usage && (
            <>
              <div className="big-total">
                <strong>{formatBytes(usage.total_bytes)}</strong>
                <span className="muted"> · {t.files(formatInt(usage.total_files))}</span>
              </div>
              <div className="bar tall">
                {PARTS.map((p) => (
                  <i key={p.key} style={{ width: `${(usage[p.key].bytes / total) * 100}%`, background: p.color }} />
                ))}
              </div>
              <div className="parts">
                {PARTS.map((p) => (
                  <div className="part" key={p.key}>
                    <span className="swatch" style={{ background: p.color }} />
                    <strong>{t[p.name]}</strong>
                    <span className="mono muted">{p.dir}</span>
                    <span className="spacer" />
                    <span>{t.partFiles(formatInt(usage[p.key].files))}</span>
                    <span className="num mono">{formatBytes(usage[p.key].bytes)}</span>
                  </div>
                ))}
              </div>
            </>
          )}
          {usage && (
            <dl className="kv">
              <dt>{t.recordsLabel}</dt>
              <dd>
                {formatBytes(usage.records.bytes)} · {t.files(formatInt(usage.records.files))}
              </dd>
              <dt>{t.diskFree}</dt>
              <dd>
                {formatBytes(usage.disk_available)} / {formatBytes(usage.disk_total)}
              </dd>
              <dt>{t.scannedAt}</dt>
              <dd>{formatDateTime(usage.scanned_at)}</dd>
            </dl>
          )}
          <p className="muted small">{t.usageNote}</p>
        </section>

        <section className="card panel">
          <div>
            <h2>{t.manualClear}</h2>
            <p className="muted small">{t.manualClearHint}</p>
          </div>
          <p className="muted small">{serve ? t.serverClearNote : t.proxyClearNote}</p>
            <>
              <div className="scope on">
                <Check checked disabled onChange={() => undefined}>
                  <strong>{serve ? t.serverCache : t.proxyCache}</strong>
                </Check>
                <span className="muted small">
                  {t.scopeInfo(usage ? t.approx(formatBytes(usage.total_bytes)) : t.usageScanning)}
                </span>
              </div>
              <div className="scope">
                <div className="label">{t.localScope}</div>
                {localPaths.length === 0 ? (
                  <span className="muted small">{t.noLocalPaths}</span>
                ) : (
                  localPaths.map((p) => (
                    <Check
                      key={p}
                      checked={picked.includes(p)}
                      disabled={serviceBusy}
                      onChange={(v) => setPicked((cur) => (v ? [...cur, p] : cur.filter((x) => x !== p)))}
                    >
                      <span className="mono break">{p}</span>
                    </Check>
                  ))
                )}
              </div>
              <p className="muted small">{t.keepNote}</p>
              {serviceBusy && <Banner kind="info">{t.serviceBusy(service.busy ?? '')}</Banner>}
              {lastResult && (
                <Banner kind="info">
                  {t.lastCleared(formatBytes(lastResult.reclaimed_bytes), formatInt(lastResult.removed_files), lastResult.restarted)}
                </Banner>
              )}
              <div className="row end">
                <button type="button" className="btn danger" disabled={serviceBusy} onClick={() => setConfirming(true)}>
                  {t.reviewAndClear}
                </button>
              </div>
            </>
        </section>
      </div>

      <div className="stack gap-lg">
        <section className="card panel">
          <div className="row between">
            <h2>{t.mechanism}</h2>
            <span className="pill ok">{t.currentManual}</span>
          </div>
          <dl className="kv">
            <dt>{t.autoCleanup}</dt>
            <dd>{t.notProvided}</dd>
            <dt>{t.quota}</dt>
            <dd>{t.notImplemented}</dd>
            <dt>{t.ttl}</dt>
            <dd>{t.notImplemented}</dd>
            <dt>{t.eviction}</dt>
            <dd>{t.notImplemented}</dd>
            <dt>{t.remoteMgmt}</dt>
            <dd>{t.remoteMgmtValue}</dd>
          </dl>
          <p className="muted small">{t.uiFlow}</p>
        </section>

        <section className="card panel">
          <div className="row between">
            <h2>{t.recentClears}</h2>
            {history && history.length > 3 && (
              <button type="button" className="link" onClick={() => setAllHistory((v) => !v)}>
                {allHistory ? t.collapse : t.viewAll(history.length)}
              </button>
            )}
          </div>
          {historyError && <Banner>{historyError}</Banner>}
          {history && history.length === 0 && <div className="empty small">{t.noHistory}</div>}
          {!history && !historyError && <div className="empty small">{t.loading}</div>}
          {shownHistory.map((ev) => (
            <CleanupItem key={ev.id} ev={ev} />
          ))}
          <p className="muted small">{t.historyNote}</p>
        </section>
      </div>

      {confirming && (
        <ClearDialog
          proxy={!serve}
          serviceId={service.id}
          running={service.running}
          serviceBusy={service.busy}
          picked={picked}
          onClose={() => setConfirming(false)}
          onBusyChange={(b) => cb.current.onBusyChange?.(b)}
          onDone={(r) => {
            // The request may outlive this view (service switched): only a still-mounted view may publish usage.
            if (alive.current) {
              setLastResult(r);
              setConfirming(false);
              cb.current.onCleared(r.usage);
            }
            cb.current.onStatusChanged();
            void loadHistory();
          }}
          onFailed={() => {
            cb.current.onStatusChanged();
            void loadHistory();
          }}
        />
      )}
    </div>
  );
}

function ClearDialog({
  proxy,
  serviceId,
  running,
  serviceBusy,
  picked,
  onClose,
  onBusyChange,
  onDone,
  onFailed,
}: {
  proxy: boolean;
  serviceId: string;
  running: boolean;
  /** Server-reported transition (starting/stopping/clearing) for this service, if any. */
  serviceBusy: string | null;
  picked: string[];
  onClose: () => void;
  onBusyChange: (busy: boolean) => void;
  onDone: (r: ClearResult) => void;
  onFailed: () => void;
}) {
  const t = useMessages(M);
  const [plan, setPlan] = useState<ClearPlan | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const [planKey, setPlanKey] = useState(0);
  const [restart, setRestart] = useState(running);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    api<ClearPlan>('GET', `/api/clear-plan${query({ service: serviceId })}`).then(
      (p) => live && (setPlan(p), setPlanError(null)),
      (e) => live && setPlanError(errorMessage(e)),
    );
    return () => {
      live = false;
    };
  }, [serviceId, planKey]);

  const confirm = async () => {
    if (!plan || busy || serviceBusy !== null) return;
    setBusy(true);
    onBusyChange(true);
    setError(null);
    let result: ClearResult | null = null;
    try {
      result = await api<ClearResult>('POST', '/api/clear', { service: serviceId, restart, local_paths: picked, plan_id: plan.plan_id });
    } catch (e) {
      setError(errorMessage(e));
      // Refetch after a failure so the next attempt confirms the current paths and state.
      setPlan(null);
      setPlanKey((k) => k + 1);
    } finally {
      setBusy(false);
      onBusyChange(false);
    }
    // End the parent's mutation generation before its completion callback refreshes status.
    if (result) onDone(result);
    else onFailed();
  };

  const isRunning = plan?.running ?? running;
  const blocked = serviceBusy !== null && !busy;
  return (
    <Modal title={t.confirmTitle} onClose={busy ? () => undefined : onClose}>
      <div className="modal-body">
        <div className="label accent">{t.confirmTitle}</div>
        <h2>{proxy ? t.confirmHeadingProxy : t.confirmHeading}</h2>
        <p className="muted">{proxy ? t.confirmBodyProxy : t.confirmBody}</p>
        {planError && <Banner>{t.planFailed(planError)}</Banner>}
        {!plan && !planError && <div className="empty small">{t.computingScope}</div>}
        {plan && (
          <dl className="kv wide">
            <dt>{t.deleteScope}</dt>
            <dd>
              <div className="mono break">{plan.store}</div>
              <ul className="plain mono small">
                {plan.paths.map((p) => (
                  <li key={p} className="break">
                    {p}
                  </li>
                ))}
              </ul>
            </dd>
            <dt>{t.estimated}</dt>
            <dd>
              {t.estimatedApprox(formatBytes(plan.estimated_bytes))}
              <div className="muted small">{t.estimatedNote}</div>
            </dd>
            <dt>{t.localPathsLabel}</dt>
            <dd>
              {picked.length === 0 ? (
                <span className="muted">{t.nonePicked}</span>
              ) : (
                <ul className="plain mono small">
                  {picked.map((p) => (
                    <li key={p} className="break">
                      {p}
                    </li>
                  ))}
                </ul>
              )}
            </dd>
            <dt>{t.keptData}</dt>
            <dd>{t.keptDataValue}</dd>
          </dl>
        )}
        <div className="notice-steps">
          <strong>{isRunning ? t.pausedDuring : t.alreadyStopped}</strong>
          <ol>
            {isRunning && <li>{t.step1}</li>}
            <li>{t.step2(picked.length > 0)}</li>
            <li>{t.step3(restart)}</li>
          </ol>
        </div>
        <Check checked={restart} disabled={busy} onChange={setRestart}>
          {t.restartAfter}
        </Check>
        <p className="muted small">{t.cancelNote}</p>
        {blocked && serviceBusy && <Banner kind="info">{t.serviceBusy(serviceBusy)}</Banner>}
        {error && (
          <Banner>
            {t.clearFailed(error)} {plan && t.failedState(plan.running)}
          </Banner>
        )}
        <div className="row end gap">
          <button type="button" className="btn" disabled={busy} onClick={onClose}>
            {t.cancel}
          </button>
          <button type="button" className="btn danger solid" disabled={busy || !plan || blocked} onClick={confirm}>
            {busy ? t.clearing : error ? t.retryClear : isRunning ? t.stopAndClear : t.clearCache}
          </button>
        </div>
      </div>
    </Modal>
  );
}
