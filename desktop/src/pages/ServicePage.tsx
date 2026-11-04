import { useEffect, useRef, useState } from 'react';
import { defineMessages, pick, useMessages } from '../i18n';
import { Banner, Check, CopyButton } from '../components';
import { buildCli } from '../format';
import type { Config, Mode, Status } from '../types';

const MIB = 1024 * 1024;
// The server rejects < 1 MiB anyway (errMaxMessage), so KiB would only produce invalid values.
const SIZE_UNITS = [
  { label: 'MiB', bytes: MIB },
  { label: 'GiB', bytes: 1024 * MIB },
];
// Largest unit that shows the value as a whole number, e.g. 512 MiB, 2 GiB.
const fittingUnit = (bytes: number) => [...SIZE_UNITS].reverse().find((u) => bytes > 0 && bytes % u.bytes === 0) ?? SIZE_UNITS[0];
const LOG_LEVELS = ['swiftcache=info', 'swiftcache=debug', 'swiftcache=trace', 'swiftcache=warn', 'info', 'debug'];
const DEFAULT_TCP = '127.0.0.1:50051';

function splitTcp(address: string) {
  const separator = address.lastIndexOf(':');
  return separator < 0
    ? { host: address, port: '' }
    : { host: address.slice(0, separator), port: address.slice(separator + 1) };
}

