# swiftcache — 快取伺服器與真實專案的驗證流程
#
#   make server       前景啟動 server（Ctrl-C 停止，socket 會自動清掉）
#   make ui           打包桌面 UI（debug .app）並開啟
#   make ui-dev       桌面 UI 開發模式（前端熱更新）
#   make ui-stop      關掉桌面 UI 與它的 daemon（.app 與 ui-dev 皆是；會停掉執行中的服務）
#   make oss          clone 驗證用專案到 examples/（IceCubesApp、swift-build）
#   make icecubes     備妥 IceCubesApp：clone + xcconfig + SDK patch + resolve
#   make spm          swift build（examples/swift-build）走 server
#   make spm-verify   跑兩次 swift build，第二次必須全是 cache hit
#   make pkg          xcodebuild 直接建 SwiftPM 套件（唯一受支援的設定管道）
#   make pkg-verify   跑兩次 pkg，第二次必須全是 Cache hit
#   make xcode        xcodebuild（IceCubesApp scheme）走 server
#   make xcode-verify 跑兩次 xcodebuild，第二次必須全是 Cache hit
#   make app          建 IceCubesApp 的 app target（模擬器）
#   make run          建完直接裝進模擬器並啟動
#   make swiftbuild        swift build 走預設 swiftbuild 引擎（需要 main snapshot）
#   make swiftbuild-verify 跑兩次 swiftbuild，第二次必須全是 cache hit
#   make toolchain         印出 6.5-dev snapshot 的路徑、bundle id 與切換方式
#   make status       看 socket 與 store 現況
#   make clean-cache  清空 server 的 store 與本機 CAS

CARGO_MANIFEST := server/Cargo.toml
SWIFTCACHE     := server/target/release/swiftcache

STORE := $(CURDIR)/cache/store
SOCK  := $(CURDIR)/cache/cache.sock

# 驗證用的兩個 OSS 專案，都 clone 到 examples/：
#   IceCubesApp — Xcode 專案 + 13 個 local package，走 xcodebuild
#   swift-build — 純 SwiftPM 套件、約 1200 個編譯工作，走 swift build
OSS_REPO := https://github.com/Dimillian/IceCubesApp.git
OSS_DIR  := $(CURDIR)/examples
OSS      := $(OSS_DIR)/IceCubesApp
SWB_REPO := https://github.com/swiftlang/swift-build.git
SWB      := $(OSS_DIR)/swift-build

PLUGIN := $(shell xcode-select -p)/usr/lib/libToolchainCASPlugin.dylib

# ---- 桌面 UI（Tauri）--------------------------------------------------------
UI_DIR := $(CURDIR)/desktop
UI_APP := $(UI_DIR)/src-tauri/target/debug/bundle/macos/SwiftCache.app
# 打包進 .app 的兩個執行檔，以及 ui-dev 跑的同名執行檔；pkill 用路徑比對，
# 不會誤殺 make server。
UI_SHELL  := SwiftCache.app/Contents/MacOS/swiftcache-desktop|src-tauri/target/debug/swiftcache-desktop|^target/debug/swiftcache-desktop
UI_DAEMON := SwiftCache.app/Contents/MacOS/swiftcache daemon|src-tauri/target/debug/swiftcache daemon

# ---- SwiftPM ---------------------------------------------------------------
# 用 swift-build 自己當素材：它是純 SwiftPM 套件、host macOS 直接編得過，
# 不需要像 IceCubesApp 的 iOS-only 套件那樣硬指定 -target。
SPM_PKG := $(SWB)
SPM_CAS := /tmp/spm-cas

# -Xswiftc 一次只吃一個 token，帶值的旗標要寫兩次。
# 少了 -explicit-module-build，swift-driver 會警告後靜默關閉快取。
SPMFLAGS := --build-system native \
	-Xswiftc -explicit-module-build \
	-Xswiftc -cache-compile-job \
	-Xswiftc -Rcache-compile-job \
	-Xswiftc -cas-plugin-path   -Xswiftc $(PLUGIN) \
	-Xswiftc -cas-plugin-option -Xswiftc remote-service-path=$(SOCK) \
	-Xswiftc -cas-path          -Xswiftc $(SPM_CAS)

