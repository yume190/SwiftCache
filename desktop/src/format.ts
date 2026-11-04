import type { Config } from './types';

const UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];

export function bytesParts(n: number): [string, string] {
  if (!Number.isFinite(n) || n < 0) return ['—', ''];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024;
    i++;
  }
  if (i === 0) return [String(v), 'B'];
  return [v.toFixed(v >= 100 ? 0 : v >= 10 ? 1 : 2), UNITS[i]];
}

export function formatBytes(n: number): string {
  const [v, u] = bytesParts(n);
  return u ? `${v} ${u}` : v;
}

export function formatInt(n: number): string {
  return n.toLocaleString('en-US');
}

const pad = (n: number, w = 2) => String(n).padStart(w, '0');

export function formatClock(ms: number): string {
  const d = new Date(ms);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
}

export function formatDate(ms: number): string {
  const d = new Date(ms);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

export function formatDateTime(ms: number): string {
  const d = new Date(ms);
  const off = -d.getTimezoneOffset();
  const sign = off >= 0 ? '+' : '-';
  const tz = `${sign}${pad(Math.floor(Math.abs(off) / 60))}:${pad(Math.abs(off) % 60)}`;
  return `${formatDate(ms)} ${formatClock(ms)} ${tz}`;
}

export function formatShortDateTime(ms: number): string {
  const d = new Date(ms);
  return `${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

export function formatDuration(us: number): string {
  if (us < 1000) return `${us} µs`;
  if (us < 1_000_000) return `${(us / 1000).toFixed(1)} ms`;
  return `${(us / 1_000_000).toFixed(2)} s`;
}

export function formatUptime(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  return `${pad(Math.floor(s / 3600))}:${pad(Math.floor((s % 3600) / 60))}:${pad(s % 60)}`;
}

// started_at is documented as unix-ms; tolerate seconds from older daemons.
export function toMs(t: number): number {
  return t < 1e11 ? t * 1000 : t;
}

export function shellQuote(s: string): string {
  if (s !== '' && /^[A-Za-z0-9_/.:=@%+,-]+$/.test(s)) return s;
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

// Equivalent `swiftcache serve|proxy` invocation for a saved Config, one flag per line.
export function buildCli(c: Config): string {
  const env = c.log_level ? `SWIFTCACHE_LOG=${shellQuote(c.log_level)} ` : '';
  const lines = [`${env}swiftcache ${c.mode}`];
  lines.push(`--store ${shellQuote(c.store)}`);
  if (c.tcp && c.mode === 'serve') lines.push(`--tcp ${shellQuote(c.tcp)}`);
  if (c.socket) lines.push(`--socket ${shellQuote(c.socket)}`);
  if (c.mode === 'proxy') lines.push(`--upstream ${shellQuote(c.upstream)}`);
  if (c.blob_dir) lines.push(`--blob-dir ${shellQuote(c.blob_dir)}`);
  lines.push(`--max-message-size ${c.max_message_size}`);
  return lines.join(' \\\n  ');
}

export function listenerText(c: Config): string {
  const parts: string[] = [];
  if (c.tcp && c.mode === 'serve') parts.push(c.tcp);
  if (c.socket) parts.push(c.socket);
  return parts.join(' · ');
}

export function errorMessage(e: unknown): string {
  if (e instanceof Error) return e.message;
  if (typeof e === 'string') return e;
  if (e && typeof e === 'object' && 'error' in e) return String(e.error);
  return JSON.stringify(e);
}
