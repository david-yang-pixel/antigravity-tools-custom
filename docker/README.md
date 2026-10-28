# 🐋 Antigravity Manager 原生 Docker 部署手冊

本目錄包含 Antigravity Manager 的原生 Headless Docker 部署方案。該方案支持完整的 Web 管理界面、API 反代以及數據持久化，無需複雜的 VNC 或桌面環境。

## 🆕 本版本部署方案（本地前端構建復用）
適用於「前端近期不改、後端經常調整」的場景。思路是先在本地生成 `dist/`，Docker 只編譯後端並直接拷貝 `dist/`，大幅縮短構建時間並降低前端構建風險。

**步驟**
1. 本地生成前端靜態資源：
```bash
npm ci --legacy-peer-deps
npm run build
```
2. 使用本方案構建與啟動（後端-only + 復用 `dist/`）：
```bash
docker compose -f docker/docker-compose.yml -f docker/docker-compose.localdist.yml build
docker compose -f docker/docker-compose.yml -f docker/docker-compose.localdist.yml up -d
```
或合併為單條命令：
```bash
docker compose -f docker/docker-compose.yml -f docker/docker-compose.localdist.yml up -d --build
```

啟動後動態查看日誌：
```bash
docker compose -f docker/docker-compose.yml -f docker/docker-compose.localdist.yml logs -f --tail=200
```

**更新方式**
- 後端有改動：重跑上面的 `build` + `up -d`
- 前端有改動：先在本地重新 `npm run build`，再重跑 `build` + `up -d`

**Git 部署提醒**
- 若服務器不在本地構建前端，請確保 `dist/` 已提交到倉庫（本版本已從 `.gitignore` 移除）。

## 🚀 快速開始

### 1. 直接拉取鏡像 (推薦)
您可以直接從 Docker Hub 拉取已構建好的鏡像並啟动，無需獲取源碼：

> [!IMPORTANT]
> **安全警告**：從 v4.0.3 開始，Docker 版支持 **管理密碼與 API Key 分離**：
> *   **API Key**：通過 `-e API_KEY=xxx` 設置，用於所有 AI 協議的 API 調用鑒權。
> *   **Web 管理密碼**：通過 `-e WEB_PASSWORD=xxx` 設置，僅用於 Web UI 登錄。
> *   **默認行為**：若未設置 `WEB_PASSWORD`，系統會自動回退使用 `API_KEY` 作為登錄密碼。若兩者皆未設置，則生成隨機 Key。
> *   **查看方式**：執行 `docker logs antigravity-manager` 尋找 `Current API Key` 或 `Web UI Password`，或執行 `grep -E '"api_key"|"admin_password"' ~/.antigravity_tools/gui_config.json` 查看。

```bash
# 啟動容器 (請替换 your-secret-key 為強密鑰)
docker run -d \
  --name antigravity-manager \
  -p 8045:8045 \
  -e API_KEY=your-api-key \
  -e WEB_PASSWORD=your-login-password \
  -e ABV_MAX_BODY_SIZE=104857600 \
  -v ~/.antigravity_tools:/root/.antigravity_tools \
  lbjlaq/antigravity-manager:latest
```