# ---- SwiftPM × 預設 swiftbuild 引擎（Swift 6.5-dev 以後）--------------------
# 已發行的 6.4 不行：swift-build 只有在 build settings 說 caching on 時才會拿掉
# -ivfsstatcache，而 6.4 的 SwiftPM 沒有傳 build settings 的管道。
# main 補上了 `swift package experimental-build-cache`，設定走套件的 .swiftpm。
SNAPSHOT    := $(HOME)/Library/Developer/Toolchains/swift-DEVELOPMENT-SNAPSHOT-2026-10-11-a.xctoolchain
SB_SWIFT    := $(SNAPSHOT)/usr/bin/swift
# xcodebuild / xcrun 認的是 bundle id，不是資料夾名。
SNAPSHOT_ID  = $(shell /usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' $(SNAPSHOT)/Info.plist 2>/dev/null)
# 預設拿 swift-build 當素材，量體跟 xcode-verify 同級才有可比性；
# 想要快一點就 SB_PKG=$(CURDIR)/examples/SwiftPMDemo。
SB_PKG      := $(SWB)
SB_CAS      := /tmp/swiftbuild-cas

# ---- Xcode -----------------------------------------------------------------
PROJECT := $(OSS)/IceCubesApp.xcodeproj
# 量測用 StatusKit：它會連帶編 Models/NetworkClient/Env/DesignSystem，
# 1096 個編譯工作，而且不牽涉簽章與 app bundle。
SCHEME  := StatusKit
DEST    := generic/platform=iOS Simulator
DD      := /tmp/dd-ice
CAS     := /tmp/cas-ice

# 真的要跑起來的是 app scheme，裝進模擬器需要具體機型（不能用 generic）。
APP_SCHEME := IceCubesApp
APP_BUNDLE := Ice Cubes.app
SIM        := iPhone 17 Pro
APP_DD     := /tmp/dd-app
APP_CAS    := /tmp/cas-app

# 少了 ENABLE_PLUGIN 就不會載入 CAS plugin，REMOTE_SERVICE_PATH 會被無聲忽略。
XCFLAGS := \
	CODE_SIGNING_ALLOWED=NO \
	COMPILATION_CACHE_ENABLE_CACHING=YES \
	COMPILATION_CACHE_ENABLE_PLUGIN=YES \
	COMPILATION_CACHE_REMOTE_SERVICE_PATH=$(SOCK) \
	COMPILATION_CACHE_CAS_PATH=$(CAS) \
	COMPILATION_CACHE_ENABLE_DIAGNOSTIC_REMARKS=YES

# 同一組設定，但建的是純 SwiftPM 套件目錄（不需要 .xcodeproj）。
# `swift build --build-system swiftbuild` 不能用快取：swift-build 只有在
# build settings 說「caching on」時才會拿掉 -ivfsstatcache，而 SwiftPM 沒有
# 傳 build settings 的管道，-Xswiftc 偷渡的旗標它看不到。
PKG_PKG    := $(OSS)/Packages/Models
PKG_SCHEME := Models
PKG_DD     := /tmp/dd-pkg
PKG_CAS    := /tmp/cas-pkg
PKGFLAGS   := \
	CODE_SIGNING_ALLOWED=NO \
	COMPILATION_CACHE_ENABLE_CACHING=YES \
	COMPILATION_CACHE_ENABLE_PLUGIN=YES \
	COMPILATION_CACHE_REMOTE_SERVICE_PATH=$(SOCK) \
	COMPILATION_CACHE_CAS_PATH=$(PKG_CAS) \
	COMPILATION_CACHE_ENABLE_DIAGNOSTIC_REMARKS=YES

.PHONY: build server ui ui-dev ui-stop oss icecubes spm spm-verify pkg pkg-verify xcode xcode-verify \
        app run \
        swiftbuild swiftbuild-verify toolchain verify status clean-cache clean \
        guard-socket guard-snapshot

build: $(SWIFTCACHE)

$(SWIFTCACHE): $(wildcard server/src/*.rs) $(CARGO_MANIFEST)
	cargo build --release --manifest-path $(CARGO_MANIFEST)

server: build
	$(SWIFTCACHE) serve --store $(STORE) --socket $(SOCK)

# ---- 桌面 UI ---------------------------------------------------------------
# beforeBuildCommand / beforeDevCommand 會先 prepare:server（編 server 並放進
# binaries/），所以這裡不用另外 make build。
$(UI_DIR)/node_modules: $(UI_DIR)/package.json
	cd $(UI_DIR) && bun install
	@touch $@

# 先關掉舊的視窗程序，不然 open 只會把舊版叫到前景。daemon 不動，執行中的服務
# 照常；若還活著的 daemon 管理 API 版本（server/api-revision）與新版不同，桌面
# 程式啟動時會自動停掉它（含其中執行中的服務）並換成新版。ui-dev 同理。
ui: $(UI_DIR)/node_modules
	cd $(UI_DIR) && bun run tauri build --debug --bundles app
	@pkill -f 'SwiftCache.app/Contents/MacOS/swiftcache-desktop' || true
	@while pgrep -f 'SwiftCache.app/Contents/MacOS/swiftcache-desktop' >/dev/null; do sleep 0.2; done
	open '$(UI_APP)'

ui-dev: $(UI_DIR)/node_modules
	cd $(UI_DIR) && bun run tauri dev

ui-stop:
	@pkill -f '$(UI_SHELL)' || true
	@pkill -f '$(UI_DAEMON)' || true
	@while pgrep -f '$(UI_SHELL)|$(UI_DAEMON)' >/dev/null; do sleep 0.2; done
	@echo "桌面 UI 與 daemon 已停止"

oss: $(OSS) $(SWB)

$(OSS):
	mkdir -p $(OSS_DIR)
	git clone --depth 1 $(OSS_REPO) $(OSS)
	cp -n $(OSS)/IceCubesApp.xcconfig.template $(OSS)/IceCubesApp.xcconfig
	# app target 用了 iOS 27.1 才有的 .axisBehavior 與 \.toolbarVerticalEdge，
	# Xcode 27.0 的 SDK 編不過（與快取無關）。裝了 27.1 以後這個 patch 會失敗，
	# 直接略過即可。
	git -C $(OSS) apply $(CURDIR)/patches/icecubes-xcode27.0.patch \
		|| echo "注意：icecubes-xcode27.0.patch 沒套用（SDK 可能已經是 27.1）"

$(SWB):
	mkdir -p $(OSS_DIR)
	git clone --depth 1 $(SWB_REPO) $(SWB)

# 把 IceCubesApp 準備到可以直接建置／開 Xcode 的狀態：
# clone → 產生 xcconfig → 套 SDK patch → 先解析套件相依
# （resolve 另外做，才不會把下載時間混進 build 的量測）。
icecubes: $(OSS)
	xcodebuild -project $(PROJECT) -resolvePackageDependencies
	@echo "IceCubesApp 就緒：$(OSS)"
	@echo "  make xcode / xcode-verify  量測用（$(SCHEME) scheme）"
	@echo "  make app / run             建 app target、裝進模擬器"
	@echo "  open $(PROJECT)            用 Xcode 開"

# ---- SwiftPM ---------------------------------------------------------------
# 不清 .build 與本機 CAS 就不會重編，server 自然看不到任何 RPC。
spm: oss guard-socket
	rm -rf $(SPM_PKG)/.build 
# 	$(SPM_CAS)
	cd $(SPM_PKG) && swift build $(SPMFLAGS) 2>&1 1>/dev/null

# $(call timed,<標籤>,<指令>,<要數的 pattern>)
# 跑一次建置、數命中行數、印牆鐘時間。macOS 的 date 沒有 %N，所以用 perl 取秒。
define timed
t0=$$(perl -MTime::HiRes=time -e 'printf "%.3f", time'); \
n=$$($(2) 2>&1 | grep -cE '$(3)' || true); \
perl -MTime::HiRes=time -e 'printf "%s: %s 筆，耗時 %.1fs\n", $$ARGV[0], $$ARGV[1], time - $$ARGV[2]' "$(1)" "$$n" "$$t0"
endef

spm-verify: oss guard-socket
	@echo "=== swift build 第 1 次：預期 cache miss，內容上傳到 server ==="
	@rm -rf $(SPM_PKG)/.build $(SPM_CAS)
	@cd $(SPM_PKG) && $(call timed,cache miss,swift build $(SPMFLAGS),cache miss)
	@echo "=== swift build 第 2 次：清掉本機 CAS，只能從 server 拿 ==="
	@rm -rf $(SPM_PKG)/.build $(SPM_CAS)
	@cd $(SPM_PKG) && $(call timed,cache hit,swift build $(SPMFLAGS),cache hit)

# ---- SwiftPM 套件 × xcodebuild ---------------------------------------------
pkg: oss guard-socket
	rm -rf $(PKG_DD) $(PKG_CAS)
	cd $(PKG_PKG) && xcodebuild -scheme $(PKG_SCHEME) \
		-destination '$(DEST)' -derivedDataPath $(PKG_DD) $(PKGFLAGS) build

pkg-verify: oss guard-socket
	@echo "=== 套件 xcodebuild 第 1 次：預期 Cache miss ==="
	@rm -rf $(PKG_DD) $(PKG_CAS)
	@cd $(PKG_PKG) && $(call timed,Cache miss,xcodebuild -scheme $(PKG_SCHEME) -destination '$(DEST)' -derivedDataPath $(PKG_DD) $(PKGFLAGS) build,^Cache miss)
	@echo "=== 套件 xcodebuild 第 2 次：清掉本機 CAS，只能從 server 拿 ==="
	@rm -rf $(PKG_DD) $(PKG_CAS)
	@cd $(PKG_PKG) && $(call timed,Cache hit,xcodebuild -scheme $(PKG_SCHEME) -destination '$(DEST)' -derivedDataPath $(PKG_DD) $(PKGFLAGS) build,^Cache hit)

# ---- Xcode -----------------------------------------------------------------
xcode: oss guard-socket
	rm -rf $(DD) $(CAS)
	xcodebuild -project $(PROJECT) -scheme $(SCHEME) \
		-destination '$(DEST)' -derivedDataPath $(DD) $(XCFLAGS) build

xcode-verify: oss guard-socket
	@echo "=== xcodebuild 第 1 次：預期 Cache miss，內容上傳到 server ==="
	@rm -rf $(DD) $(CAS)
	@$(call timed,Cache miss,xcodebuild -project $(PROJECT) -scheme $(SCHEME) -destination '$(DEST)' -derivedDataPath $(DD) $(XCFLAGS) build,^Cache miss)
	@echo "=== xcodebuild 第 2 次：清掉本機 CAS，只能從 server 拿 ==="
	@rm -rf $(DD) $(CAS)
	@$(call timed,Cache hit,xcodebuild -project $(PROJECT) -scheme $(SCHEME) -destination '$(DEST)' -derivedDataPath $(DD) $(XCFLAGS) build,^Cache hit)

# 把 IceCubesApp 真的跑起來（模擬器）。沿用同一組快取設定，所以 app 本身的
# 編譯也會走 server。
app: oss guard-socket
	xcodebuild -project $(PROJECT) -scheme $(APP_SCHEME) \
		-destination 'platform=iOS Simulator,name=$(SIM)' \
		-derivedDataPath $(APP_DD) \
		CODE_SIGNING_ALLOWED=NO \
		COMPILATION_CACHE_ENABLE_CACHING=YES \
		COMPILATION_CACHE_ENABLE_PLUGIN=YES \
		COMPILATION_CACHE_REMOTE_SERVICE_PATH=$(SOCK) \
		COMPILATION_CACHE_CAS_PATH=$(APP_CAS) \
		COMPILATION_CACHE_ENABLE_DIAGNOSTIC_REMARKS=YES build

run: app
	xcrun simctl boot '$(SIM)' 2>/dev/null || true
	xcrun simctl install booted '$(APP_DD)/Build/Products/Debug-iphonesimulator/$(APP_BUNDLE)'
	xcrun simctl launch booted com.example.IceCubesApp
	# 這包 Xcode 沒有 Simulator.app（GUI），有就打開，沒有就用截圖看畫面。
	open -a Simulator 2>/dev/null \
		|| xcrun simctl io booted screenshot /tmp/icecubes.png \
		&& echo "截圖：/tmp/icecubes.png"

# ---- SwiftPM × 預設 swiftbuild 引擎 ----------------------------------------
# 設定寫進 $(SB_PKG)/.swiftpm，不是每次 build 的旗標。獨立 toolchain 不是 Xcode
# 內嵌的，所以一定要給 --experimental-plugin-path，否則會用內建 CAS，
# remote-service-path 等於白設。
swiftbuild: oss guard-snapshot guard-socket
	$(SB_SWIFT) package --package-path $(SB_PKG) experimental-build-cache configure \
		--enable-caching --path $(SB_CAS) --enable-diagnostic-remarks \
		--experimental-plugin-path $(PLUGIN) \
		--experimental-remote-service-path $(SOCK)
	rm -rf $(SB_PKG)/.build $(SB_CAS)
	cd $(SB_PKG) && $(SB_SWIFT) build

swiftbuild-verify: oss guard-snapshot guard-socket
	@$(SB_SWIFT) package --package-path $(SB_PKG) experimental-build-cache configure \
		--enable-caching --path $(SB_CAS) --enable-diagnostic-remarks \
		--experimental-plugin-path $(PLUGIN) \
		--experimental-remote-service-path $(SOCK)
	@echo "=== swiftbuild 引擎第 1 次：預期全 miss，內容上傳到 server ==="
	@rm -rf $(SB_PKG)/.build $(SB_CAS)
	@cd $(SB_PKG) && $(call timed,build,$(SB_SWIFT) build,Compiling|Build complete)
	@echo "=== swiftbuild 引擎第 2 次：清掉本機 CAS，只能從 server 拿 ==="
	@rm -rf $(SB_PKG)/.build $(SB_CAS)
	@cd $(SB_PKG) && $(call timed,build,$(SB_SWIFT) build,Compiling|Build complete)
	@echo "（命中看 server 統計：第 1 次 put、第 2 次 hit；兩次間隔短時會併成同一個 session）"

# 切到 6.5-dev 的三種方式，以及這台機器上的實際值。
toolchain:
	@echo "預設（Xcode 內建）：$$(swift --version 2>&1 | head -1)"
	@test -x $(SB_SWIFT) \
		&& echo "snapshot：        $$($(SB_SWIFT) --version 2>&1 | head -1)" \
		|| echo "snapshot：        找不到 $(SB_SWIFT)"
	@echo "路徑：            $(SNAPSHOT)"
	@echo "bundle id：       $(SNAPSHOT_ID)"
	@echo
	@echo "切換方式："
	@echo "  export TOOLCHAINS=$(SNAPSHOT_ID)        # swift / swiftc / xcrun"
	@echo "  xcodebuild … TOOLCHAINS=$(SNAPSHOT_ID)  # xcodebuild 不吃環境變數，要當 build setting"
	@echo "  $(SB_SWIFT) build  # 直接指路徑，make 用這個"
	@echo
	@echo "注意：compiler 版本是 cache key 的一部分，6.4 與 6.5-dev 不會互相命中。"

verify: spm-verify pkg-verify xcode-verify

status:
	@test -S $(SOCK) && echo "socket: 執行中 $(SOCK)" || echo "socket: 未啟動"
	@echo "objects: $$(find $(STORE)/objects -type f 2>/dev/null | wc -l | tr -d ' ')"
	@echo "kv:      $$(find $(STORE)/kv -type f 2>/dev/null | wc -l | tr -d ' ')"
	@echo "size:    $$(du -sh $(STORE) 2>/dev/null | cut -f1)"

clean-cache:
	rm -rf $(STORE) $(DD) $(CAS) $(SPM_CAS) $(PKG_DD) $(PKG_CAS) $(SB_CAS) \
		$(APP_DD) $(APP_CAS) $(SPM_PKG)/.build $(SB_PKG)/.build

clean: clean-cache
	cargo clean --manifest-path $(CARGO_MANIFEST)
	rm -rf $(OSS) $(SWB)

# socket 由執行中的 server 建立，不存在就代表沒開 server；
# 不先擋掉的話 plugin 會每個編譯工作 hang 30 秒再安靜退化成純本機快取。
guard-socket:
	@test -S $(SOCK) || { \
		echo "找不到 $(SOCK)，請先在另一個終端機跑 'make server'"; \
		exit 1; \
	}

guard-snapshot:
	@test -x $(SB_SWIFT) || { \
		echo "找不到 $(SB_SWIFT)"; \
		echo "預設 swiftbuild 引擎的快取支援只在 main（Swift 6.5-dev），請裝 main snapshot"; \
		echo "或用 SNAPSHOT=/path/to/toolchain.xctoolchain 覆寫"; \
		exit 1; \
	}
