# Qoder CLI 1.1.83 协议与客户端指纹

默认路径已接入官方 1.1.83 WASM 签名/响应解码、原生 UMID 机器标识和
Node/OpenSSL 3.5 的 TLS 配置。实现位于 `src-tauri/crates/qoder-client`；
网关只负责账号、出口和协议适配，不在通用 Gemini pipeline 中加入这些差异。

## TLS

官方 CLI 使用内置 undici 7.1.3，TLS 采用当前 Node 的默认配置，ALPN 只包含
`http/1.1`。官方用户的 Node/OpenSSL 版本不唯一，因此这里明确对齐 **OpenSSL 3.5
系列的 Node 默认配置**，不声称复制任意 Node 版本的 ClientHello。

网关为 Qoder 单独使用 vendored OpenSSL 3.5.5，固定 Node 的 cipher list、TLS 1.2–1.3、
ALPN 和 supported groups，避免 Linux/macOS/Windows 系统 TLS 库改变指纹。
WorkBuddy 仍使用自己的 rquest 配置，其余来源不切换 TLS 客户端。

两套 TLS 库在同一可执行文件中需要隔离 C 符号。`crates/boring-sys2` 对原有
4.15.15 依赖施加构建补丁，利用 BoringSSL 的前缀头将原生符号命名为 `abv_bssl_*`，
Rust API 和 TLS 配置保持原样。上游源码包校验后本地解压，无构建时下载；原生构建
增加一轮符号收集编译。补丁支持解析 ELF/Mach-O/COFF 目标文件，拒绝未隔离符号和
外部预编译/FIPS 覆盖；许可证和完整来源见该目录 README。

本地捕获测试逐项对比实际发出的 ClientHello 与 Node 24.19.0 / OpenSSL 3.5.7
对照数据（`tests/fixtures/node-tls-3.5.json`），以下字段一致：

| 字段 | 对齐结果 |
| --- | --- |
| 密码套件 | 同顺序的 52 项，含 DHE、ARIA、CCM、CBC |
| 扩展 | renegotiation_info、server_name、ec_point_formats、supported_groups、session_ticket、alpn、encrypt_then_mac、extended_master_secret、signature_algorithms、supported_versions、psk_kex_modes、key_share |
| supported groups | X25519MLKEM768、X25519、P-256、X448、P-384、P-521、ffdhe2048、ffdhe3072 |
| 签名算法 | 同顺序的 26 项，含 ML-DSA |
| EC 点格式 | uncompressed、ansiX962_prime、ansiX962_char2 |
| ALPN | http/1.1 |
| TLS 版本 | 1.3、1.2 |

随机数、密钥份额和会话票据不固定。账号内保留 TLS 会话复用；账号之间的 HTTP 连接池
和会话缓存独立。本地 TLS 服务测试验证了同一账号第二次握手复用、另一账号不复用。
HTTP/HTTPS CONNECT、SOCKS4/4a/5/5h 出口仍进行目标域名证书校验；禁止重定向和
传输层自动重放。代理认证只发送给代理。整体请求截止时间覆盖响应体，压缩响应按
undici 的 gzip/deflate/br 行为解码。

## 请求头与机器身份

原研究只搜索了 JavaScript 字符串，遗漏了 WASM 生成的头。运行官方 WASM 后确认：

| 字段 | 推理请求 | 模型列表请求 |
| --- | --- | --- |
| `Cosy-ClientIp` | 不发送 | 官方 WASM 仍生成，值来自持久化 machine ID |
| `Cosy-MachineCode` / `Cosy-MachineToken` / `Cosy-MachineType` | 官方 UMID 输出 | 官方 UMID 输出 |
| `Cosy-MachineHostname` | 当前主机名，按官方规则规范化并限制长度 | 不附加 |
| `Cosy-MachineId` | 账号持久化 machine ID | 账号持久化 machine ID |
| `Cosy-Version` | 默认 1.1.83 | 默认 1.1.83 |

UMID 不是可用随机字符串替代的字段。程序嵌入该版本官方平台组件，执行前检查
SHA-256，再提取到 `<data-dir>/qoder-runtime/1.1.83-<platform>/`。支持 Linux/macOS
x64、ARM64 和 Windows x64；Windows 同时携带官方 `sgsdk.dll`。不要求安装 Node，
不在运行时下载组件，也不把伪造的机器码作为失败兜底。

组件按真实机器生成身份，同一台主机上的账号可能拥有相同的机器特征。成功结果按
账号/区域缓存，使用时才检查是否需更新；组件执行、输出大小和退出等待有上限。
SDK 自己的设备身份初始化独立于网关 HTTP 栈，**其流量不受账号出口配置控制**。
网关的登录轮询、凭证刷新、模型、额度和推理 HTTP 请求仍走账号绑定出口。
使用账号代理或通用上游代理时，默认在启动 SDK 前拒绝现代签名请求；管理员只有在
接受 SDK 直连并暴露本机网络/机器身份的情况下，才应设置
`ABV_QODER_ALLOW_DIRECT_UMID=true`。另一种选择是显式启用旧协议。版本检测不启动 SDK。

## 签名与响应

`qoder_auth.wasm` 直接提取自 npm `@qoder-ai/qodercli@1.1.83`：