> [!TIP]
> **🧪 體驗 Beta / 預覽版鏡像**：
> 若需使用最新的 Beta 預發布特性，請拉取對應的 Beta 版本 Tag（預發布版本獨立構建發布，不會覆蓋 `latest` 穩定版標籤）：
> ```bash
> # 拉取指定 Beta 預發布版本
> docker pull lbjlaq/antigravity-manager:v4.8.2-beta.0
> 
> # 運行 Beta 容器
> docker run -d --name antigravity-manager-beta \
>   -p 8045:8045 \
>   -e API_KEY=your-api-key \
>   -e WEB_PASSWORD=your-login-password \
>   -e ABV_MAX_BODY_SIZE=104857600 \
>   -v ~/.antigravity_tools:/root/.antigravity_tools \
>   lbjlaq/antigravity-manager:v4.8.2-beta.0
> ```
> 完整版本標籤請查看 [Docker Hub Tags](https://hub.docker.com/r/lbjlaq/antigravity-manager/tags)。若需直接運行未發版 Tag 的當前最新 `beta` 分支源碼，可在本地構建：`docker build -t lbjlaq/antigravity-manager:beta -f docker/Dockerfile .`。

#### 🔐 鑒權邏輯 (Security Scenarios)
*   **場景 A：僅設置了 `API_KEY`**
    - **Web 登錄**：使用 `API_KEY` 即可進入後台。
    - **API 調用**：使用 `API_KEY` 進行 AI 請求鑒權。
*   **場景 B：同時設置了 `API_KEY` 和 `WEB_PASSWORD` (推薦)**
    - **Web 登錄**：**必須**使用 `WEB_PASSWORD`。此時輸入 API Key 將被拒絕，確保管理權限與調用權限隔離。
    - **API 調用**：繼續使用 `API_KEY`。您可以放心地將 API Key 分發給團隊成員，而保留密碼僅供管理員使用。

#### 🆙 舊版本升級指引
如果您是從舊版本升級，默認沒有設置 `WEB_PASSWORD`。您可以通過以下方式添加：
1.  **Web UI (推薦)**：使用原有的 `API_KEY` 登錄，在 **API 反代** 設置頁面中設置新的管理密碼。
2.  **環境變量**：停止舊容器，啟動新容器時增加 `-e WEB_PASSWORD=您的新密碼`。

> [!TIP]
> **優先級邏輯 (Priority)**:
> - **環境變量** (`ABV_WEB_PASSWORD` / `WEB_PASSWORD`) 具有最高優先級。如果設置了環境變量，程序將始終使用它，忽略配置文件中的值。
> - **配置文件** (`gui_config.json`) 用於持久化存儲。當您通過 Web UI 修改密碼並保存時，新密碼會寫入此文件（JSON 字段名為 `admin_password`）。
> - **回退機制**: 如果上述兩者皆未設置，則回退使用 `API_KEY`；若連 `API_KEY` 也未設置，則隨機生成。

### 2. 使用 Docker Compose
在 `docker` 目錄下執行：
```bash
docker compose up -d
```

### 3. 手動構建鏡像 (開發者 / 二改版)
如果您需要修改代碼或自定義構建，請在項目根目錄下執行：

**Windows PowerShell（推薦）**
```powershell
# 一鍵構建二改版鏡像（標籤 antigravity-manager:local + 版本-fix）
.\docker\build.ps1

# 国内網絡加速
.\docker\build.ps1 -UseMirror

# 構建並推送到你自己的倉庫
.\docker\build.ps1 -UseMirror -Push -Registry "yourname/antigravity-manager"
```

**手動 docker build**
```bash
# 默認構建最新標籤
docker build -t antigravity-manager:local -f docker/Dockerfile .

# 二改版 Compose 啟動（Windows 可用端口映射，不用 host 網絡）
docker compose -f docker/docker-compose.yml -f docker/docker-compose.fork.yml up -d --build
```

#### 💡 構建參數
本鏡像支持自動鏡像源切換，以提升国内構建速度：
*   `USE_MIRROR`: 
    *   `auto` (默認): 自動檢測網絡環境，若無法訪問 Google 則切換至国内镜像（阿里云/NPM Mirror）。
    *   `true`: 強制使用国内镜像源。
    *   `false`: 強制使用官方默認源。

示例：
```bash
# 強制使用国内镜像加速構建
docker build --build-arg USE_MIRROR=true -t antigravity-manager:latest -f docker/Dockerfile .
```

## ⚙️ 環境變量配置

| 變量名 | 默認值 | 說明 |
| :--- | :--- | :--- |
| `PORT` | `8045` | 容器內服務監聽端口 |
| `ABV_API_KEY` | - | **[重要]** 代理 API 密鑰。客戶端（如 Claude Code）訪問時需提供的 Key |
| `ABV_WEB_PASSWORD` | - | **[安全]** Web 管理後台登錄密碼。若不設置則回退使用 API Key |
| `ABV_MAX_BODY_SIZE` | `104857600` | **[性能]** 最大請求體限制 (Byte)。默認 100MB，用於解決大圖傳輸 413 錯誤 |
| `LOG_LEVEL` | `info` | 日志等級 (debug, info, warn, error) |
| `ABV_DIST_PATH` | `/app/dist` | 前端靜態資源託管路徑 (Dockerfile 已內置) |
| `ABV_PUBLIC_URL` | - | 用於遠程 OAuth 回調的公網 URL (可選) |
| `ABV_PORTAL_REGISTRATION` | `false` | 用量中心是否開放郵箱註冊 (`true` / `false`)。註冊的帳號自帶一個額度為 0 的 Token，需管理員開通 |
| `ABV_PORTAL_EMAIL_DOMAINS` | - | 允許註冊的郵箱網域，逗號分隔（含子網域）；留空不限制 |
| `ABV_SMTP_HOST` / `ABV_SMTP_PORT` | - / `465` | 驗證碼郵件的 SMTP 伺服器與連接埠 |
| `ABV_SMTP_SECURITY` | `tls` | `tls`（465）、`starttls`（587）或 `none`（僅限內網中繼） |
| `ABV_SMTP_USERNAME` / `ABV_SMTP_PASSWORD` | - | SMTP 登入帳號與密碼（授權碼） |
| `ABV_SMTP_FROM` / `ABV_SMTP_FROM_NAME` | - | 寄件地址與寄件者名稱 |
| `ABV_ZPAY_ENABLED` | `false` | 用量中心是否開放線上儲值（ZPay 等相容易支付的閘道） |
| `ABV_ZPAY_GATEWAY` | `https://zpayz.cn` | 支付閘道位址，必須是 HTTPS（查單請求會帶上商戶密鑰） |
| `ABV_ZPAY_PID` / `ABV_ZPAY_KEY` | - | 商戶 ID 與商戶密鑰（ZPay 後台「支付渠道 → API安全」） |
| `ABV_ZPAY_PUBLIC_URL` | - | 本站公網位址，例如 `https://example.com`；回調位址為 `{位址}/api/pay/epay/notify`，用戶付款後返回 `{位址}/usage` |
| `ABV_ZPAY_PAY_TYPES` | `alipay` | 提供的支付方式，逗號分隔：`alipay`、`wxpay`；只填已開通並簽約的渠道 |
| `ABV_ZPAY_MIN_AMOUNT` / `ABV_ZPAY_MAX_AMOUNT` | `5` / `1000` | 單筆儲值金額範圍（元，最多兩位小數）；兩者需滿足 0.01 ≤ 最低 ≤ 最高 ≤ 100000，否則忽略 |
| `ABV_ZPAY_SUBJECT` | `API 调用额度充值` | 支付頁顯示的商品名稱，需如實描述售賣內容（不可含 `&` 或 `=`） |
| `ABV_PORTAL_ANNOUNCEMENT` | - | 用量中心登入頁與首頁顯示的公告（純文字，`\n` 換行）；設為空字串則關閉公告 |
| `ABV_PORTAL_ANNOUNCEMENT_LEVEL` | `info` | 公告樣式：`info`（一般）或 `warning`（醒目提醒） |
| `ABV_ALERT_ENABLED` | `false` | 是否啟用維運告警：模型連續失敗、上游額度耗盡、WorkBuddy 點數低於門檻時推送 |
| `ABV_ALERT_WEBHOOKS` | - | 告警 Webhook，逗號分隔；自動識別企業微信、釘釘（自訂關鍵字填「告警」）、飛書、Slack、Discord、Server醬，其他位址收到含 `title`/`body`/`text` 的 JSON |
| `ABV_ALERT_EMAILS` | - | 告警收件信箱，逗號分隔（透過上方 SMTP 寄送） |
| `ABV_ALERT_FAILURE_THRESHOLD` | `5` | 同一模型連續上游失敗（401/403/429/5xx）幾次後告警（1-1000） |
| `ABV_ALERT_COOLDOWN_MINUTES` | `30` | 同一模型或帳號的重複提醒間隔（1-1440 分鐘） |
| `ABV_ALERT_CREDIT_THRESHOLD` | `0` | WorkBuddy 帳號剩餘點數 ≤ 此值時告警；`0` 表示僅在用完時告警 |

> 以上用量中心設定（含線上儲值）也可以在管理後台「用戶 Token → 註冊設置」中修改，保存在數據目錄的 `portal_settings.json`；設置了環境變量的欄位以環境變量為準，後台顯示為鎖定。反向代理（如 Nginx）請以 `proxy_set_header X-Forwarded-For $remote_addr;` 覆蓋客戶端 IP，並傳遞 `X-Forwarded-Proto`，以確保登入限流與 Cookie 的 `Secure` 屬性正確生效。

用量中心的「餘額」頁籤僅對按餘額計費的用戶顯示，包含儲值、待支付訂單與可分頁篩選的歷史記錄；餘額為零也可儲值。待支付訂單可在建立後 24 小時內繼續付款，重新登入或返回頁面不會延長期限。倒數依伺服器時間計算，到期後從待支付區移除，可在「已關閉」記錄中查詢。

繼續支付會核對訂單歸屬、原金額、支付方式與閘道狀態，沿用原訂單號；閘道查詢失敗時不會重新簽發支付表單。此期限限制的是本站發起／繼續支付的入口，不代表支付閘道已撤銷先前開啟的收銀台。訂單和對帳記錄不會刪除，經驗證的延遲付款仍會按原有冪等流程入帳。新介面沿用既有資料表，舊訂單的截止時間由建立時間推算。

## 📂 數據持久化
請務必將宿主機目錄掛載至容器內的 `/root/.antigravity_tools`，否則賬號和配置在容器重啟後會丟失。

## 🌐 訪問位址
*   **管理界面**: [http://localhost:8045](http://localhost:8045)
*   **API Base**: [http://localhost:8045/v1](http://localhost:8045/v1)

## 📦 Docker Hub 分發 (推薦)
若要推送至你的倉庫：
```bash
# 打上版本標籤並推送
docker tag antigravity-manager:latest lbjlaq/antigravity-manager:latest
docker tag antigravity-manager:latest lbjlaq/antigravity-manager:4.3.0
docker push lbjlaq/antigravity-manager:latest
docker push lbjlaq/antigravity-manager:4.3.0
```
