# 04 — SwiftPM 設定

## 1. 結論先講

已發行的 SwiftPM（Swift 6.4 / Xcode 27.0）**沒有** compilation caching 的第一級旗標
（`swift build --help` 裡的 `--cache-path` / `--manifest-cache` 是講依賴與 manifest
快取，與編譯快取無關）。main 上已經有了，見 [3.2](#32-main-上已經補好了swift-65-dev)。

在 6.4 上要啟用只能把 swift-driver 的旗標用 `-Xswiftc` 傳進去，而且**必須搭配
`--build-system native`**：

```bash
PLUGIN=$(xcode-select -p)/usr/lib/libToolchainCASPlugin.dylib

swift build --build-system native \
  -Xswiftc -explicit-module-build \
  -Xswiftc -cache-compile-job \
  -Xswiftc -Rcache-compile-job \
  -Xswiftc -cas-plugin-path   -Xswiftc "$PLUGIN" \
  -Xswiftc -cas-plugin-option -Xswiftc "remote-service-path=/tmp/swiftcache/cache.sock" \
  -Xswiftc -cas-path          -Xswiftc /tmp/spm-cas
```

## 2. 旗標說明

| 旗標 | 作用 |
| --- | --- |
| `-explicit-module-build` | **必要**。沒有它 swift-driver 會警告 `-cache-compile-job cannot be used without explicit module build, turn off caching` 並靜默關閉快取 |
| `-cache-compile-job` | 啟用編譯快取 |
| `-cas-plugin-path <dylib>` | 使用 plugin CAS（對應 Xcode 的 `COMPILATION_CACHE_ENABLE_PLUGIN`） |
| `-cas-plugin-option remote-service-path=<socket>` | 指向遠端服務 |
| `-cas-path <dir>` | 本機 CAS 目錄（傳給 plugin 當 on-disk path） |
| `-Rcache-compile-job` | 印出 `remark: cache hit/miss for input file …`，除錯必開 |
| `-cache-disable-replay` | 只寫不讀，用來驗證上傳路徑 |

注意 `-Xswiftc` 每次只傳一個 token，所以帶值的旗標要寫兩次 `-Xswiftc`。

## 3. 預設的 `swiftbuild` 引擎：6.4 不行，main 已經可以

### 3.1 已發行的 Swift 6.4（Xcode 27.0）

SwiftPM 6.x 預設 `--build-system swiftbuild`（也就是 Xcode 的 swift-build 引擎）。
用 `-Xswiftc` 偷渡快取旗標時會失敗：

```
<unknown>:0: error: stat cache file '.../SDKStatCaches.noindex/macosx27.0-….sdkstatcache' not found
error: SwiftCompile normal arm64 failed with a nonzero exit code
```

原因不是檔案沒產生（它確實存在，實測 15 MB），而是：

* swift-build 在 `SwiftCompilerSpec` 裡的條件是
  `shouldUseSDKStatCache() && toolFeatures.has(.vfsstatcache) && !isCachingEnabled`
  —— 也就是**啟用快取時它本來就會拿掉 `-Xcc -ivfsstatcache`**。
* 但 `isCachingEnabled` 是由 build settings 決定的，我們用 `-Xswiftc` 傳進去的旗標
  swift-build 看不到，於是 `-ivfsstatcache` 還是被加上了。
* 開啟快取後 frontend 跑在 CAS 捕捉的虛擬檔案系統上，stat cache 檔不在那份快照裡，
  因此回報「not found」。

SwiftPM 沒有把 build settings 傳給 swift-build 的管道（只有 `-Xcc` / `-Xswiftc` /
`-Xlinker` / `-Xcxx` / `--toolset`）。環境變數也沒用，swift-build 的設定來自 PIF
而不是 environment —— 實測 `SDK_STAT_CACHE_ENABLE=NO swift build --build-system
swiftbuild …` 仍然是同一個錯誤。`swift build --help-hidden` 也沒有任何
compile-cache 開關。

所以在 6.4 上命令列只有兩條路：

1. `--build-system native`（會印 deprecation 警告，但快取功能正常）。
2. **直接用 `xcodebuild` 建一個純 SwiftPM 套件**——不需要 `.xcodeproj`，
   `xcodebuild` 認得 `Package.swift`，而且走的是有 build settings 的路徑，
   所以 Xcode 那套快取開關全部可用：

```bash
cd Packages/Models
xcodebuild -scheme Models -destination 'generic/platform=iOS Simulator' \
    -derivedDataPath /tmp/dd-pkg \
    COMPILATION_CACHE_ENABLE_CACHING=YES \
    COMPILATION_CACHE_ENABLE_PLUGIN=YES \
    COMPILATION_CACHE_REMOTE_SERVICE_PATH=/tmp/swiftcache/cache.sock \
    COMPILATION_CACHE_CAS_PATH=/tmp/cas-pkg \
    COMPILATION_CACHE_ENABLE_DIAGNOSTIC_REMARKS=YES build
```

實測 IceCubesApp 的 `Packages/Models`：第一次 240 `Cache miss`、0 CAS error，
清掉 DerivedData 與本機 CAS 後第二次 240 `Cache hit`（server 側 174.2 MiB 上傳 /
140.9 MiB 下載）。

### 3.2 main 上已經補好了（Swift 6.5-dev）

缺的那條管道在 2026-10-01 補上了（swift-package-manager `268dc7d1` “Introduce
support for Clang/Swift compilation caching”）：

* SwiftPM 多了 `BuildCacheConfiguration`，`constructBuildCacheSettingsOverrides()`
  把它轉成 `SWIFT_ENABLE_COMPILE_CACHE` / `COMPILATION_CACHE_ENABLE_PLUGIN` /
  `COMPILATION_CACHE_REMOTE_SERVICE_PATH` 等 build settings，塞進
  `SWBBuildParameters.overrides.synthesized`（`SwiftBuildSystem.swift:1128`）。
* swift-build 這邊用
  [`CompilationCachingInfo`](https://github.com/swiftlang/swift-build/blob/main/Sources/SWBCore/Settings/CompilationCachingInfo.swift)
  收集這組設定並往相依的 package target 傳遞（`BuildRequestContext` /
  `ProductPlan`），衝突會被記錄成 `Conflict`。
* 既然 `isCachingEnabled` 現在是真的，swift-build 就會自己拿掉
  `-Xcc -ivfsstatcache`，3.1 那個 stat cache 錯誤自然消失。

設定寫在套件的 `.swiftpm` 設定檔裡（或 `--global`），不是每次 build 的旗標：

```bash
swift package experimental-build-cache configure \
    --enable-caching \
    --path /tmp/spm-cas \
    --enable-diagnostic-remarks \
    --experimental-plugin-path "$(xcode-select -p)/usr/lib/libToolchainCASPlugin.dylib" \
    --experimental-remote-service-path /tmp/swiftcache/cache.sock

swift build          # 預設引擎，不需要 --build-system native，也不用 -Xswiftc
```

`swift package experimental-build-cache get-configuration` 可以回看目前設定，
`clean` / `info` 管本機 CAS。注意 prefix mapping 預設就是開的（跨機器共用的前提）。

用 2026-10-11 的 main snapshot（`swift-DEVELOPMENT-SNAPSHOT-2026-10-11-a`，Swift
6.5-dev）實測一個 6 檔案的套件，server 側：

```
session #1 done in 0.3s | kv: 0 hit / 9 miss / 9 put | cas: 0 hit / 0 miss / 26 put | 342.0 KiB up, 0 B down
session #2 done in 0.0s | kv: 9 hit / 0 miss / 0 put | cas: 20 hit / 0 miss / 0 put | 0 B up, 340.3 KiB down
```

第二次是在 `rm -rf .build <cas>` 之後，所以全部來自本 server，`swift build`
退出碼 0、沒有任何 stat cache 錯誤。該 commit 目前只在 `main`，`release/6.3` 與
已發行的 6.4 都沒有，子命令名稱在這個 snapshot 還是
`experimental-build-cache`（上游 10-08 已經把 `experimental-` 前綴 revert 掉）。

repo 裡對應的是 `make swiftbuild-verify`（拿 `examples/SwiftPMDemo` 跑兩次，
toolchain 路徑用 `SNAPSHOT=` 覆寫）：

```
=== swiftbuild 引擎第 1 次：預期全 miss，內容上傳到 server ===
build: 1 筆，耗時 3.1s
=== swiftbuild 引擎第 2 次：清掉本機 CAS，只能從 server 拿 ===
build: 1 筆，耗時 1.2s
```

server 側 `kv: 7 hit / 7 miss / 7 put | cas: 18 hit / 0 miss / 22 put |
348.7 KiB up, 348.1 KiB down`（兩次間隔短，併成同一個 session）。

## 4. 驗證流程

`examples/SwiftPMDemo/` 有一個兩 target 的套件。

```bash
cd examples/SwiftPMDemo
PLUGIN=$(xcode-select -p)/usr/lib/libToolchainCASPlugin.dylib
FLAGS=(--build-system native
  -Xswiftc -explicit-module-build
  -Xswiftc -cache-compile-job
  -Xswiftc -Rcache-compile-job
  -Xswiftc -cas-plugin-path   -Xswiftc "$PLUGIN"
  -Xswiftc -cas-plugin-option -Xswiftc "remote-service-path=/tmp/swiftcache/cache.sock"
  -Xswiftc -cas-path          -Xswiftc /tmp/spm-cas)

# 第一次：全 miss，上傳
rm -rf .build /tmp/spm-cas && swift build "${FLAGS[@]}"

# 第二次：清掉 .build 與本機 CAS，只能從 server 拿
rm -rf .build /tmp/spm-cas && swift build "${FLAGS[@]}" 2>&1 | grep "cache hit"
```

實測第二次全部命中：

```
remark: cache hit for input file '.../Sources/DemoLib/Statistics.swift': key '0~Eqlddq…'
remark: cache hit for input file '.../Sources/DemoLib/Shapes.swift':     key '0~VLxqDF…'
remark: cache hit for input file '.../Sources/DemoLib/Shapes.swift':     key '0~HdSvYS…'
remark: cache hit for input file '.../Sources/DemoApp/main.swift':       key '0~0fDM2T…'
remark: cache hit for input file '.../Sources/DemoApp/main.swift':       key '0~IaP73S…'
Build complete!
```

（同一個檔案出現兩個 key 是正常的：emit-module 與 compile 是兩個 frontend job。）

### 4.1 真實套件：`swiftlang/swift-build`

`make oss` 會把它 clone 到 `examples/swift-build`，`make spm-verify` 用它跑兩次
（它是純 SwiftPM 套件、host macOS 直接編得過，不必像 IceCubesApp 的 iOS-only 套件
那樣硬指定 `-target`）：

```
=== swift build 第 1 次：預期 cache miss，內容上傳到 server ===
cache miss: 1172 筆，耗時 55.8s
=== swift build 第 2 次：清掉本機 CAS，只能從 server 拿 ===
cache hit: 7336 筆，耗時 179.7s
```

server 側：

```
session #1 done in 44.4s | kv: 0 hit / 583 miss / 1376 put | cas: 0 hit / 0 miss / 3862 put | 438.4 MiB up, 0 B down | 583 clients
session #2 done in 15.1s | kv: 1357 hit / 0 miss / 0 put | cas: 2627 hit / 0 miss / 0 put | 0 B up, 326.5 MiB down | 566 clients
```

**命中率 100%，但牆鐘時間反而從 55.8s 變成 179.7s。** 原因在 client 側：swift-driver
是每個編譯工作一個 process，單次建置就有 583 個不同的 client pid，每個都要重新連
socket、把 326 MiB 的產物搬回本機。這個量級的套件「重放」比「重編」貴。

對照組是 Xcode：單一 process 重用連線，`StatusKit` 的 1096 個工作是 59.0s → 23.8s。
所以遠端快取的效益取決於**建置系統怎麼跑編譯器**，不只是命中率。

## 5. 直接用 swiftc

不經過 SwiftPM 時旗標完全一樣：

```bash
swiftc -c Foo.swift \
  -explicit-module-build -cache-compile-job -Rcache-compile-job \
  -cas-plugin-path "$PLUGIN" \
  -cas-plugin-option remote-service-path=/tmp/swiftcache/cache.sock \
  -cas-path /tmp/my-cas
```
