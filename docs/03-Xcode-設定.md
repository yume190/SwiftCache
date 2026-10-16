# 03 — Xcode 設定

## 1. 最小可用設定

```bash
xcodebuild -project MyApp.xcodeproj -scheme MyApp \
    COMPILATION_CACHE_ENABLE_CACHING=YES \
    COMPILATION_CACHE_ENABLE_PLUGIN=YES \
    COMPILATION_CACHE_REMOTE_SERVICE_PATH=/tmp/swiftcache/cache.sock \
    COMPILATION_CACHE_ENABLE_DIAGNOSTIC_REMARKS=YES \
    build
```

三個關鍵開關：

1. `COMPILATION_CACHE_ENABLE_CACHING=YES` — 打開編譯快取。
2. `COMPILATION_CACHE_ENABLE_PLUGIN=YES` — **必要**。不開這個就會用內建 CAS，
   根本不會載入 plugin，`COMPILATION_CACHE_REMOTE_SERVICE_PATH` 會被忽略並發出警告：
   `… is set but COMPILATION_CACHE_ENABLE_PLUGIN is not enabled; the remote CAS service will not be used`
3. `COMPILATION_CACHE_REMOTE_SERVICE_PATH` — socket 路徑。

實務上會把它們放進 `.xcconfig` 或 CI 的 build settings，而不是每次打在命令列。

## 2. Build settings 全表

以下清單取自 Xcode 27 的 `SWBCore` 與 `swiftlang/swift-build` 的
`Sources/SWBCore/Settings/CASOptions.swift`。

| Setting | 型別 | 預設 | 說明 |
| --- | --- | --- | --- |
| `COMPILATION_CACHE_ENABLE_CACHING` | Bool | `$(COMPILATION_CACHE_ENABLE_CACHING_DEFAULT)` | 總開關；`SWIFT_ENABLE_COMPILE_CACHE` 與 `CLANG_ENABLE_COMPILE_CACHE` 都預設繼承它 |
| `SWIFT_ENABLE_COMPILE_CACHE` | Bool | 同上 | 只開 Swift |
| `CLANG_ENABLE_COMPILE_CACHE` | Bool | 同上 | 只開 C/ObjC |
| `COMPILATION_CACHE_ENABLE_PLUGIN` | Bool | NO | 用 plugin CAS 取代內建 CAS，**遠端快取的前提** |
| `COMPILATION_CACHE_PLUGIN_PATH` | String | `$(DEVELOPER_USR_DIR)/lib/libToolchainCASPlugin.dylib` | 換成自己的 plugin 時才需要改 |
| `COMPILATION_CACHE_REMOTE_SERVICE_PATH` | Path | 空 | 遠端服務的 Unix socket |
| `COMPILATION_CACHE_REMOTE_SUPPORTED_LANGUAGES` | StringList | 空（= 全部） | 限制哪些語言走遠端，例如只填 `swift` |
| `COMPILATION_CACHE_CAS_PATH` | Path | `$(DERIVED_DATA_DIR)/CompilationCache.noindex` | 本機 CAS 根目錄；plugin 實際用的是其下的 `plugin/` |
| `COMPILATION_CACHE_KEEP_CAS_DIRECTORY` | Bool | YES | 設 NO 則建置後丟棄本機 CAS |
| `COMPILATION_CACHE_LIMIT_SIZE` | String | 空 | 本機 CAS 上限，支援 `512M` / `20G` / `1T` / `0`（無限） |
| `COMPILATION_CACHE_LIMIT_PERCENT` | String | 空 | 以可用空間百分比設上限 |
| `COMPILATION_CACHE_ENABLE_DIAGNOSTIC_REMARKS` | Bool | NO | 印出 cache hit/miss 的 note，**除錯必開** |
| `COMPILATION_CACHE_ENABLE_STRICT_CAS_ERRORS` | Bool | — | CAS 錯誤直接讓建置失敗，而不是降級成普通編譯 |
| `COMPILATION_CACHE_ENABLE_DETACHED_KEY_QUERIES` | Bool | — | key 查詢不佔用建置的執行通道 |
| `COMPILATION_CACHE_VALIDATE_POST_BUILD` | Bool | — | 建置後驗證 CAS 內容 |

另外有兩個 Xcode user defaults（對整個 app 生效，優先於 build setting）：

```bash
defaults write com.apple.dt.Xcode CompilationCachingCASPath -string /path/to/cas
defaults write com.apple.dt.Xcode CompilationCachingDiskSizeLimit -string 20G
```

## 3. 怎麼確認真的有在用快取

打開 `COMPILATION_CACHE_ENABLE_DIAGNOSTIC_REMARKS=YES`，建置輸出會出現：

```
note: Local cache miss for key: 0~XwF91nDOCbb6ZFuKHvSKYDHKmin8ym49u4g-… (in target 'DemoTool' …)
note: Replay cache miss (in target 'DemoTool' …)
```

第二次（清掉 DerivedData 與本機 CAS）應該變成：

```
note: Replay cache hit (in target 'DemoTool' …)
Cache hit
```

同時 server 端會看到 `KV hit` / `CAS Load hit`。兩邊都看到才算真的走遠端。

## 4. 本 repo 的驗證專案

`examples/XcodeDemo/` 是一個三個 Swift 檔的 macOS command line tool。
完整的「寫入 → 清空 → 命中」流程：

```bash
SOCK=/tmp/swiftcache/cache.sock
CAS=/tmp/xcode-cas
DD=/tmp/xcdd

# 第一次：全部 miss，結果上傳到 server
rm -rf "$DD" "$CAS"
xcodebuild -project examples/XcodeDemo/DemoTool.xcodeproj -scheme DemoTool \
    -derivedDataPath "$DD" \
    COMPILATION_CACHE_ENABLE_CACHING=YES \
    COMPILATION_CACHE_ENABLE_PLUGIN=YES \
    COMPILATION_CACHE_REMOTE_SERVICE_PATH="$SOCK" \
    COMPILATION_CACHE_CAS_PATH="$CAS" \
    COMPILATION_CACHE_ENABLE_DIAGNOSTIC_REMARKS=YES build

# 第二次：連本機 CAS 一起清掉，只能從 server 拿
rm -rf "$DD" "$CAS"
xcodebuild … build | grep -c "Cache hit"
```

實測第二次輸出 `6`（3 個 Swift 檔 × emit-module/compile，全部命中）。

## 5. 注意事項

* **`COMPILATION_CACHE_CAS_PATH` 預設在 DerivedData 底下**，所以「清 DerivedData」
  同時也清掉本機 CAS。想衡量遠端命中率，這正是你要的；想要本機快取存活，就把它
  指到 DerivedData 之外。
* **socket 必須在建置開始前就存在**。plugin 連不上時會退化成本機快取，不會讓建置
  失敗（除非開了 `STRICT_CAS_ERRORS`），容易誤以為「有在用遠端」。
* **跨機器要讓 cache key 一致**：key 包含了絕對路徑。Xcode 提供 prefix mapping
  來消除這個差異，相關開關是 `SWIFT_ENABLE_PREFIX_MAPPING`、
  `CLANG_ENABLE_PREFIX_MAPPING`、`*_ENABLE_PROJECT_PREFIX_MAPPING`、
  `*_OTHER_PREFIX_MAPPINGS`；它們會把 SDK/Xcode/專案路徑映射成
  `/^sdk`、`/^xcode`、`/^src` 等符號前綴。CI 與開發機 Xcode 版本、SDK 版本也必須相同。
