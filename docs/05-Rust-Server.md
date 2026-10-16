# 05 — Rust Server 實作

## 1. 編譯與執行

```bash
cargo build --release --manifest-path server/Cargo.toml
```

需要 `protoc`（`brew install protobuf`）。產物：`server/target/release/swiftcache`。

```
swiftcache serve  --store <dir> [--socket <path>] [--tcp <addr>]
                  [--blob-dir <dir>] [--max-message-size <bytes>]
swiftcache proxy  --socket <path> --upstream <http://host:port>
                  [--blob-dir <dir>] [--max-message-size <bytes>]
```

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
swiftcache proxy --socket /tmp/swiftcache/cache.sock --upstream http://cache-host:50051
```

`proxy` 子命令只做兩件事：轉發 RPC，以及把 `CASBytes.file_path` 換成內嵌 bytes
（檔案路徑無法跨機器）。反方向則把上游回傳的 bytes 視需要落到本機 `--blob-dir`。

> 協定沒有認證與 TLS。要對外暴露請放在 mTLS reverse proxy 或 WireGuard /
> Tailscale 之後。

## 3. 程式結構

| 檔案 | 職責 |
| --- | --- |
| `src/main.rs` | CLI、監聽器（UDS / TCP）、關機處理、session 統計 |
| `src/proto.rs` | `tonic::include_proto!` 產生的綁定 |
| `src/service.rs` | 以磁碟為後端的 `CASDBService` / `KeyValueDB` |
| `src/proxy.rs` | 轉發版的同兩個 service |
| `src/store.rs` | 磁碟儲存與統計 |
| `src/blob.rs` | `CASBytes` 的讀寫（inline vs `file_path`） |
| `src/hash.rs` | CAS id 計算（LLVM builtin BLAKE3 相容） |
| `src/peer.rs` | 從連線取得呼叫端身分（UDS 取 pid、TCP 取位址） |

## 4. 儲存格式

```text
<store>/objects/<前2位hex>/<完整hex>   CAS 物件
<store>/kv/<前2位hex>/<完整hex>        action cache 項目（序列化的 kv.Value）
<store>/tmp/                           暫存，用 rename 原子發佈
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
  沒關係：寫入遇到目錄不見會重建並重試一次，讀取本來就把 ENOENT 當 miss。
* **認證與 TLS**：見上文，請用外部方案包住。
* **壓縮**：blob 原樣落檔，方便除錯但比較吃空間。
