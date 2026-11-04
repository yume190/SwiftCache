import { useCallback, useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { api, inTauri, query } from './api';
import { Banner, Modal } from './components';
import { buildCli, errorMessage, formatBytes, formatInt, formatUptime, listenerText, toMs } from './format';
import { keepIfSame, usePoll, useVisible } from './hooks';
import { defineMessages, LocaleProvider, resolveLocale, useMessages } from './i18n';
import { Preferences } from './Preferences';
import { defaultPrefs, loadPrefs, savePrefs, useTheme, type Prefs } from './prefs';
import { CachePage } from './pages/CachePage';
import { EventsPage } from './pages/EventsPage';
import { ServicePage, validate } from './pages/ServicePage';
import { TransfersPage } from './pages/TransfersPage';
import type { AppEvent, Config, PageId, ServiceStatus, Status, Usage } from './types';

const M = defineMessages(
  {
    uptime: (t: string) => `已執行 ${t}`,
    saveFailed: (e: string) => `儲存設定失敗：${e}`,
    cannotConnect: (e: string) => `無法連線到管理服務：${e}`,
    bootHint: '將持續自動重試。若為瀏覽器開發模式，請確認已啟動 swiftcache daemon，並以 SWIFTCACHE_DATA_DIR 或 SWIFTCACHE_DAEMON_FILE 指向其 daemon.json。',
    connecting: '連線到 SwiftCache daemon…',
    connections: '連線',
    addConnection: '新增連線',
    noConnections: '儲存服務設定後會出現在這裡。',
    emptyHint: '尚未建立任何連線。新增一個連線，即可設定、啟動並查看它的傳輸與快取。',
    removeConnection: '刪除這筆連線',
    removeLocked: '服務執行中，停止後才能刪除',
    newConnection: '+ 新增連線',
    newUnsaved: '新連線（未儲存）',
    stateRunning: '執行中',
    stateStopped: '已停止',
    stateError: '錯誤',
    settings: '設定',
    tabConfig: '組態',
    tabTransfers: '傳輸明細',
    tabEvents: '事件紀錄',
    tabCache: '快取',
    unset: '未設定',
    working: '處理中…',
    stop: '■ 停止服務',
    start: '▶ 啟動服務',
    disconnected: (e: string) => `與管理服務的連線中斷（${e}），顯示的是最後一次取得的狀態。`,
    serviceError: (e: string) => `服務錯誤：${e}`,
    dismiss: '關閉訊息',
    unsaved: '尚未儲存',
    saved: '設定已儲存。',
    started: '服務已啟動。',
    stopped: '服務已停止。',
    deleted: '連線設定已刪除，快取與紀錄仍保留。',
    saving: '儲存中…',
    starting: '啟動中…',
    stopping: '停止中…',
    clearing: '清除中…',
    deleting: '刪除中…',
    leaveTitle: '要儲存設定變更嗎？',
    leaveBody: '切換連線前，請選擇儲存或捨棄目前的變更。',
    saveContinue: '儲存並繼續',
    discard: '捨棄變更',
    cancel: '取消',
    deleteTitle: '刪除連線設定？',
    deleteBody: '只移除這筆連線設定。磁碟快取與傳輸紀錄會保留。',
    confirmDelete: '刪除設定',
    saveStart: '儲存並啟動',
    saveConfig: '儲存設定',
    deleteConnection: '刪除連線',
    connectionTabs: '連線詳細資料',
    transfersCleared: '傳輸明細已清除，事件紀錄、payload、快取與設定仍保留。',
    eventsCleared: '事件紀錄已清除，傳輸明細、統計、payload、快取與設定仍保留。',
  },
  {
    uptime: (t: string) => `Up ${t}`,
    saveFailed: (e: string) => `Couldn't save settings: ${e}`,
    cannotConnect: (e: string) => `Cannot reach the management service: ${e}`,
    bootHint: 'Retrying automatically. In browser dev mode, make sure swiftcache daemon is running and SWIFTCACHE_DATA_DIR or SWIFTCACHE_DAEMON_FILE points at its daemon.json.',
    connecting: 'Connecting to the SwiftCache daemon…',
    connections: 'Connections',
    addConnection: 'New connection',
    noConnections: 'Saved service settings show up here.',
    emptyHint: 'No connections yet. Add one to configure it, start it, and see its transfers and cache.',
    removeConnection: 'Delete this connection',
    removeLocked: 'Stop the service to delete it',
    newConnection: '+ New connection',
    newUnsaved: 'New connection (unsaved)',
    stateRunning: 'Running',
    stateStopped: 'Stopped',
    stateError: 'Error',
    settings: 'Settings',
    tabConfig: 'Configuration',
    tabTransfers: 'Transfers',
    tabEvents: 'Events',
    tabCache: 'Cache',
    unset: 'Not set',
    working: 'Working…',
    stop: '■ Stop service',
    start: '▶ Start service',
    disconnected: (e: string) => `Lost the connection to the management service (${e}); showing the last known state.`,
    serviceError: (e: string) => `Service error: ${e}`,
    dismiss: 'Dismiss',
    unsaved: 'Unsaved changes',
    saved: 'Settings saved.',
    started: 'Service started.',
    stopped: 'Service stopped.',
    deleted: 'Connection settings deleted. Cache and history are kept.',
    saving: 'Saving…',
    starting: 'Starting…',
    stopping: 'Stopping…',
    clearing: 'Clearing…',
    deleting: 'Deleting…',
    leaveTitle: 'Save your changes?',
    leaveBody: 'Save or discard your changes before switching connections.',
    saveContinue: 'Save and continue',
    discard: 'Discard changes',
    cancel: 'Cancel',
    deleteTitle: 'Delete connection settings?',
    deleteBody: 'Only this connection’s settings will be removed. Disk cache and transfer history are kept.',
    confirmDelete: 'Delete settings',
    saveStart: 'Save and start',
    saveConfig: 'Save settings',
    deleteConnection: 'Delete connection',
    connectionTabs: 'Connection details',
    transfersCleared: 'Transfers cleared. Events, saved payloads, cache, and settings are kept.',
    eventsCleared: 'Events cleared. Transfers, statistics, saved payloads, cache, and settings are kept.',
  },
);

const DEFAULT_CONFIG: Config = {
  name: '',
  mode: 'serve',
  store: '',
  socket: null,
  tcp: '127.0.0.1:50051',
  upstream: 'http://127.0.0.1:50051',
  blob_dir: null,
  max_message_size: 536870912,
  log_level: 'swiftcache=info',
  local_cache_paths: [],
};

const normalize = (c: Config): Config => ({
  ...c,
  name: c.name.trim(),
  store: c.store.trim(),
  upstream: c.upstream.trim(),
  tcp: c.tcp === null ? null : c.tcp.trim(),
  socket: c.socket === null ? null : c.socket.trim(),
  blob_dir: c.blob_dir === null ? null : c.blob_dir.trim(),
  log_level: c.log_level.trim(),
  local_cache_paths: c.local_cache_paths.map((p) => p.trim()).filter(Boolean),
});

const SELECTED_KEY = 'swiftcache.selected';
const readSelected = (): string | null => {
  try {
    return localStorage.getItem(SELECTED_KEY);
  } catch {
    return null;
  }
};
const storeSelected = (id: string) => {
  try {
    localStorage.setItem(SELECTED_KEY, id);
  } catch {
    // Storage can be unavailable; the selection just won't persist.
  }
};

// Keep endpoint-derived labels for connections whose custom name is empty.
const serviceLabel = (c: Config) =>
  c.name.trim() || (c.mode === 'proxy' ? `→ ${c.upstream.replace(/^https?:\/\//, '')}` : (c.tcp ?? (c.socket ?? c.store).split('/').filter(Boolean).pop() ?? c.store));

function Uptime({ since }: { since: number }) {
  const [now, setNow] = useState(Date.now());
  const visible = useVisible();
  useEffect(() => {
    if (!visible) return;
    setNow(Date.now());
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [visible]);
  const t = useMessages(M);
  return <>{t.uptime(formatUptime((now - toMs(since)) / 1000))}</>;
}

function SuccessToast({ message, dismissLabel, onDismiss }: { message: string; dismissLabel: string; onDismiss: () => void }) {
  const [closing, setClosing] = useState(false);
  useEffect(() => {
    const fade = setTimeout(() => setClosing(true), 3750);
    const dismiss = setTimeout(onDismiss, 4000);
    return () => { clearTimeout(fade); clearTimeout(dismiss); };
  }, [onDismiss]);
  return (
    <div className={`success-toast${closing ? ' closing' : ''}`} role="status" aria-live="polite" aria-atomic="true">
      <span>{message}</span>
      <button type="button" className="icon-btn" aria-label={dismissLabel} onClick={onDismiss}>×</button>
    </div>
  );
}

export default function App() {
  const [prefs, setPrefs] = useState<Prefs>(defaultPrefs);
  const [prefsError, setPrefsError] = useState<string | null>(null);
  const [prefsOpen, setPrefsOpen] = useState(false);
  const closePrefs = useCallback(() => setPrefsOpen(false), []);
  const openPrefs = useCallback(() => setPrefsOpen(true), []);
  useTheme(prefs.theme);

  useEffect(() => {
    loadPrefs()
      .then((loaded) => {
        setPrefs(loaded);
        // Re-push so the menu bar picks up a changed system language.
        return savePrefs(loaded);
      })
      .catch((e) => setPrefsError(errorMessage(e)));
  }, []);

  // changePrefs can fire again before React re-renders, so merge into the latest prefs, not a render's snapshot.
  const latest = useRef(prefs);
  latest.current = prefs;
  const changePrefs = useCallback((patch: Partial<Prefs>) => {
    const next = { ...latest.current, ...patch };
    next.locale = resolveLocale(next.language);
    latest.current = next;
    setPrefs(next);
    savePrefs(next).then(
      () => setPrefsError(null),
      (e) => setPrefsError(errorMessage(e)),
    );
  }, []);

  // ⌘, (macOS app menu / menu bar item → "open-settings") and Ctrl+, elsewhere.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === ',') {
        e.preventDefault();
        setPrefsOpen(true);
      }
    };
    window.addEventListener('keydown', onKey);
    const unlisten = inTauri ? listen('open-settings', () => setPrefsOpen(true)) : null;
    return () => {
      window.removeEventListener('keydown', onKey);
      void unlisten?.then((stop) => stop());
    };
  }, []);

  return (
    <LocaleProvider locale={prefs.locale}>
      <Shell onOpenPrefs={openPrefs} />
      {prefsOpen && <Preferences prefs={prefs} error={prefsError} onChange={changePrefs} onClose={closePrefs} />}
    </LocaleProvider>
  );
}

