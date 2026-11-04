import { useRef, useState } from 'react';
import { api } from './api';
import { Banner, Modal } from './components';
import { errorMessage } from './format';
import { defineMessages, useMessages } from './i18n';

export type HistoryScope = 'transfers' | 'events';

const M = defineMessages(
  {
    transfersAction: '清除傳輸明細',
    eventsAction: '清除事件紀錄',
    transfersTitle: '清除傳輸明細？',
    eventsTitle: '清除事件紀錄？',
    transfersBody: '這會刪除此連線的所有傳輸明細（不受目前篩選或時間範圍限制），並將上傳、下載、錯誤與 KV 命中率統計歸零。事件紀錄會保留，但指向已刪除傳輸的連結會被移除。',
    eventsBody: '這會刪除此連線的所有事件紀錄（不受目前篩選或時間範圍限制）。傳輸明細、流量統計與快取內容不受影響。',
    transfersKept: '已儲存的 payload、CAS / KV 快取與連線設定會保留。',
    eventsKept: '傳輸明細、統計、已儲存的 payload、快取與連線設定都會保留。',
    keptNote: '服務會持續執行，清除期間或之後產生的新紀錄仍會出現。已儲存的 payload 檔案不會被刪除。此操作無法復原。',
    cancelNote: '取消不會修改任何資料。',
    failed: (e: string) => `清除失敗：${e}`,
    cancel: '取消',
    clearing: '清除中…',
    retry: '重試清除',
  },
  {
    transfersAction: 'Clear transfers',
    eventsAction: 'Clear events',
    transfersTitle: 'Clear transfers?',
    eventsTitle: 'Clear events?',
    transfersBody: 'This deletes all transfer records of this connection (regardless of the current filters or time range) and resets its upload, download, error, and KV hit-rate statistics. Events are kept, but their links to the deleted transfers are removed.',
    eventsBody: 'This deletes all events of this connection (regardless of the current filters or time range). Transfers, traffic statistics, and cache contents are not affected.',
    transfersKept: 'Saved payloads, the CAS / KV cache, and connection settings are kept.',
    eventsKept: 'Transfers, statistics, saved payloads, the cache, and connection settings are kept.',
    keptNote: 'The service keeps running, and new records created during or after the clear still appear. Saved payload files are not deleted. This cannot be undone.',
    cancelNote: 'Canceling doesn’t change any data.',
    failed: (e: string) => `Clear failed: ${e}`,
    cancel: 'Cancel',
    clearing: 'Clearing…',
    retry: 'Retry clear',
  },
);

/**
 * Header button plus confirmation for clearing one service's transfer or event history.
 * `onBusyChange(false)` runs before `onCleared`, so the parent ends its mutation generation first.
 */
export function HistoryClear({
  scope,
  serviceId,
  serviceName,
  locked,
  onBusyChange,
  onCleared,
}: {
  scope: HistoryScope;
  serviceId: string;
  serviceName: string;
  /** Another operation (or a clear) is in flight, or the daemon is unreachable. */
  locked: boolean;
  onBusyChange: (busy: boolean) => void;
  onCleared: () => void;
}) {
  const t = useMessages(M);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const running = useRef(false);

  const close = () => {
    if (running.current) return;
    setOpen(false);
    setError(null);
  };

  const confirm = async () => {
    if (running.current) return;
    running.current = true;
    setBusy(true);
    setError(null);
    onBusyChange(true);
    let ok = false;
    try {
      await api<unknown>('POST', '/api/history/clear', { service: serviceId, scope });
      ok = true;
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      running.current = false;
      setBusy(false);
      onBusyChange(false);
    }
    if (ok) {
      setOpen(false);
      onCleared();
    }
  };

  const title = scope === 'transfers' ? t.transfersTitle : t.eventsTitle;
  const action = scope === 'transfers' ? t.transfersAction : t.eventsAction;
  return (
    <>
      <button type="button" className="btn small danger" disabled={locked || busy} onClick={() => { setError(null); setOpen(true); }}>
        {busy ? t.clearing : action}
      </button>
      {open && (
        <Modal title={title} onClose={close}>
          <div className="modal-body">
            <h2>{title}</h2>
            <div className="idbox mono break">{serviceName}</div>
            <p className="muted">{scope === 'transfers' ? t.transfersBody : t.eventsBody}</p>
            <p className="muted">{scope === 'transfers' ? t.transfersKept : t.eventsKept}</p>
            <p className="muted small">{t.keptNote} {t.cancelNote}</p>
            {error && <Banner>{t.failed(error)}</Banner>}
            <div className="row gap end">
              <button type="button" className="btn" disabled={busy} onClick={close}>{t.cancel}</button>
              <button type="button" className="btn danger solid" disabled={busy || locked} onClick={() => void confirm()}>
                {busy ? t.clearing : error ? t.retry : action}
              </button>
            </div>
          </div>
        </Modal>
      )}
    </>
  );
}
