import { useEffect, useRef, useState, type ReactNode } from 'react';
import { api, query, saveExport } from './api';
import { errorMessage, formatBytes, formatInt } from './format';
import { defineMessages, useMessages } from './i18n';
import type { PayloadChunk, PayloadRef } from './types';

const M = defineMessages(
  {
    copy: '複製',
    copied: '已複製',
    copyFailed: '複製失敗',
    shown: (from: string, to: string, total: string) => `顯示 ${from}–${to} / 共 ${total} 筆`,
    latest: '第一頁',
    prevPage: '上一頁',
    nextPage: '下一頁',
    pageOf: (page: number, pages: number) => `第 ${page} / ${pages} 頁`,
    noPayload: '這筆傳輸沒有 payload（未命中、錯誤或僅有 metadata）。',
    exporting: '匯出中…',
    exportPayload: '匯出此 payload',
    loadingPayload: '載入 payload…',
    byteRange: (from: string, to: string, total: string) => `${from}–${to} B / 完整 ${total} B`,
    prevChunk: '上一段',
    nextChunk: '下一段',
    hex: '十六進位',
    text: '文字',
    digest: (id: string) => `摘要 ${id}`,
    retry: '重試',
  },
  {
    copy: 'Copy',
    copied: 'Copied',
    copyFailed: 'Copy failed',
    shown: (from: string, to: string, total: string) => `Showing ${from}–${to} of ${total}`,
    latest: 'First page',
    prevPage: 'Previous',
    nextPage: 'Next',
    pageOf: (page: number, pages: number) => `Page ${page} of ${pages}`,
    noPayload: 'This transfer has no payload (miss, error, or metadata only).',
    exporting: 'Exporting…',
    exportPayload: 'Export this payload',
    loadingPayload: 'Loading payload…',
    byteRange: (from: string, to: string, total: string) => `${from}–${to} B of ${total} B`,
    prevChunk: 'Previous chunk',
    nextChunk: 'Next chunk',
    hex: 'Hex',
    text: 'Text',
    digest: (id: string) => `digest ${id}`,
    retry: 'Retry',
  },
);

export function Banner({ kind = 'error', children }: { kind?: 'error' | 'warn' | 'info'; children: ReactNode }) {
  return (
    <div className={`banner ${kind}`} role={kind === 'error' ? 'alert' : 'status'}>
      {children}
    </div>
  );
}

export function Check({
  checked,
  onChange,
  disabled,
  children,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  children: ReactNode;
}) {
  return (
    <label className={`check ${disabled ? 'disabled' : ''}`}>
      <input type="checkbox" checked={checked} disabled={disabled} onChange={(e) => onChange(e.target.checked)} />
      <span className="box" aria-hidden="true">
        ✓
      </span>
      <span>{children}</span>
    </label>
  );
}

export function CopyButton({ text, label }: { text: string; label?: string }) {
  const t = useMessages(M);
  const [state, setState] = useState<'idle' | 'ok' | 'fail'>('idle');
  useEffect(() => {
    if (state === 'idle') return;
    const timer = setTimeout(() => setState('idle'), 1600);
    return () => clearTimeout(timer);
  }, [state]);
  return (
    <button
      type="button"
      className="btn small"
      onClick={() => {
        navigator.clipboard.writeText(text).then(
          () => setState('ok'),
          () => setState('fail'),
        );
      }}
    >
      {state === 'ok' ? t.copied : state === 'fail' ? t.copyFailed : (label ?? t.copy)}
    </button>
  );
}

export function Pager({
  offset,
  limit,
  total,
  onChange,
  disabled = false,
}: {
  offset: number;
  limit: number;
  total: number;
  onChange: (offset: number) => void;
  disabled?: boolean;
}) {
  const t = useMessages(M);
  const from = total === 0 ? 0 : offset + 1;
  const to = Math.min(total, offset + limit);
  return (
    <div className="pager">
      <span className="muted">{t.shown(formatInt(from), formatInt(to), formatInt(total))}</span>
      <div className="row gap">
        <button type="button" className="btn small" disabled={disabled || offset === 0} onClick={() => onChange(0)}>
          {t.latest}
        </button>
        <button type="button" className="btn small" disabled={disabled || offset === 0} onClick={() => onChange(Math.max(0, offset - limit))}>
          {t.prevPage}
        </button>
        <span className="muted">{t.pageOf(Math.floor(offset / limit) + 1, Math.max(1, Math.ceil(total / limit)))}</span>
        <button type="button" className="btn small" disabled={disabled || offset + limit >= total} onClick={() => onChange(offset + limit)}>
          {t.nextPage}
        </button>
      </div>
    </div>
  );
}

export function JsonBlock({ value }: { value: unknown }) {
  return <pre className="json mono">{JSON.stringify(value, null, 2)}</pre>;
}

export function Modal({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const element = dialog.current;
    const previous = document.activeElement;
    element?.showModal();
    return () => {
      element?.close();
      if (previous instanceof HTMLElement && previous.isConnected) previous.focus();
    };
  }, []);
  return (
    <dialog
      ref={dialog}
      className="modal"
      aria-label={title}
      onKeyDown={(e) => {
        if (e.key !== 'Tab') return;
        const controls = [...e.currentTarget.querySelectorAll<HTMLElement>('button, input, select, textarea, a[href], [tabindex]')]
          .filter((el) => !el.matches(':disabled, [tabindex="-1"]') && el.getClientRects().length > 0);
        const first = controls[0];
        const last = controls.at(-1);
        if (!first) { e.preventDefault(); e.currentTarget.focus(); }
        else if (e.shiftKey && (document.activeElement === first || document.activeElement === e.currentTarget)) { e.preventDefault(); last?.focus(); }
        else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
      }}
      onCancel={(e) => { e.preventDefault(); onClose(); }}
      onClick={(e) => {
        if (e.target !== e.currentTarget) return;
        const box = e.currentTarget.getBoundingClientRect();
        if (e.clientX < box.left || e.clientX > box.right || e.clientY < box.top || e.clientY > box.bottom) onClose();
      }}
    >
      {children}
    </dialog>
  );
}