function splitUpstream(address: string) {
  const match = /^(https?:\/\/)(\[[^\]]*\]|[^:/?#]*)(?::([^/?#]*))?(.*)$/i.exec(address);
  return match
    ? { prefix: match[1], host: match[2], port: match[3] ?? (match[1].toLowerCase() === 'https://' ? '443' : '80'), suffix: match[4] }
    : { prefix: 'http://', host: address, port: '50051', suffix: '' };
}

const M = defineMessages(
  {
    errStore: '請填寫快取目錄（--store）。',
    errNoListener: '至少啟用 TCP 或 Unix socket 其中一種接收方式。',
    errNoUnix: '此平台不支援 Unix socket，無法使用 Proxy 模式。',
    errUpstream: '請填寫上游位址（--upstream）。',
    errUpstreamPort: '上游 Port 必須是 1–65535 的整數。',
    errTcpEmpty: 'TCP 位址不可為空。',
    errTcpPort: 'TCP Port 必須是 0–65535 的整數。',
    errSocketEmpty: '請填寫本機 Unix socket 路徑（--socket）。',
    errBlobEmpty: '檔案回應目錄不可為空。',
    errMaxMessage: '單筆訊息上限至少 1 MiB。',
    unsupportedPlatform: (p: string) => `此平台（${p}）不支援 Unix socket：Serve 僅能使用 TCP，Proxy 模式需要 Unix socket，因此無法使用。`,
    modeServe: 'Server · 快取服務',
    modeProxy: 'Proxy · 本機中繼',
    lockedHint: '服務執行中；停止後才能修改設定。',
    unlockedHint: '設定儲存後，按啟動服務開始接收請求。',
    serveTitle: 'Server 連線與儲存',
    proxyTitle: 'Proxy 連線與本機快取',
    serveDesc: '本機工具鏈透過 socket 使用快取；其他機器的 Proxy 透過 TCP 連入。',
    proxyDesc: 'Proxy 在本機以 Unix socket 接收 client，命中時由本機快取回應，未命中與寫入則轉送到遠端 serve。',
    tcpScope: '監聽範圍',
    tcpLocal: '僅本機',
    tcpAll: '所有介面',
    tcpExisting: '現有位址',
    tcpPort: 'Port',
    localSocket: '本機 Unix socket · --socket',
    socketNoteServe: '將此 socket 路徑填入 Xcode／Swift 的快取設定；TCP 供其他機器連入。',
    socketNoteProxy: '必填：本機工具鏈連到這個 socket；Proxy 再透過 TCP 連到上游 Server。',
    socketOnly: '這筆既有 Server 只使用 Unix socket，原設定會保留；需要對外提供服務時可在進階設定加入 TCP。',
    addTcp: '加入 TCP 監聽',
    storeDir: '快取目錄 · --store / SWIFTCACHE_STORE',
    upstreamServe: '上游 Serve · --upstream / SWIFTCACHE_UPSTREAM',
    upstreamHost: '上游主機',
    upstreamCustom: '自訂',
    upstreamAddress: 'IP 或主機名稱',
    maxMessage: '單筆訊息上限 · --max-message-size',
    sizeUnit: '單位',
    logLevel: '文字 Log 層級 · SWIFTCACHE_LOG',
    enableBlobDir: '啟用檔案回應目錄 · --blob-dir',
    blobPlaceholder: '未設定；目前回傳 inline bytes',
    blobNoteServe: '只適用於 client 與 server 共用檔案系統；跨機器部署維持 bytes。',
    blobNoteProxy: '此目錄位於開發機，用於提供本機 client 可存取的檔案回應；上傳時本機 file_path 會被讀成 bytes 再傳到遠端。',
    localPaths: '本機建置快取路徑（選填，每行一個）',
    localPathsNote: '供「快取管理」清除時勾選；只有此處列出的路徑可被清除，不會刪除未宣告的目錄。',
    noHotReload: '不支援熱更新；套用設定時明確重啟。',
    uiLogTitle: '紀錄內容',
    perEvent: '每條事件',
    perEventBody: '訊息、層級、時間、來源',
    perTransfer: '每筆傳輸',
    perTransferBody: '完整請求 / 回應、結果與耗時',
    payloadBody: '保存原始 bytes，供預覽與匯出',
    recordsDir: '紀錄存放位置（唯讀）',
    recordsNote: '紀錄與快取目錄分開；清空快取時保留。文字 log 層級與 UI 詳細紀錄分別設定。',
    cliTitle: '同等 CLI 指令',
    copyCli: '複製 CLI 指令',
    cliNote: '反映上方表單內容；未啟用的 socket / blob 選項不產生 CLI 參數。UI 詳細紀錄另由 UI 管理。',
    remoteTitle: '遠端容量與清除',
    remoteBody: '現有 gRPC 僅提供 CAS / KV，不含管理 API，因此無法取得上游磁碟用量，也不提供遠端清除。快取清除作用於資料所在的主機；本機建置快取請另行於「快取管理」選擇。',
    advanced: '進階設定',
    advancedHint: '訊息上限、Log、檔案回應與本機清除路徑',
    revert: '還原變更',
    unsaved: '有未儲存的變更',
    busyHint: '服務正在處理操作，完成後可修改設定。',
    storePlaceholder: '/path/to/swiftcache/store',
    storeNoteServe: 'Serve 的所有 CAS / KV 資料都存放在此目錄。',
    storeNoteProxy: 'Proxy 在此保存本機讀取快取（必填）。寫入仍會先送到上游；清除快取不會影響上游。',
    connectionName: '連線名稱（選填）',
    nameNote: '留空會依連線位址自動顯示預設名稱。名稱可以重複。',
    connectionType: '連線類型',
    chooseType: '建立時選擇類型，儲存後即固定。',
    fixedType: '類型已固定；如需改用另一種類型，請新增連線。',
  },
  {
    errStore: 'Enter a cache directory (--store).',
    errNoListener: 'Enable at least one of TCP or Unix socket.',
    errNoUnix: 'This platform does not support Unix sockets, so Proxy mode is unavailable.',
    errUpstream: 'Enter an upstream address (--upstream).',
    errUpstreamPort: 'Upstream port must be an integer from 1 to 65535.',
    errTcpEmpty: 'The TCP address can’t be empty.',
    errTcpPort: 'TCP port must be an integer from 0 to 65535.',
    errSocketEmpty: 'Enter a local Unix socket path (--socket).',
    errBlobEmpty: 'The file response directory can’t be empty.',
    errMaxMessage: 'The maximum message size must be at least 1 MiB.',
    unsupportedPlatform: (p: string) => `This platform (${p}) does not support Unix sockets: Serve can only use TCP, and Proxy mode requires a Unix socket, so it is unavailable.`,
    modeServe: 'Server · Cache service',
    modeProxy: 'Proxy · Local relay',
    lockedHint: 'The service is running. Stop it to change settings.',
    unlockedHint: 'Save your settings, then start the service to accept requests.',
    serveTitle: 'Server connection and storage',
    proxyTitle: 'Proxy connection and local cache',
    serveDesc: 'Local toolchains use this cache through its socket; Proxies on other machines connect over TCP.',
    proxyDesc: 'Proxy accepts clients on a local Unix socket, serves hits from its local cache, and forwards misses and writes to a remote serve.',
    tcpScope: 'Listen scope',
    tcpLocal: 'Local only',
    tcpAll: 'All interfaces',
    tcpExisting: 'Existing address',
    tcpPort: 'Port',
    localSocket: 'Local Unix socket · --socket',
    socketNoteServe: 'Use this socket path in the Xcode / Swift cache settings. TCP accepts connections from other machines.',
    socketNoteProxy: 'Required: local toolchains connect to this socket; Proxy connects to the upstream Server over TCP.',
    socketOnly: 'This existing Server uses only a Unix socket. Its settings are preserved; add TCP under Advanced settings to accept network clients.',
    addTcp: 'Add TCP listener',
    storeDir: 'Cache directory · --store / SWIFTCACHE_STORE',
    upstreamServe: 'Upstream Serve · --upstream / SWIFTCACHE_UPSTREAM',
    upstreamHost: 'Upstream host',
    upstreamCustom: 'Custom',
    upstreamAddress: 'IP or hostname',
    maxMessage: 'Maximum message size · --max-message-size',
    sizeUnit: 'Unit',
    logLevel: 'Text log level · SWIFTCACHE_LOG',
    enableBlobDir: 'Enable file response directory · --blob-dir',
    blobPlaceholder: 'Not set; inline bytes are returned',
    blobNoteServe: 'Only applies when client and server share a file system; cross-machine setups keep using bytes.',
    blobNoteProxy: 'This directory is on the development machine and serves file responses that local clients can access; on upload, the local file_path is read into bytes and sent to the remote.',
    localPaths: 'Local build cache paths (optional, one per line)',
    localPathsNote: 'Offered as choices when clearing in Cache; only paths listed here can be cleared, and undeclared directories are never deleted.',
    noHotReload: 'Hot reload isn’t supported; settings are applied by an explicit restart.',
    uiLogTitle: 'Recorded data',
    perEvent: 'Each event',
    perEventBody: 'Message, level, time, source',
    perTransfer: 'Each transfer',
    perTransferBody: 'Full request / response, result, and duration',
    payloadBody: 'Raw bytes are kept for preview and export',
    recordsDir: 'Log storage location (read-only)',
    recordsNote: 'Logs are kept separate from the store and survive clearing the cache. The text log level and detailed UI logs are configured separately.',
    cliTitle: 'Equivalent CLI command',
    copyCli: 'Copy CLI command',
    cliNote: 'Reflects the form above; disabled socket / blob options produce no CLI arguments. Detailed UI logs are managed by the UI.',
    remoteTitle: 'Remote capacity and clearing',
    remoteBody: 'The current gRPC API only offers CAS / KV and no management API, so upstream disk usage is unavailable and remote clearing isn’t offered. Clearing acts on the host where the data lives; choose local build caches separately in Cache.',
    advanced: 'Advanced settings',
    advancedHint: 'Message limit, logs, file responses, and local cleanup paths',
    revert: 'Revert changes',
    unsaved: 'You have unsaved changes',
    busyHint: 'A service operation is in progress. Settings can be changed when it finishes.',
    storePlaceholder: '/path/to/swiftcache/store',
    storeNoteServe: 'Serve keeps all CAS / KV data in this directory.',
    storeNoteProxy: 'Proxy keeps a local read-through cache here (required). Writes still go to the upstream first; clearing it never touches the upstream.',
    connectionName: 'Connection name (optional)',
    nameNote: 'Leave blank to use a name based on the connection address. Names can be shared.',
    connectionType: 'Connection type',
    chooseType: 'Choose the type when creating this connection. Saving makes it permanent.',
    fixedType: 'The type is fixed. Create another connection to use a different type.',
  },
);

export function validate(c: Config, unixOk: boolean): string[] {
  const t = pick(M);
  const errs: string[] = [];
  if (!c.store.trim()) errs.push(t.errStore);
  if (c.mode === 'serve') {
    if (!c.tcp && !c.socket) errs.push(t.errNoListener);
  } else {
    if (!unixOk) errs.push(t.errNoUnix);
    const { host, port } = splitUpstream(c.upstream.trim());
    if (!host.trim()) errs.push(t.errUpstream);
    if (!/^\d+$/.test(port) || Number(port) < 1 || Number(port) > 65535) errs.push(t.errUpstreamPort);
  }
  if (c.mode === 'serve' && c.tcp !== null) {
    if (!c.tcp.trim()) errs.push(t.errTcpEmpty);
    else {
      const { port } = splitTcp(c.tcp.trim());
      if (!/^\d+$/.test(port) || Number(port) > 65535) errs.push(t.errTcpPort);
    }
  }
  if ((unixOk || c.mode === 'proxy') && (c.socket === null || !c.socket.trim())) errs.push(t.errSocketEmpty);
  if (c.blob_dir !== null && !c.blob_dir.trim()) errs.push(t.errBlobEmpty);
  if (!(c.max_message_size >= MIB)) errs.push(t.errMaxMessage);
  return errs;
}

export function ServicePage({
  status,
  running,
  draft,
  setDraft,
  dirty,
  busy,
  onRevert,
  isNew,
}: {
  status: Status;
  /** The selected service is running (its settings are locked). */
  running: boolean;
  draft: Config;
  setDraft: (c: Config) => void;
  dirty: boolean;
  busy: boolean;
  onRevert: () => void;
  isNew: boolean;
}) {
  const locked = running || busy;
  const unixOk = status.unix_socket_supported;
  const set = (patch: Partial<Config>) => setDraft({ ...draft, ...patch });
  const errs = validate(draft, unixOk);
  const cli = buildCli(draft);
  const serve = draft.mode === 'serve';
  const t = useMessages(M);
  const tcp = splitTcp(draft.tcp ?? DEFAULT_TCP);
  const customHost = tcp.host !== '127.0.0.1' && tcp.host !== '0.0.0.0' ? tcp.host : null;

  return (
    <div className={`service-grid${isNew ? ' creating' : ''}`}>
      <div className="stack gap-lg">
        {!unixOk && (
          <Banner kind="warn">
            {t.unsupportedPlatform(status.platform)}
          </Banner>
        )}
        <div className="card row between mode-bar">
          {isNew ? (
          <div className="seg big" role="group" aria-label={t.connectionType}>
            {(['serve', 'proxy'] as Mode[]).map((m) => (
              <button
                type="button"
                key={m}
                className={draft.mode === m ? 'on' : ''}
                aria-pressed={draft.mode === m}
                disabled={locked || (m === 'proxy' && !unixOk)}
                onClick={() => {
                  if (m !== draft.mode) set({ mode: m, tcp: m === 'serve' ? draft.tcp ?? DEFAULT_TCP : null });
                }}
              >
                {m === 'serve' ? t.modeServe : t.modeProxy}
              </button>
            ))}
          </div>
          ) : <strong>{serve ? t.modeServe : t.modeProxy}</strong>}
          <span className="muted small">{isNew ? t.chooseType : t.fixedType}</span>
        </div>
        {!isNew && <p className="muted small">{busy ? t.busyHint : running ? t.lockedHint : t.unlockedHint}</p>}

        <section className="card panel">
          <div>
            <h2>{serve ? t.serveTitle : t.proxyTitle}</h2>
            <p className="muted small">
              {serve && draft.tcp === null ? t.socketOnly : serve ? t.serveDesc : t.proxyDesc}
            </p>
          </div>
          <div className="field">
            <label className="label" htmlFor="service-name">{t.connectionName}</label>
            <input
              id="service-name"
              className="input"
              disabled={locked}
              value={draft.name}
              placeholder={serve ? draft.tcp || draft.socket || draft.store || 'Serve' : `→ ${draft.upstream.replace(/^https?:\/\//, '')}`}
              onChange={(e) => set({ name: e.target.value })}
            />
            <span className="muted small">{t.nameNote}</span>
          </div>

          <div className="field">
            <label className="label" htmlFor="service-store">{t.storeDir}</label>
            <input id="service-store" className="input mono" disabled={locked} value={draft.store} placeholder={t.storePlaceholder} onChange={(e) => set({ store: e.target.value })} />
            <span className="muted small">{serve ? t.storeNoteServe : t.storeNoteProxy}</span>
          </div>

          {(unixOk || !serve) && (
            <div className="field">
              <label className="label" htmlFor="service-socket">{t.localSocket}</label>
              <input
                id="service-socket"
                className="input mono"
                disabled={locked || !unixOk}
                value={draft.socket ?? ''}
                placeholder="/tmp/swiftcache/cache.sock"
                onChange={(e) => set({ socket: e.target.value })}
              />
              <span className="muted small">{serve ? t.socketNoteServe : t.socketNoteProxy}</span>
            </div>
          )}

          {serve && draft.tcp !== null && (
            <div className="field">
              <div className="tcp-fields">
                <fieldset className="tcp-scope" disabled={locked}>
                  <legend className="label">{t.tcpScope}</legend>
                  <div className="radio-options">
                    {[
                      { host: '127.0.0.1', label: t.tcpLocal },
                      { host: '0.0.0.0', label: t.tcpAll },
                      ...(customHost ? [{ host: customHost, label: t.tcpExisting }] : []),
                    ].map(({ host, label }) => (
                      <label className="radio-option" key={host}>
                        <input type="radio" name="tcp-scope" value={host} checked={tcp.host === host} onChange={() => set({ tcp: `${host}:${tcp.port}` })} />
                        <span>{label} <span className="mono">{host}</span></span>
                      </label>
                    ))}
                  </div>
                </fieldset>
                <div className="field">
                  <label className="label" htmlFor="service-tcp-port">{t.tcpPort}</label>
                  <input id="service-tcp-port" className="input mono" type="number" inputMode="numeric" min={0} max={65535} step={1} disabled={locked} value={tcp.port} onChange={(e) => set({ tcp: `${tcp.host}:${e.target.value}` })} />
                </div>
              </div>
            </div>
          )}


          {!serve && (
            <div className="field">
              <span className="label">{t.upstreamServe}</span>
              <UpstreamInput value={draft.upstream} disabled={locked} onChange={(upstream) => set({ upstream })} />
            </div>
          )}

          <details className="advanced-settings">
            <summary>{t.advanced}<span className="muted small">{t.advancedHint}</span></summary>
            <div className="stack">
              {serve && draft.tcp === null && (
                <div><button type="button" className="btn" disabled={locked} onClick={() => set({ tcp: DEFAULT_TCP })}>{t.addTcp}</button></div>
              )}
          <div className="grid2">
            <div className="field">
              <label className="label" htmlFor="service-message-size">{t.maxMessage}</label>
              <SizeInput bytes={draft.max_message_size} disabled={locked} unitLabel={t.sizeUnit} onChange={(max_message_size) => set({ max_message_size })} />
            </div>
            <div className="field">
              <label className="label" htmlFor="service-log">{t.logLevel}</label>
              <input
                id="service-log"
                className="input mono"
                list="log-levels"
                disabled={locked}
                value={draft.log_level}
                onChange={(e) => set({ log_level: e.target.value })}
              />
              <datalist id="log-levels">
                {LOG_LEVELS.map((l) => (
                  <option key={l} value={l} />
                ))}
              </datalist>
            </div>
          </div>

          <div className="field">
            <Check checked={draft.blob_dir !== null} disabled={locked} onChange={(v) => set({ blob_dir: v ? '/tmp/swiftcache/blobs' : null })}>
              {t.enableBlobDir}
            </Check>
            <input
              aria-label={t.enableBlobDir}
              className="input mono"
              disabled={locked || draft.blob_dir === null}
              value={draft.blob_dir ?? ''}
              placeholder={t.blobPlaceholder}
              onChange={(e) => set({ blob_dir: e.target.value })}
            />
            <span className="muted small">
              {serve ? t.blobNoteServe : t.blobNoteProxy}
            </span>
          </div>

          <div className="field">
            <label className="label" htmlFor="service-local-paths">{t.localPaths}</label>
            <textarea
              id="service-local-paths"
              className="input mono"
              rows={3}
              disabled={locked}
              value={draft.local_cache_paths.join('\n')}
              placeholder={'~/Library/Developer/Xcode/DerivedData\n/path/to/project/.build'}
              onChange={(e) => set({ local_cache_paths: e.target.value.split('\n') })}
            />
            <span className="muted small">{t.localPathsNote}</span>
          </div>
            </div>
          </details>

          {errs.length > 0 && (
            <Banner kind="warn">
              <ul className="plain">
                {errs.map((e) => (
                  <li key={e}>{e}</li>
                ))}
              </ul>
            </Banner>
          )}
          {dirty && (
            <div className="row gap wrap form-actions">
              <span className="muted small">{t.unsaved}</span>
              <span className="spacer" />
              {!isNew && <button type="button" className="btn" disabled={locked} onClick={onRevert}>{t.revert}</button>}
            </div>
          )}
          <p className="muted small">{t.noHotReload}</p>
        </section>
      </div>

      {!isNew && (
      <div className="stack gap-lg">
        <section className="card panel">
          <h2>{t.uiLogTitle}</h2>
          <dl className="kv">
            <dt>{t.perEvent}</dt>
            <dd>{t.perEventBody}</dd>
            <dt>{t.perTransfer}</dt>
            <dd>{t.perTransferBody}</dd>
            <dt>Payload</dt>
            <dd>{t.payloadBody}</dd>
          </dl>
          <div className="label">{t.recordsDir}</div>
          <div className="idbox mono">{status.records_dir}</div>
          <p className="muted small">{t.recordsNote}</p>
        </section>

        <section className="card panel">
          <div className="row between">
            <h2>{t.cliTitle}</h2>
            <CopyButton text={cli.replace(/ \\\n {2}/g, ' ')} label={t.copyCli} />
          </div>
          <pre className="json mono">{cli}</pre>
          <p className="muted small">{t.cliNote}</p>
        </section>

        {!serve && (
          <section className="card panel">
            <h2>{t.remoteTitle}</h2>
            <p className="muted small">
              {t.remoteBody}
            </p>
          </section>
        )}
      </div>
      )}
    </div>
  );
}

function UpstreamInput({ value, disabled, onChange }: { value: string; disabled: boolean; onChange: (value: string) => void }) {
  const t = useMessages(M);
  const { prefix, host, port, suffix } = splitUpstream(value);
  const [local, setLocal] = useState(host === '127.0.0.1');
  const [customHost, setCustomHost] = useState(local ? '' : host);
  const lastValue = useRef(value);
  // Follow saved/reverted values without changing the radio while a custom IP is being typed.
  useEffect(() => {
    if (value !== lastValue.current) {
      setLocal(host === '127.0.0.1');
      setCustomHost(host === '127.0.0.1' ? '' : host);
      lastValue.current = value;
    }
  }, [value, host]);
  const update = (nextHost: string, nextPort = port) => {
    const authority = nextHost.includes(':') && !nextHost.startsWith('[') ? `[${nextHost}]` : nextHost;
    const next = `${prefix}${authority}:${nextPort}${suffix}`;
    lastValue.current = next;
    onChange(next);
  };
  return (
    <div className="tcp-fields">
      <div className="field">
        <fieldset className="tcp-scope" disabled={disabled}>
          <legend className="label">{t.upstreamHost}</legend>
          <div className="radio-options">
            <label className="radio-option">
              <input type="radio" name="upstream-host" checked={local} onChange={() => { setLocal(true); update('127.0.0.1'); }} />
              <span>{t.tcpLocal} <span className="mono">127.0.0.1</span></span>
            </label>
            <label className="radio-option">
              <input type="radio" name="upstream-host" checked={!local} onChange={() => { setLocal(false); update(customHost); }} />
              <span>{t.upstreamCustom}</span>
            </label>
          </div>
        </fieldset>
        {!local && (
          <input id="service-upstream-host" className="input mono" aria-label={t.upstreamAddress} disabled={disabled} value={customHost} placeholder="192.168.1.10 / cache-host" onChange={(e) => { setCustomHost(e.target.value); update(e.target.value); }} />
        )}
      </div>
      <div className="field">
        <label className="label" htmlFor="service-upstream-port">{t.tcpPort}</label>
        <input id="service-upstream-port" className="input mono" type="number" inputMode="numeric" min={1} max={65535} step={1} disabled={disabled} value={port} onChange={(e) => update(local ? '127.0.0.1' : customHost, e.target.value)} />
      </div>
    </div>
  );
}

/** Byte count edited as a whole number plus a unit dropdown (e.g. 20 MiB). */
function SizeInput({ bytes, disabled, unitLabel, onChange }: { bytes: number; disabled: boolean; unitLabel: string; onChange: (bytes: number) => void }) {
  const [unit, setUnit] = useState(() => fittingUnit(bytes));
  // Follow external changes (another service selected) only when the value no longer fits the
  // chosen unit, so typing 1024 MiB never flips to 1 GiB mid-edit.
  useEffect(() => {
    if (bytes % unit.bytes !== 0) setUnit(fittingUnit(bytes));
  }, [bytes, unit]);
  // The typed text is kept locally so the field can be empty mid-edit; a controlled number
  // would snap an empty field to 0 and turn the next keystroke into "020".
  const [text, setText] = useState(() => String(bytes / unit.bytes));
  useEffect(() => {
    if (Number(text) * unit.bytes !== bytes) setText(String(bytes / unit.bytes));
  }, [bytes, unit, text]);
  const amount = Number(text) || 0;
  return (
    <div className="size-input">
      <input
        className="input mono"
        id="service-message-size"
        type="number"
        min={1}
        step={1}
        disabled={disabled}
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          onChange(Math.max(0, Math.round(Number(e.target.value) || 0)) * unit.bytes);
        }}
      />
      <select
        className="input select"
        aria-label={unitLabel}
        disabled={disabled}
        value={unit.label}
        onChange={(e) => {
          // Keep the number, change the magnitude: 20 MiB -> 20 GiB.
          const next = SIZE_UNITS.find((u) => u.label === e.target.value) ?? SIZE_UNITS[0];
          setUnit(next);
          onChange(amount * next.bytes);
        }}
      >
        {SIZE_UNITS.map((u) => (
          <option key={u.label} value={u.label}>
            {u.label}
          </option>
        ))}
      </select>
    </div>
  );
}
