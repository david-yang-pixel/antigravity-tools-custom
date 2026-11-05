# WorkBuddy 客户端环境对齐记录

## 取证基准

2026-11-05 从官方首页 `https://www.workbuddy.cn/` 的下载模块追踪至
`https://www.codebuddy.cn/v2/update?platform=workbuddy-linux-x64-deb`。
下载元数据、安装包 SHA-256、关键文件 SHA-256 和无账号 TLS 捕获结果保存在
`src-tauri/src/modules/workbuddy/fixtures/linux-5.6.2.json`。

- 官方安装包：WorkBuddy Linux 5.6.2.40046327，产品版本 5.6.2。
- 安装包 SHA-256：`12149ad8c0cd748fd5062d8f62c7827d688d8095427ada99b1ae9e7019897523`。
- 随包 CLI 的内嵌发布元数据：2.147.0；Axios：1.16.1。
- 实际执行随包运行时的 `process.versions`：Electron 37.10.3、Node 22.21.1、
  Chromium 138.0.7204.251、undici 6.22.0，TLS 使用 Electron 的 BoringSSL。

只静态检查发行包并对 loopback 捕获握手；未加载应用主入口、登录账号、调用
腾讯业务 API、运行 Turing SDK 或消耗积分。仓库不包含腾讯安装包或源码副本。

## 已采用的行为

- `main/tar.js` 的 `AuthService.buildHeaders` / `buildHeadersWithTuringToken`：
  计费/签到只带 JSON 协商、Bearer、用户 ID、可选企业/租户/域及真实设备令牌。
  与推理的 Origin、Referer、machine/session、CLI 产品头分开。
  Axios 默认 UA 来自随包 Axios 版本。
- `main/index.js` 的 `TuringSdkService`：设备令牌来自原生 SDK，绝非 UUID。
  官方 SDK 仅支持 Windows/macOS，Linux 不支持。网关不合成设备证明，不伪装 SDK
  成功；导入的令牌是客户端快照，不声称能代替 SDK 持续刷新或通过服务端验证。
- 随包 CLI `buildConversationHeaders`：同一次请求的 message ID 和 request ID
  使用同一个生成值；会话 ID 在同一账号的同一会话内稳定，不在账号之间共享。
  agent intent/type 和 private-data 字段按已观察到的主会话语义设置。
- 客户端版本按账号保存，版本、UA 和 IDE 版本一致，不按账号随机散开版本。
  已知旧默认组合 5.5.4 / 2.137.1 升级至上述已核验组合；显式保存的其他版本保留。
  机器/会话 ID 在重新授权后保留，新导入的真实 SDK 令牌可以替换旧快照。
- 默认连接配置对齐官方运行时 **fetch** 捕获：HTTP/1.1、TLS 1.2–1.3、
  18 项 cipher suites、3 项 supported groups、9 项 signature algorithms 和
  extension 顺序。不发送 Chrome 浏览器 GREASE/ALPS、HTTP/2 或浏览器默认头。
  随机数、密钥及会话票据仍由 TLS 实现生成。
- 连接缓存按账号隔离，账号绑定的节点改变后重建连接；失败不自动改绑或直连。
  签到的每日次数、退避、暂停和恢复规则见 `WORKBUDDY_UPSTREAM.md`。

## 验证与边界

`workbuddy_client_hello_matches_the_official_electron_node_fetch_capture` 向本地监听器
发起握手，按 fixture 比较 cipher suites、extension 顺序、groups、签名算法、
ALPN 和 TLS versions。握手捕获后关闭连接，不关闭证书验证。

这不是所有平台和所有客户端路径的完整复制：官方 `https.Agent` / Axios 捕获
不发送 ALPN，而当前 rquest API 的 HTTP/1.1 配置发送 `http/1.1`，与官方 fetch
路径一致。macOS/Windows 的 Turing SDK、全局版发行包、真实账号端到端调用及
腾讯未公开的风控判断尚未验证。不能将本地握手或单测通过解释为“不会封号”。

复现官方捕获：先核验并解压上述安装包，保留 Electron 二进制旁的 ICU 和 snapshot
文件，然后运行 `python3 scripts/capture-workbuddy-tls.py /path/to/workbuddy`。
该脚本仅使用指定运行时向本机监听器发送 `https` 和 `fetch` 握手。

回退原连接配置：设置 `ABV_WORKBUDDY_LEGACY_CHROME=true` 并重启服务。
这是显式兼容回退；正常路径失败不自动切换或重放。回退不撤销签到保护和账号隔离。