const byteAt = (hex: string, i: number) => parseInt(hex.slice(i * 2, i * 2 + 2), 16);

export function HexDump({ hex, offset }: { hex: string; offset: number }) {
  const n = Math.floor(hex.length / 2);
  const lines: string[] = [];
  for (let i = 0; i < n; i += 16) {
    const end = Math.min(n, i + 16);
    let bytes = '';
    let ascii = '';
    for (let j = i; j < i + 16; j++) {
      if (j < end) {
        const b = byteAt(hex, j);
        bytes += b.toString(16).padStart(2, '0') + ' ';
        ascii += b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : '.';
      } else bytes += '   ';
      if (j === i + 7) bytes += ' ';
    }
    lines.push(`${(offset + i).toString(16).padStart(6, '0')}  ${bytes} | ${ascii}`);
  }
  return <pre className="hex mono">{lines.join('\n')}</pre>;
}

const CHUNK = 4096;
const EXPORT_CHUNK = 65536;

export function PayloadViewer({ payloads }: { payloads: PayloadRef[] }) {
  const t = useMessages(M);
  const [index, setIndex] = useState(0);
  const [offset, setOffset] = useState(0);
  const [mode, setMode] = useState<'hex' | 'text'>('hex');
  const [chunk, setChunk] = useState<PayloadChunk | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [exporting, setExporting] = useState(false);
  const [retryKey, setRetryKey] = useState(0);
  const current = payloads[index];

  useEffect(() => {
    if (!current) return;
    let live = true;
    setChunk(null);
    setError(null);
    api<PayloadChunk>('GET', `/api/payload/${encodeURIComponent(current.id)}${query({ offset, limit: CHUNK })}`).then(
      (c) => live && setChunk(c),
      (e) => live && setError(errorMessage(e)),
    );
    return () => {
      live = false;
    };
  }, [current, offset, retryKey]);

  if (payloads.length === 0) return <div className="empty small">{t.noPayload}</div>;

  const exportPayload = async () => {
    setExporting(true);
    setError(null);
    try {
      let hex = '';
      for (let off = 0; off < current.bytes; off += EXPORT_CHUNK) {
        const c = await api<PayloadChunk>('GET', `/api/payload/${encodeURIComponent(current.id)}${query({ offset: off, limit: EXPORT_CHUNK })}`);
        if (c.hex.length === 0) break;
        hex += c.hex;
      }
      await saveExport(`payload-${current.id.slice(0, 16)}.json`, JSON.stringify({ id: current.id, label: current.label, bytes: current.bytes, hex }, null, 2));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setExporting(false);
    }
  };

  const total = chunk?.total ?? current.bytes;
  const shown = chunk ? Math.floor(chunk.hex.length / 2) : 0;
  return (
    <div className="stack">
      <div className="payload-list">
        {payloads.map((p, i) => (
          <button
            type="button"
            key={`${p.id}-${i}`}
            className={`payload-item ${i === index ? 'active' : ''}`}
            onClick={() => {
              setIndex(i);
              setOffset(0);
            }}
          >
            <span className="mono ellipsis">{p.label}</span>
            <span className="muted">{formatBytes(p.bytes)}</span>
          </button>
        ))}
      </div>
      <div className="row between">
        <div className="seg">
          <button type="button" className={mode === 'hex' ? 'on' : ''} aria-pressed={mode === 'hex'} onClick={() => setMode('hex')}>
            {t.hex}
          </button>
          <button type="button" className={mode === 'text' ? 'on' : ''} aria-pressed={mode === 'text'} onClick={() => setMode('text')}>
            {t.text}
          </button>
        </div>
        <button type="button" className="btn small" disabled={exporting} onClick={exportPayload}>
          {exporting ? t.exporting : t.exportPayload}
        </button>
      </div>
      <div className="muted small mono ellipsis" title={current.id}>
        {t.digest(current.id)}
      </div>
      {error && <Banner>{error} <button type="button" className="link" onClick={() => setRetryKey((key) => key + 1)}>{t.retry}</button></Banner>}
      {!chunk && !error && <div className="empty small">{t.loadingPayload}</div>}
      {chunk && (mode === 'hex' ? <HexDump hex={chunk.hex} offset={chunk.offset} /> : <pre className="hex mono">{chunk.text}</pre>)}
      <div className="row between">
        <span className="muted small">{t.byteRange(formatInt(chunk?.offset ?? offset), formatInt((chunk?.offset ?? offset) + shown), formatInt(total))}</span>
        <div className="row gap">
          <button type="button" className="btn small" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - CHUNK))}>
            {t.prevChunk}
          </button>
          <button type="button" className="btn small" disabled={offset + CHUNK >= total} onClick={() => setOffset(offset + CHUNK)}>
            {t.nextChunk}
          </button>
        </div>
      </div>
    </div>
  );
}
