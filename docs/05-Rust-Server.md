# 05 — Rust Server 實作

## 1. 編譯與執行

```bash
./scripts/bin/cargo build --release --manifest-path server/Cargo.toml
```

需要 `protoc`（`brew install protobuf`）。產物：`server/target/release/swiftcache`。

```
swiftcache [--records <dir>] serve  --store <dir> [--socket <path>] [--tcp <addr>]
                                    [--blob-dir <dir>] [--max-message-size <bytes>]
swiftcache [--records <dir>] proxy  --store <dir> --socket <path> --upstream <http://host:port>
                                    [--blob-dir <dir>] [--max-message-size <bytes>]
swiftcache daemon [--data-dir <dir>] [--listen 127.0.0.1:0]
```

`--records` 指定傳輸／事件紀錄目錄（預設為應用程式資料目錄下的 `records/`）。
`daemon` 是桌面程式用的本機管理程序，見第 8 節。

`SWIFTCACHE_LOG` 控制紀錄層級（預設 `swiftcache=info`，除錯用 `swiftcache=debug`
可以看到每一次 RPC）。

## 2. 兩種部署拓樸

### 單機 / CI runner 本機

```bash
swiftcache serve --store /var/lib/swiftcache --socket /tmp/swiftcache/cache.sock
```

編譯器直接連 socket，零網路開銷。適合「同一台機器上多個 workspace 或多個 CI job
共用快取」。

### 團隊共用（真・遠端）

plugin 只吃 Unix socket，所以跨機器需要本機中繼：

```bash
# 快取主機
swiftcache serve --store /var/lib/swiftcache --tcp 0.0.0.0:50051

# 每台開發機 / CI runner
swiftcache proxy --store /var/tmp/swiftcache-proxy --socket /tmp/swiftcache/cache.sock --upstream http://cache-host:50051
```

`proxy` 必填本機 `--store`（亦可用 `SWIFTCACHE_STORE`）。讀取先查本機 CAS／KV，未命中
才請求上游，並保存完整的成功回應。寫入先暫存資料，上游成功後才以它回傳的 opaque ID
原子發佈；上游錯誤不會形成可命中的本機項目，暫存檔也會移除。上游 ID 與物件 references
原樣保留。`Load` 的 blob-only 回應獨立保存，不會讓後續 `Get` 誤認 references 為空。

Proxy 使用延遲連線，因此上游離線時仍可啟動並提供本機命中；未命中與寫入失敗會正常回報。
跨機器傳輸前會把 `CASBytes.file_path` 轉成 bytes；回應可依本機 `--blob-dir` 落檔。

> 協定沒有認證與 TLS。要對外暴露請放在 mTLS reverse proxy 或 WireGuard /
> Tailscale 之後。

## 3. 程式結構

| 檔案 | 職責 |
| --- | --- |
| `src/main.rs` | CLI 參數、tracing 初始化 |
| `src/runtime.rs` | 服務生命週期（UDS / TCP 監聽、優雅停止）、session 統計，CLI 與 daemon 共用 |
| `src/journal.rs` | SQLite 紀錄（傳輸、事件、彙總）與去重後的 payload 檔 |
| `src/management.rs` | `swiftcache daemon` 的本機管理 API（設定、啟停、查詢、用量、清除） |
| `src/proto.rs` | `tonic::include_proto!` 產生的綁定 |
| `src/service.rs` | 以磁碟為後端的 `CASDBService` / `KeyValueDB` |
| `src/proxy.rs` | 本機讀取快取與同步寫入上游的 CAS / KV service |
| `src/store.rs` | 磁碟儲存與統計 |
| `src/blob.rs` | `CASBytes` 的讀寫（inline vs `file_path`） |
| `src/hash.rs` | CAS id 計算（LLVM builtin BLAKE3 相容） |
| `src/peer.rs` | 從連線取得呼叫端身分（UDS 取 pid、TCP 取位址） |

## 4. 儲存格式