```
SHA-256 c7a14b11d6c23b94ac39f6868ebd3dd6a70492efe87c864c33857adadb9f0951
```

Rust 使用 wasmi 执行该模块，提供 wasm-bindgen 所需的字符串、Map、字节数组、时间和
OS 安全随机数。WASM 没有文件、网络和进程接口，设置了内存、指令预算及输入大小上限。

现代签名准备按账号独立接纳任务，每账号覆盖全部 8 个可配置推理槽位及 1 个后台模型目录准备；
没有跨账号共享的 4 任务接纳上限，单个账号超出自身准备容量时在初始化 UMID 前返回 503。
请求体在入队前按 WASM 的 16 MiB 上限检查。最多一个签名任务进入阻塞线程池，其余
任务异步等待；排队及执行共用 20 秒上限，并受调用方更短的请求截止时间约束。
不同账号共用 CPU 执行顺序，不共用接纳预算。超时或取消会释放未启动任务及其请求体，
并取消线程池中尚未启动的工作。已经运行的
WASM 仍受指令预算约束，保留队列容量直到结束，避免超时后新任务不断累积。

1. `generate_runtime_auth_fields` 生成当前账号的加密认证字段。
2. `QoderContext.prepareInferRequest` 返回推理 URL、请求体和签名头。
3. `QoderContext.prepareRequest` 处理模型列表请求；不再强行套用旧版 Encode 查询参数。
4. `decrypt_server_response` 在 JSON/SSE 归一化前解码响应，兼容明文 JSON 和 `[DONE]`。

签名上下文按账号缓存；Token、machine ID、版本、组织信息或数据策略变化时重建，删除
账号时清理。WASM 生成的 URL 仍需通过官方 HTTPS 域名校验，不能由响应或导入数据指定
任意凭证接收地址。

## 来源、验证与回退

### 官方新版本检测

Qoder 账号页显示网关内置版本、官方 npm `latest` 版本和上次成功检测时间。
打开页面时自动查询，支持手动「检测官方新版本」。比较使用 SemVer，基准始终是
实际内置的 `CLI_VERSION`；`ABV_QODER_VERSION` 覆盖和旧协议模式单独显示，不会
让页面把版本号覆盖误认为组件已经升级。

检测只访问 `https://registry.npmjs.org/-/package/@qoder-ai%2fqodercli/dist-tags`，
使用网关通用上游代理，不使用账号出口或凭据、不调用 UMID。请求禁止重定向，并限制
等待时间和响应大小。成功结果在进程内缓存 24 小时，失败后自动查询间隔 5 分钟；
手动刷新可跳过该缓存，但同一网关每分钟至多请求一次。并发检测合并。
页面关闭后不启动定时轮询。失败保留上次版本与成功时间，并明确显示失败状态。

桌面命令 `check_native_client_version` 与管理员接口
`POST /api/native-providers/client-version` 共用实现，接口正文为
`{"provider":"qoder","refresh":false}`，手动刷新使用 `true`。结果包含
`bundled_version`、`advertised_version`、`legacy_protocol`、`latest_version`、
`update_available`、`checked_at`、`last_attempt_at` 和 `error`；时间为 Unix 秒，
未成功查询的版本/时间/更新结论为 `null`。网络失败通过 `error` 表示，不误报为没有新版。

这是发版提示，不代表新版已适配。检测不下载组件、不修改版本配置；更新 WASM、UMID
或 TLS 配置仍需经过验证后随网关发布。

### 资源验证与回退

上游 npm integrity、各文件 SHA-256、Apache-2.0 许可证与 NOTICE 存于
`crates/qoder-client/vendor/`。可重新验证提取，并用官方 JS glue 生成独立测试向量：

```bash
python scripts/vendor-qoder-client.py --fixtures
cd src-tauri
cargo test -p qoder-client --tests --locked
```

测试向量只使用假账号、固定时间和测试随机数；正式运行始终使用 OS 安全随机数。
专项测试覆盖官方签名 URL/请求体/完整请求头、响应解码、WASM 资源释放、TLS 捕获、
会话隔离、代理认证/远端 DNS、证书拒绝、压缩和流式截止时间。它们不运行原生 UMID
程序，也不消耗真实账号额度。真实账号端到端调用以及 macOS/Windows 在线 SDK 行为
仍需目标环境验证；不能将离线向量通过解释为服务端授权成功。

`tls_coexistence` 集成测试在同一进程中链接并使用 OpenSSL 和 BoringSSL，捕获两者
握手并核对 Qoder 的 Node 密码套件及 rquest 的 Chrome HTTP/2 协商。新版本检测测试
覆盖 SemVer、缓存/并发合并、失败保留旧结果、重定向拒绝和响应大小上限。

如需回退，设置 `ABV_QODER_LEGACY_PROTOCOL=true` 后重启。该开关明确选择原来的
Rust COSY 签名、rustls 传输和账号派生标识，默认版本恢复为 1.1.75；不会调用 UMID。
现代路径失败时不会自动降级或跨协议重放。`ABV_QODER_VERSION` 可显式覆盖客户端版本，
但不会替换固定的 WASM/UMID 组件；升级这些组件必须同时更新 provenance 和测试向量。