function Shell({ onOpenPrefs }: { onOpenPrefs: () => void }) {
  const t = useMessages(M);
  const [page, setPage] = useState<PageId>('service');
  const [status, setStatus] = useState<Status | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);
  // Selection: `selectedId` is a saved service; `isNew` marks an unsaved draft (selectedId is then null).
  // The ref mirrors them for callbacks that run after an await.
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [isNew, setIsNew] = useState(false);
  const sel = useRef<{ id: string | null; isNew: boolean }>({ id: null, isNew: false });
  const [draft, setDraft] = useState<Config | null>(null);
  const draftRef = useRef<Config | null>(null);
  const changeDraft = useCallback((value: Config | null) => {
    draftRef.current = value;
    setDraft(value);
  }, []);
  // JSON of the selected service's server-side config; the draft is dirty when it differs.
  const base = useRef('');
  // Bumped around every mutation so a poll that started earlier (and may predate it) is dropped.
  const mutation = useRef(0);
  const [busy, setBusy] = useState(false);
  const acting = useRef(false);
  const [operation, setOperation] = useState<'saving' | 'starting' | 'stopping' | 'deleting' | null>(null);
  const [clearing, setClearing] = useState(false);
  const [notice, setNotice] = useState<{ message: string; kind: 'error' } | { message: string; kind: 'info'; id: number } | null>(null);
  const noticeSequence = useRef(0);
  const dismissNotice = useCallback(() => setNotice(null), []);
  const [pendingNavigation, setPendingNavigation] = useState<{ kind: 'new' } | { kind: 'select'; id: string } | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<ServiceStatus | null>(null);
  const [usage, setUsage] = useState<Usage | null>(null);
  const [usageError, setUsageError] = useState<string | null>(null);
  const [scanning, setScanning] = useState(false);
  const [transferFocus, setTransferFocus] = useState<{ id: number; nonce: number } | null>(null);
  const [eventFocus, setEventFocus] = useState<{ event: AppEvent; nonce: number } | null>(null);
  const [transfersEpoch, setTransfersEpoch] = useState(0);
  const [eventsEpoch, setEventsEpoch] = useState(0);

  const choose = useCallback((id: string | null, fresh: boolean) => {
    sel.current = { id, isNew: fresh };
    setSelectedId(id);
    setIsNew(fresh);
    if (id !== null) storeSelected(id);
  }, []);

  const load = useCallback(
    (svc: ServiceStatus) => {
      choose(svc.id, false);
      base.current = JSON.stringify(svc.config);
      changeDraft(svc.config);
    },
    [choose, changeDraft],
  );

  // Keep the selection valid and adopt the server config only while the user has no unsaved edits,
  // so polling never overwrites a draft.
  const reconcile = useCallback(
    (s: Status) => {
      const { id, isNew: fresh } = sel.current;
      if (fresh) return;
      const svc = id === null ? undefined : s.services.find((x) => x.id === id);
      if (!svc) {
        const stored = readSelected();
        const next = s.services.find((x) => x.id === stored) ?? s.services.find((x) => x.running) ?? s.services[0];
        if (next) {
          load(next);
        } else {
          choose(null, false);
          base.current = '';
          changeDraft(null);
        }
        return;
      }
      const incoming = JSON.stringify(svc.config);
      const cur = draftRef.current;
      const clean = cur === null ? null : JSON.stringify(cur);
      if (clean !== null && clean !== base.current) return;
      base.current = incoming;
      if (clean !== incoming) changeDraft(svc.config);
    },
    [choose, load, changeDraft],
  );

  const applyStatus = useCallback(
    (s: Status) => {
      setStatus((cur) => (cur === null ? s : keepIfSame(cur, s)));
      setStatusError(null);
      reconcile(s);
    },
    [reconcile],
  );

  const refreshStatus = useCallback(async () => {
    const gen = mutation.current;
    try {
      const s = await api<Status>('GET', '/api/status');
      if (gen === mutation.current) applyStatus(s);
    } catch (e) {
      if (gen === mutation.current) setStatusError(errorMessage(e));
    }
  }, [applyStatus]);

  usePoll(refreshStatus, 1500, true, [refreshStatus]);

  const usageId = isNew ? null : selectedId;
  const usageIdRef = useRef(usageId);
  usageIdRef.current = usageId;
  const scanSeq = useRef(0);

  // Another service's numbers must not linger while the new selection scans.
  useEffect(() => {
    scanSeq.current++;
    setScanning(false);
    setUsage(null);
    setUsageError(null);
  }, [usageId]);

  // `fresh` makes the server rescan instead of serving its cached usage; only the button asks for it.
  const scan = useCallback(
    async (fresh: boolean) => {
      if (usageId === null) return;
      const mine = ++scanSeq.current;
      setScanning(true);
      try {
        const u = await api<Usage>('GET', `/api/usage${query({ service: usageId, fresh: fresh ? 1 : undefined })}`);
        if (usageIdRef.current === usageId && mine === scanSeq.current) {
          setUsage(u);
          setUsageError(null);
        }
      } catch (e) {
        if (usageIdRef.current === usageId && mine === scanSeq.current) setUsageError(errorMessage(e));
      } finally {
        if (mine === scanSeq.current) setScanning(false);
      }
    },
    [usageId],
  );
  const rescan = useCallback(() => scan(false), [scan]);
  const rescanFresh = useCallback(() => void scan(true), [scan]);

  // Usage scans touch the disk: a scan per selection change plus a slow refresh only.
  usePoll(rescan, 30_000, status !== null && usageId !== null, [rescan, status === null]);

  const selected = status && !isNew ? (status.services.find((s) => s.id === selectedId) ?? null) : null;

  // Focus targets belong to one service; drop them when the selection moves.
  useEffect(() => {
    setTransferFocus(null);
    setEventFocus(null);
  }, [selectedId, isNew]);
  const edited = draft !== null && JSON.stringify(draft) !== base.current;
  const dirty = draft !== null && (isNew || edited);
  const actionsLocked = busy || clearing;
  const serviceLocked = actionsLocked || selected?.busy != null || statusError !== null;
  const running = selected?.running ?? false;
  const validDraft = draft !== null && status !== null && validate(draft, status.unix_socket_supported).length === 0;
  const canSave = dirty && validDraft && !running && !serviceLocked;

  useEffect(() => {
    if (!edited) return;
    const warn = (e: BeforeUnloadEvent) => { e.preventDefault(); e.returnValue = ''; };
    window.addEventListener('beforeunload', warn);
    return () => window.removeEventListener('beforeunload', warn);
  }, [edited]);

  const clearBusyChanged = useCallback((value: boolean) => {
    mutation.current++;
    scanSeq.current++;
    setScanning(false);
    setClearing(value);
  }, []);

  // Records were deleted: drop focus refs to them, refresh numbers, and remount the views that cache the other kind
  // (events keep transfer links; transfer inspectors and cache history list events).
  const historyCleared = (scope: 'transfers' | 'events') => {
    setTransferFocus(null);
    setEventFocus(null);
    if (scope === 'transfers') setTransfersEpoch((n) => n + 1);
    else setEventsEpoch((n) => n + 1);
    setNotice({ message: scope === 'transfers' ? t.transfersCleared : t.eventsCleared, kind: 'info', id: ++noticeSequence.current });
    void refreshStatus();
    rescanFresh();
  };

  // Returns the saved service's id, or null on failure.
  const save = async (): Promise<string | null> => {
    if (!draft) return null;
    try {
      const body = normalize(draft);
      const saved = isNew
        ? await api<ServiceStatus>('POST', '/api/services', body)
        : await api<ServiceStatus>('PUT', `/api/services/${selectedId}`, body);
      setStatus((cur) =>
        cur && {
          ...cur,
          services: cur.services.some((s) => s.id === saved.id)
            ? cur.services.map((s) => (s.id === saved.id ? saved : s))
            : [...cur.services, saved],
        },
      );
      load(saved);
      return saved.id;
    } catch (e) {
      setNotice({ message: t.saveFailed(errorMessage(e)), kind: 'error' });
      return null;
    }
  };

  const act = async (kind: NonNullable<typeof operation>, job: () => Promise<void>) => {
    if (acting.current || clearing) return;
    acting.current = true;
    setBusy(true);
    setOperation(kind);
    setNotice(null);
    mutation.current++;
    try {
      await job();
    } catch (e) {
      setNotice({ message: errorMessage(e), kind: 'error' });
    } finally {
      mutation.current++;
      acting.current = false;
      setBusy(false);
      setOperation(null);
      void refreshStatus();
    }
  };

  const saveOnly = () => act('saving', async () => {
    if (await save()) setNotice({ message: t.saved, kind: 'info', id: ++noticeSequence.current });
  });
  const start = () => act('starting', async () => {
    const id = dirty ? await save() : selectedId;
    if (id === null) return;
    applyStatus(await api<Status>('POST', `/api/services/${id}/start`));
    setNotice({ message: t.started, kind: 'info', id: ++noticeSequence.current });
  });
  const stop = () => act('stopping', async () => {
    if (selectedId === null) return;
    applyStatus(await api<Status>('POST', `/api/services/${selectedId}/stop`));
    setNotice({ message: t.stopped, kind: 'info', id: ++noticeSequence.current });
  });
  const remove = () => act('deleting', async () => {
    if (!deleteTarget) return;
    await api<unknown>('DELETE', `/api/services/${deleteTarget.id}`);
    setDeleteTarget(null);
    if (deleteTarget.id === selectedId) {
      base.current = '';
      changeDraft(null);
      choose(null, false);
    }
    applyStatus(await api<Status>('GET', '/api/status'));
    setNotice({ message: t.deleted, kind: 'info', id: ++noticeSequence.current });
  });

  const navigate = (next: NonNullable<typeof pendingNavigation>) => {
    setNotice(null);
    if (next.kind === 'select') {
      const svc = status?.services.find((s) => s.id === next.id);
      if (svc) load(svc);
    } else {
      choose(null, true);
      const uuid = crypto.randomUUID();
      const initialConfig = { ...DEFAULT_CONFIG, store: `/tmp/${uuid}`, socket: status?.unix_socket_supported ? `/tmp/${uuid}.sock` : null, local_cache_paths: [] };
      base.current = JSON.stringify(initialConfig);
      changeDraft(initialConfig);
      setPage('service');
    }
    setPendingNavigation(null);
  };
  const requestNavigation = (next: NonNullable<typeof pendingNavigation>) => {
    if (actionsLocked) return;
    if (edited) { setNotice(null); setPendingNavigation(next); }
    else navigate(next);
  };
  const select = (svc: ServiceStatus) => {
    if (svc.id !== selectedId || isNew) requestNavigation({ kind: 'select', id: svc.id });
  };
  const newConnection = () => requestNavigation({ kind: 'new' });
  const saveAndContinue = () => act('saving', async () => {
    if (pendingNavigation && await save()) navigate(pendingNavigation);
  });

  const goTransfer = (id: number) => {
    setTransferFocus({ id, nonce: Date.now() });
    setPage('transfers');
  };
  const goEvent = (event: AppEvent) => {
    setEventFocus({ event, nonce: Date.now() });
    setPage('events');
  };

  const saveShortcut = useRef<() => void>(() => {});
  saveShortcut.current = () => { if (canSave) void saveOnly(); };
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey || e.key.toLowerCase() !== 's') return;
      e.preventDefault();
      if (!e.repeat && !document.querySelector('dialog[open]')) saveShortcut.current();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  if (!status) {
    return (
      <div className="boot">
        <div className="card panel boot-card">
          <h2>SwiftCache</h2>
          {statusError ? (
            <>
              <Banner>{t.cannotConnect(statusError)}</Banner>
              <p className="muted small">{t.bootHint}</p>
            </>
          ) : (
            <p className="muted">{t.connecting}</p>
          )}
        </div>
      </div>
    );
  }

  const cfg: Config = (selected ? selected.config : draft) ?? DEFAULT_CONFIG;
  const hasView = isNew || selected !== null;
  const canStart = validDraft;
  const transferCount = selected?.summary.transfers ?? 0;
  const usageText = usage ? formatBytes(usage.total_bytes) : '—';
  const title = selected ? serviceLabel(selected.config) : t.newUnsaved;
  const stateLabel = (value: string) => value === 'starting' ? t.starting : value === 'stopping' ? t.stopping : value === 'clearing' ? t.clearing : t.working;
  const transition = clearing ? t.clearing : operation ? t[operation] : selected?.busy ? stateLabel(selected.busy) : null;
  const saveKey = navigator.platform.includes('Mac') ? '⌘S' : 'Ctrl+S';
  const tabs: { id: PageId; label: string; count?: number }[] = [
    { id: 'service', label: t.tabConfig },
    { id: 'transfers', label: t.tabTransfers, count: transferCount },
    { id: 'events', label: t.tabEvents },
    { id: 'cache', label: t.tabCache },
  ];

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <span className="logo">S</span>
          <span>SwiftCache</span>
        </div>
        <div className="side-head">
          <div className="side-label">{t.connections}</div>
          <button type="button" className="side-add" aria-label={t.addConnection} title={t.addConnection} disabled={actionsLocked || statusError !== null} onClick={newConnection}>
            +
          </button>
        </div>
        <div className="profiles">
          {status.services.length === 0 && !isNew && <p className="muted small profiles-empty">{t.noConnections}</p>}
          {status.services.map((s) => (
            <div className="profile-row" key={s.id}>
              <button type="button" className={s.id === selectedId && !isNew ? 'on' : ''} aria-current={s.id === selectedId && !isNew ? 'true' : undefined} disabled={actionsLocked} title={buildCli(s.config)} onClick={() => select(s)}>
                <span className={`dot ${s.running ? 'on' : ''}`} />
                <span className="profile-text">
                  <span className="ellipsis">{serviceLabel(s.config)}</span>
                  <span className="muted small">
                    {s.config.mode === 'serve' ? 'Server' : 'Proxy'} · {s.busy ? stateLabel(s.busy) : s.running ? t.stateRunning : t.stateStopped}
                    {s.error && <span className="danger-text"> · {t.stateError}</span>}
                  </span>
                </span>
              </button>
            </div>
          ))}
          {isNew && (
            <div className="profile-row">
              <button type="button" className="on" onClick={() => setPage('service')}>
                <span className="dot" />
                <span className="ellipsis">{t.newUnsaved}</span>
              </button>
            </div>
          )}
        </div>
        <div className="side-spacer" />
        <button type="button" className="side-settings" onClick={onOpenPrefs}>
          <span aria-hidden="true">⚙</span>
          {t.settings}
        </button>
      </aside>

      <main className="main">
        {statusError && <Banner>{t.disconnected(statusError)}</Banner>}
        {notice?.kind === 'error' && !pendingNavigation && !deleteTarget && (
          <Banner>
            <div className="row between">
              <span>{notice.message}</span>
              <button type="button" className="icon-btn" aria-label={t.dismiss} onClick={() => setNotice(null)}>×</button>
            </div>
          </Banner>
        )}
        {hasView ? (
          <>
            <div className="card svc-head">
              <span className={`dot ${running ? 'on' : ''}`} />
              <div className="svc-id">
                <div className="svc-title-row">
                  <strong className="svc-title ellipsis" title={title}>
                    {title}
                  </strong>
                  <span className={`pill ${running ? 'ok' : 'idle'}`}>{cfg.mode === 'serve' ? 'Server' : 'Proxy'}</span>
                  {dirty && <span className="pill idle">{t.unsaved}</span>}
                </div>
                <span className="muted small">
                  {transition ?? (running ? t.stateRunning : t.stateStopped)}
                  {running && selected?.started_at != null && (
                    <>
                      {' · '}
                      <Uptime since={selected.started_at} />
                    </>
                  )}
                </span>
              </div>
              <div className="svc-facts">
                <div className="svc-fact">
                  <span className="label">LISTENER</span>
                  <span className="mono ellipsis" title={listenerText(cfg)}>
                    {listenerText(cfg) || t.unset}
                  </span>
                </div>
                <div className="svc-fact">
                  <span className="label">STORE</span>
                  <span className="mono ellipsis" title={cfg.store}>
                    {cfg.store || t.unset}
                  </span>
                </div>
                {cfg.mode === 'proxy' && (
                  <div className="svc-fact">
                    <span className="label">UPSTREAM</span>
                    <span className="mono ellipsis" title={cfg.upstream}>
                      {cfg.upstream || t.unset}
                    </span>
                  </div>
                )}
                <div className="svc-fact svc-usage">
                  <span className="label">USAGE</span>
                  <span className="mono ellipsis">{usageText}</span>
                </div>
              </div>
              <div className="svc-actions">
                {selected && (
                  <button type="button" className="btn danger" disabled={serviceLocked || running} title={running ? t.removeLocked : t.removeConnection} onClick={() => { setNotice(null); setDeleteTarget(selected); }}>
                    {t.deleteConnection}
                  </button>
                )}
                <button type="button" className="btn" disabled={!canSave} title={`${t.saveConfig} (${saveKey})`} aria-keyshortcuts="Meta+S Control+S" onClick={saveOnly}>
                  {operation === 'saving' ? t.saving : t.saveConfig}
                </button>
                {running ? (
                  <button type="button" className="btn" disabled={serviceLocked} onClick={stop}>
                    {transition ?? t.stop}
                  </button>
                ) : (
                  <button type="button" className="btn primary" disabled={serviceLocked || !canStart} onClick={start}>
                    {transition ?? (dirty ? t.saveStart : t.start)}
                  </button>
                )}
              </div>
            </div>

            {selected?.error && <Banner>{t.serviceError(selected.error)}</Banner>}

            {!isNew && (
            <div className="tabs" role="tablist" aria-label={t.connectionTabs}>
              {tabs.map((tab) => (
                <button
                  type="button"
                  role="tab"
                  aria-selected={page === tab.id}
                  id={`tab-${tab.id}`}
                  aria-controls={`panel-${tab.id}`}
                  tabIndex={page === tab.id ? 0 : -1}
                  key={tab.id}
                  className={page === tab.id ? 'on' : ''}
                  onClick={() => setPage(tab.id)}
                  onKeyDown={(e) => {
                    const available = tabs;
                    const index = available.findIndex((x) => x.id === tab.id);
                    const next = e.key === 'Home' ? 0 : e.key === 'End' ? available.length - 1 : e.key === 'ArrowRight' ? (index + 1) % available.length : e.key === 'ArrowLeft' ? (index + available.length - 1) % available.length : -1;
                    if (next < 0) return;
                    e.preventDefault();
                    setPage(available[next].id);
                    document.getElementById(`tab-${available[next].id}`)?.focus();
                  }}
                >
                  {tab.label}
                  {tab.count ? <span className="count">{formatInt(tab.count)}</span> : null}
                </button>
              ))}
            </div>
            )}

            <div className="pages">
              <div id="panel-service" role={isNew ? undefined : 'tabpanel'} aria-labelledby={isNew ? undefined : 'tab-service'} hidden={page !== 'service'}>
                {draft && (
                  <ServicePage
                    key={selectedId ?? 'new'}
                    status={status}
                    running={running}
                    draft={draft}
                    setDraft={(value) => { changeDraft(value); setNotice(null); }}
                    dirty={dirty}
                    busy={serviceLocked}
                    onRevert={() => { if (selected) load(selected); setNotice(null); }}
                    isNew={isNew}
                  />
                )}
              </div>
              {selected && (
                <>
                  <div id="panel-transfers" role="tabpanel" aria-labelledby="tab-transfers" hidden={page !== 'transfers'}>
                    <TransfersPage
                      key={`${selected.id}:${eventsEpoch}`}
                      active={page === 'transfers'}
                      serviceId={selected.id}
                      serviceName={title}
                      summary={selected.summary}
                      focus={transferFocus}
                      locked={serviceLocked}
                      onOpenEvent={goEvent}
                      onClearBusy={clearBusyChanged}
                      onHistoryCleared={() => historyCleared('transfers')}
                    />
                  </div>
                  <div id="panel-events" role="tabpanel" aria-labelledby="tab-events" hidden={page !== 'events'}>
                    <EventsPage
                      key={`${selected.id}:${transfersEpoch}`}
                      active={page === 'events'}
                      serviceId={selected.id}
                      serviceName={title}
                      focus={eventFocus}
                      locked={serviceLocked}
                      onOpenTransfer={goTransfer}
                      onClearBusy={clearBusyChanged}
                      onHistoryCleared={() => historyCleared('events')}
                    />
                  </div>
                  <div id="panel-cache" role="tabpanel" aria-labelledby="tab-cache" hidden={page !== 'cache'}>
                    <CachePage
                      key={eventsEpoch}
                      active={page === 'cache'}
                      service={selected}
                      usage={usage}
                      usageError={usageError}
                      scanning={scanning}
                      onRescan={rescanFresh}
                      onCleared={setUsage}
                      onStatusChanged={refreshStatus}
                      onBusyChange={clearBusyChanged}
                    />
                  </div>
                </>
              )}
            </div>
          </>
        ) : (
          <section className="card panel">
            <div className="empty small">{t.emptyHint}</div>
            <div className="row">
              <button type="button" className="btn primary" disabled={actionsLocked || statusError !== null} onClick={newConnection}>
                {t.newConnection}
              </button>
            </div>
          </section>
        )}
      </main>
      {notice?.kind === 'info' && !pendingNavigation && !deleteTarget && (
        <SuccessToast key={notice.id} message={notice.message} dismissLabel={t.dismiss} onDismiss={dismissNotice} />
      )}
      {pendingNavigation && (
        <Modal title={t.leaveTitle} onClose={() => !busy && setPendingNavigation(null)}>
          <div className="modal-body">
            <h2>{t.leaveTitle}</h2>
            <p className="muted">{t.leaveBody}</p>
            {notice?.kind === 'error' && <Banner>{notice.message}</Banner>}
            <div className="row gap wrap end">
              <button type="button" className="btn" disabled={busy} onClick={() => setPendingNavigation(null)}>{t.cancel}</button>
              <button type="button" className="btn" disabled={busy} onClick={() => navigate(pendingNavigation)}>{t.discard}</button>
              <button type="button" className="btn primary" disabled={serviceLocked || !canStart} onClick={saveAndContinue}>{busy ? t.saving : t.saveContinue}</button>
            </div>
          </div>
        </Modal>
      )}
      {deleteTarget && (
        <Modal title={t.deleteTitle} onClose={() => !busy && setDeleteTarget(null)}>
          <div className="modal-body">
            <h2>{t.deleteTitle}</h2>
            <div className="idbox mono break">{serviceLabel(deleteTarget.config)} · {deleteTarget.config.store}{deleteTarget.config.mode === 'proxy' ? ` → ${deleteTarget.config.upstream}` : ''}</div>
            <p className="muted">{t.deleteBody}</p>
            {notice?.kind === 'error' && <Banner>{notice.message}</Banner>}
            <div className="row gap end">
              <button type="button" className="btn" disabled={busy} onClick={() => setDeleteTarget(null)}>{t.cancel}</button>
              <button type="button" className="btn danger solid" disabled={actionsLocked || statusError !== null || !status.services.some((s) => s.id === deleteTarget.id && !s.running && s.busy === null)} onClick={remove}>{busy ? t.deleting : t.confirmDelete}</button>
            </div>
          </div>
        </Modal>
      )}
    </div>
  );
}