```text
<store>/objects/<前2位hex>/<完整hex>   CAS 物件
<store>/kv/<前2位hex>/<完整hex>        action cache 項目（序列化的 kv.Value）
<store>/tmp/                           暫存，用 rename 原子發佈
<store>/objects/<前2位hex>/<完整hex>.blob  Proxy 的 blob-only 快取（僅供 Load）
```

CAS 物件的記錄格式（都是 little-endian）：

```text
magic      6 bytes  "SWCAS\x01"
ref_count  u32
  ref_len  u32  +  ref_bytes          × ref_count
data_len   u64
data       data_len bytes
```

兩張表都是 append-only 且內容定址，所以同一個 id 的並行寫入互相覆蓋也無害 ——
bytes 一樣。

## 5. CAS id 的選擇

`Put` / `Save` 的請求裡沒有 id，**server 自己決定**。本實作用
`llvm::cas::BuiltinObjectHasher<BLAKE3>` 的演算法：

```text
BLAKE3( le64(refs.len())
      || refs[0] || refs[1] || …      # 原始 digest bytes，不加長度前綴
      || le64(data.len())
      || data )
```

→ 32 bytes。

為什麼挑這個而不是隨便一種雜湊：

* **自洽**：同樣內容永遠得到同樣 id，天然去重。
* **長度固定**：client 會用 `Save("")` 的回傳長度來推斷本服務的 digest 長度。
* **與 LLVM 對齊**：若將來接上使用 LLVM builtin CAS 的 client，id 空間可直接互通。

實測 Xcode 27 的 plugin 自己用的是 `luxon.blake2b`（65 bytes），和我們的 32 bytes
不同也完全不影響 —— 協定層把 id 當不透明 bytes。

## 6. 設計決策

**gRPC 訊息上限 512 MiB。** 預設 4 MiB 會在第一個 `.swiftmodule` 就爆掉。

**miss 不是 error。** `OBJECT_NOT_FOUND` / `KEY_NOT_FOUND` 走 `outcome` 欄位；
gRPC error status 只留給真正的 I/O 失敗，避免在
`COMPILATION_CACHE_ENABLE_STRICT_CAS_ERRORS=YES` 下炸掉建置。

**`write_to_disk` 預設忽略。** proto 明確允許回傳 inline bytes。只有設定
`--blob-dir` 時才落檔，因為那要求 client 與 server 共用檔案系統。

**id 長度檢查放寬到 1..=80 bytes。** 這是 LLVM `GRPCRelayCAS` 的上限；比對長度只是
為了讓畸形請求變成乾淨的 not-found，而不是拿呼叫端給的字串去碰檔案系統。

**每一筆上傳／下載都有一行紀錄，而且帶來源。** 協定本身沒有任何 client 身分，
唯一可靠的來源資訊在傳輸層：unix socket 拿得到對端 pid（也就是某個
`swift-frontend` / `clang` 工作），TCP 拿得到對端位址（也就是某台開發機的 proxy）。

```
INFO pid:59234 up   cas save 53208ccb26a7… 4.0 KiB
INFO pid:59234 up   kv  put  deadbeef0102… entries=1
INFO pid:59387 down kv  get  deadbeef0102… hit entries=1
INFO pid:59387 down cas load 53208ccb26a7… 4.0 KiB
```

量很大（一次 1000 個編譯工作大約四千行），只要統計時用
`SWIFTCACHE_LOG=swiftcache=warn`。

**統計以「一次建置」為單位，不是累計值。** 協定裡沒有建置的概念，但建置的流量是
一團密集的 RPC 後面接著安靜，所以用「計數器 2 秒沒動」當作結束，每次只報這一段的
差值，和上一段無關：

```
INFO session #1 started
INFO session #1 done in 31.5s | kv: 0 hit / 1096 miss / 1764 put | cas: 0 hit / 0 miss / 4498 put | 692.6 MiB up, 0 B down | 1 clients: pid:62257
INFO session #2 started
INFO session #2 done in 7.1s | kv: 1764 hit / 0 miss / 0 put | cas: 2931 hit / 0 miss / 0 put | 0 B up, 435.4 MiB down | 1 clients: pid:66057
```

