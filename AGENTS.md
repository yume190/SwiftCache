# 使用者連線保護

- 使用者建立的 Demo 連線屬於使用者資料，必須保留。
- 除非使用者明確要求操作指定連線，禁止修改、覆寫或刪除 Demo 連線設定，禁止啟動、停止或重啟它們，也禁止清除其快取或歷史紀錄。
- 自動驗證一律使用新建的獨立 `SWIFTCACHE_DATA_DIR`，僅操作該次驗證自行建立的連線。不得沿用使用者正在操作或存放 Demo 連線的資料目錄。
- 不得為了測試、截圖、整理環境或提交程式碼而重設使用者的連線或服務狀態。
- `/tmp/swiftcache-verify.2wt7` 已由使用者接手並存放 `Server Demo`／`Proxy Demo`，不是可再次操作的測試環境；其中的 `ui-command.json` 也禁止寫入。
- 使用者正常資料目錄 `~/Library/Application Support/dev.yume.SwiftCache`，以及 Demo 使用的 `/tmp/cas-local`、`/tmp/cas-proxy`、`/tmp/swiftcache.sock`，不得作為自動驗證資源。資料目錄不同不代表 store、socket 或 daemon 已隔離。
- 驗證只能控制本次自行啟動、且未由使用者接手的程序；每次明確記錄 PID、資料目錄及 daemon descriptor，禁止沿用舊視窗、舊 smoke helper 的目標或使用者的 `ui-command.json`。
- 建置或打包前須確認使用者正在執行的 UI／daemon 執行檔位置；不得覆寫其 app bundle 或在原地重打包。需要時使用獨立 checkout／worktree、獨立 bundle 路徑及獨立資料與 socket／store。