（實測：IceCubesApp 的 `StatusKit` scheme，1096 個編譯工作，第二次清空 DerivedData
與本機 CAS 後全部命中，31.5s → 7.1s。）

程序結束時再印一次 `total:`，那個才是累計。

## 7. 尚未實作

* **容量管理**：沒有 GC / LRU / quota。目前靠外部排程清理 `objects/` 與 `kv/`
  即可（兩張表都可以安全地整個刪掉，最多就是全部 miss 一次）。server 執行中刪也
  沒關係：寫入遇到目錄不見會重建並重試一次，讀取本來就把 ENOENT 當 miss。桌面程式
  只有手動清除（整個 `objects/`、`kv/`、`tmp/`）。
  **TODO**：每個 serve 服務各設容量上限（數字 + 單位，與單筆訊息上限同樣的輸入）。
  超過時不停服務，先依最後使用時間淘汰 KV 紀錄（LRU），再刪掉沒有任何 KV 引用的
  物件（GC），降到上限的 90%；淘汰順序要保證不會出現「KV 命中但物件不見」。
* **認證與 TLS**：見上文，請用外部方案包住。
* **壓縮**：blob 原樣落檔，方便除錯但比較吃空間。

## 8. 桌面管理程序（`swiftcache daemon`）

桌面程式不直接跑快取，而是啟動（或沿用）一個獨立的 `swiftcache daemon`：

* 只聽 loopback，每次啟動產生新的 bearer token，連線資訊寫在
  `<data-dir>/daemon.json`（權限 0600）。同一個資料目錄以檔案鎖確保只有一個 daemon。
  `daemon.json` 另帶管理 API 版本 `api`（來源 `server/api-revision`，daemon 與桌面程式
  編譯時共用）。桌面程式發現還活著的 daemon 版本不同（或沒有這個欄位）時，會送 SIGTERM
  讓它優雅停止服務、等 process 結束，再啟動自己附帶的新版，避免新介面接上舊 API。
  管理 API 有不相容的變更時要把這個數字加一。
* 可管理多個服務（serve / proxy 任意組合同時執行），設定存在 `<data-dir>/services.json`
  （每筆 `{id, config}`）；舊版的單一 `config.json` 會在第一次啟動時轉成一筆服務並刪除。
  `config.name` 為選填字串；缺少或留空時依連線位址產生顯示名稱。儲存時去除頭尾空白，
  自訂名稱會用於桌面介面、選單列與新事件的服務標籤；連線身分與紀錄關聯仍使用 `id`。
  管理 API revision 6 包含名稱欄位，舊設定載入時預設為空字串。
  管理 API：`GET /api/status`、`POST /api/services`、`PUT|DELETE /api/services/{id}`、
  `POST /api/services/{id}/start|stop`，用量與清除以 `?service={id}` / body `service` 指定。
  連線建立後 `config.mode` 不可修改，即使服務已停止也一樣；`PUT` 嘗試切換 `serve` /
  `proxy` 時回 409，原設定不變。使用另一種類型必須另建連線。管理 API revision 7
  包含此限制；現有連線保留目前的類型，其他設定仍依原有規則編輯。
  Server 與 Proxy 均必填 `store`，並共同使用本機用量掃描與清除流程。管理 API revision 9
  移除舊版 `Usage.remote_unavailable`；舊版空白 Proxy store 會遷移成資料目錄下各服務獨立的
  `cache/proxy-<id>`，並寫回設定。既有非空目錄與資料保留。Proxy 清除只刪本機 store，
  不會向上游發出清除請求。兩種模式的 blob、store 與紀錄目錄都必須分開。
  執行中的服務拒絕修改與移除；與另一個執行中服務共用 TCP 位址、Unix socket，或 store
  ／blob 目錄相同或互相包含時拒絕啟動。設定保留 `/tmp` 等寫法，內部以實體路徑比對。
  文字 log 層級是整個 process 共用，以最後啟動的服務為準。
* 啟動、停止、清除期間不鎖住整張服務表：該服務帶 `busy`（`starting` / `stopping` /
  `clearing`，`/api/status` 可見），對它的其他操作回 409「服務正在…中，請稍候」，其他
  服務與 `/api/status` 照常回應（例如 proxy 連不上 upstream 時不會卡住整個 UI）。
* `/api/usage` 的目錄掃描結果快取 15 秒，同時多個請求只掃一次；`?fresh=1` 強制重掃（UI
  的「重新掃描用量」按鈕）。清除後一定重掃。掃描只用 `stat`，不開檔。
* 每筆 RPC 在回應前寫進 `<data-dir>/records/journal.sqlite3`。寫入集中在單一 writer
  thread，把同時到的紀錄合併成一個 transaction（group commit），RPC 等到自己的紀錄
  commit 後才回應；紀錄寫入失敗時，原本成功的 RPC 會回 `INTERNAL`，不會出現「回了成功
  卻沒留下紀錄」，若已為它產生 `write_to_disk` 回應檔也會刪掉。log 行不等待 commit。
  UI 的查詢走另一條唯讀連線（WAL），不會擋住 RPC 紀錄。
* 原始 bytes 以 BLAKE3 去重：16 KiB 以下存在資料庫的 `payload_blobs`（跟紀錄同一個
  transaction），較大的存成 `records/payloads/` 檔案。已知 CAS 物件再次命中時沿用
  之前的 digest，不重新雜湊。
* 傳輸與事件都帶 `service_id`，標明是哪個服務產生的。服務內的 log 透過 tracing span
  歸屬，`GET /api/transfers|events?service_id={id}` 可以只看單一服務；`/api/status`
  的每個服務另附自己的 `summary`（流量、傳輸數、錯誤、KV 命中）。CLI 執行與 daemon
  自身的事件 `service_id` 為空。舊版紀錄庫開啟時會自動補上這個欄位，既有紀錄保留，
  但不屬於任何服務。
* 管理 API revision 8 新增 `POST /api/history/clear`，body 為
  `{ "service": "<id>", "scope": "transfers" }`，`scope` 也接受 `events`，成功回 `{}`。
  `transfers` 刪除該服務全部傳輸明細與 payload 關聯、重設該服務的 `summary`，並扣除
  全域統計的對應數值；保留事件，但將指向已刪除傳輸的 `transfer_id` 清空。
  `events` 只刪除該服務事件，保留傳輸明細與統計。不存在的服務回 404，未知 scope 回 400，
  服務另有操作進行中時回 409。刪除與紀錄寫入共用 writer transaction，服務不必停止，
  清除後提交的新紀錄照常保留；其他服務及未歸屬 CLI 紀錄不受影響。已保存的 payload
  bytes／檔案與 CAS / KV store 保留，以免影響正在處理的請求；此操作不會回收 payload 空間。
  GPUI 介面的請求另受 `desktop-gpui/src/api.rs` 的 `route_allowed` 限制；此路徑
  只允許 `POST`。新增 UI 使用的管理端點時，必須同步更新原生允許清單並驗證實際呼叫。
* 編譯快取清除流程：比對確認視窗顯示時的設定版本（`plan_id`），不符就拒絕；接著停止服務、
  等處理中的請求完成，刪除 `objects/`、`kv/`、`tmp/` 與勾選的已登記本機路徑，
  再依選項重啟。家目錄、資料目錄與紀錄目錄（及其上層）一律拒絕刪除。停止與清除在
  daemon 自己的 task 裡執行，UI 斷線也會跑完。
  介面在請求期間鎖住連線切換與其他服務操作；完成後先解除操作狀態，再重新取得 daemon
  狀態。計畫過期或清除失敗時重新取得計畫，讓使用者確認目前路徑後重試。
