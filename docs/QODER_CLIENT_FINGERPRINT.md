# Qoder 官方客户端指纹对照（qodercli 1.1.83）

记录官方 Qoder CLI 的 TLS ClientHello 与请求头，并与网关当前的 Qoder 客户端对照，供后续对齐使用。采集日期 2026-11-05。

## 采集方法

1. 从 npm 获取 `@qoder-ai/qodercli@1.1.83`（`bundle/qodercli.js`；同包 `qoder-worker-runtime.mjs` 中的相关函数相同），只做静态阅读。
2. 静态确认 CLI 的 HTTP 栈：
   - 启动时 `setGlobalDispatcher` 安装内置 **undici 7.1.3** 的 `Agent`；连接参数只有 keepAlive、超时和可选的额外 CA，**没有自定义 ciphers / ecdhCurve / sigalgs / secureOptions**。
   - 内置 undici 连接器：`allowH2 = allowH2 ?? false`，调用
     `tls.connect({highWaterMark:16384, servername, session, ALPNProtocols:["http/1.1"], port:443, host})`。
   - 因此 ClientHello 完全由 **用户本机 Node 的 TLS 默认值** + ALPN `http/1.1` + SNI 决定。
3. 将 Qoder 域名指向 `127.0.0.1`，本地 443 端口只读取并解析 ClientHello（不完成握手），分别用以下两者发起连接：
   - 系统 Node `v22.23.4`（官方构建，内置 OpenSSL 3.5.9）按上面完全相同的参数调用 `tls.connect`；
   - 与 `native_providers::http::build` 相同配置的 `reqwest 0.12.28 + rustls`（网关当前的 Qoder 客户端）。

未直接运行官方 CLI 本体（本次采集环境的安全策略不允许执行下载的第三方代码）。由于其 TLS 参数已静态确认为 Node 默认值，按相同参数调用 `tls.connect` 得到的 ClientHello 与 CLI 一致；首次连接之后的会话复用（PSK/session ticket）未覆盖。

## TLS 对照

| 项 | 官方 CLI（Node 22.23.4 / OpenSSL 3.5.9） | 网关当前（reqwest + rustls） |
| --- | --- | --- |
| JA4 | `t13d5212h1_b262b3658495_8e6e362c5eac` | `t13d1011h1_61a7ad8aa9b6_3fcd1a44f3e3` |
| JA3 (md5) | `d67b094811e5145139d7cea5f014309f` | `2d01bb186df08a190a589d4438fa5765` |
| ClientHello 长度 | 1612 字节（含 ML-KEM 混合密钥份额） | 262 字节 |
| ALPN | `http/1.1` | `http/1.1` |
| 版本 | TLS 1.3, 1.2 | TLS 1.3, 1.2 |
| 密码套件 | 52 个（含 DHE、ARIA、CCM、CBC） | 10 个（含 `0x00ff` SCSV） |
| 扩展（顺序） | renegotiation_info, server_name, ec_point_formats, supported_groups, session_ticket, alpn, encrypt_then_mac, extended_master_secret, signature_algorithms, supported_versions, psk_kex_modes, key_share | psk_kex_modes, key_share, ec_point_formats, supported_groups, supported_versions, signature_algorithms, server_name, status_request, session_ticket, extended_master_secret, alpn |
| 曲线/组 | X25519MLKEM768 (`0x11ec`), X25519, P-256, X448, P-384, P-521, ffdhe2048, ffdhe3072 | X25519, P-256, P-384 |
| 签名算法 | 26 个（含 ML-DSA `0x0904–0x0906`） | 9 个 |
| EC 点格式 | uncompressed, ansiX962_prime, ansiX962_char2 | uncompressed |

官方 CLI 完整 JA3 字符串：

```
771,4866-4867-4865-49199-49195-49200-49196-158-49191-103-49192-107-163-159-52393-52392-52394-49325-49311-49245-49249-49239-49235-162-49324-49310-49244-49248-49238-49234-49188-106-49187-64-49162-49172-57-56-49161-49171-51-50-157-49309-49233-156-49308-49232-61-60-53-47,65281-0-11-10-35-16-22-23-13-43-45-51,4588-29-23-30-24-25-256-257,0-1-2
```

签名算法：`0905 0906 0904 0403 0503 0603 0807 0808 081a 081b 081c 0809 080a 080b 0804 0805 0806 0401 0501 0601 0303 0301 0302 0402 0502 0602`

### 版本差异（未实测）

指纹随用户的 Node 版本变化（CLI 要求 Node ≥ 20）。X25519MLKEM768 组和 ML-DSA 签名算法来自 OpenSSL 3.5；使用 OpenSSL 3.0 的 Node 版本不会带这些字段，JA4 也会不同。真实用户群因此不是单一指纹，上表对应的是内置 OpenSSL 3.5 的 Node。

### 对齐可行性

- rustls 不支持 DHE、ARIA、CCM、FFDHE，也无法照搬 OpenSSL 的扩展顺序，无法通过配置得到上表指纹。
- WorkBuddy 使用的 rquest（BoringSSL）可以定制密码套件、曲线、签名算法和扩展顺序，但 BoringSSL 同样不支持 DHE/ARIA/CCM/FFDHE，只能近似、无法完全一致。
- 要完全一致，需要用 OpenSSL 3.5 本身（如 `openssl` crate 的 vendored 构建，配合 Node 的默认 cipher 列表与组）。代价是三个平台都要编译 OpenSSL（Windows 需额外工具链），需要单独评估。

## 请求头对照

| 头 | 官方 1.1.83 | 网关当前 |
| --- | --- | --- |
| `Cosy-ClientIp` | **不发送**（代码中不存在） | 发送（按账号派生的 169.254.x.y） |
| `Cosy-MachineCode` | 发送 | 不发送 |
| `Cosy-MachineHostname` | 真实 `os.hostname()`（规范化为可用作头的值），仅推理流式请求携带 | 按账号派生，仅 SSE 请求携带 |
| `Cosy-Version` / `Cosy-ClientType` / `Cosy-MachineOS` / `Cosy-MachineToken` / `Cosy-MachineType` / `Cosy-MachineId` | 发送 | 发送（版本默认 `1.1.75`） |

## 签名流程变化

1.1.83 的推理请求经过 **WASM 签名器**（`wasmInferSigningMeta` / `getWasmSigner`），响应经过解密器（`getResponseDecryptor`）。网关当前的 COSY 签名沿用 qoder2api-hub 记录的旧流程，版本号默认 `1.1.75`。服务端目前仍接受旧流程；若改为强制新签名，旧流程会失效，并且旧流程本身也是可区分的特征。

## 结论

1. TLS：网关与官方客户端的指纹差异显著（密码套件 10 对 52、扩展顺序不同、缺少 ML-KEM 组）。完全一致需要换用 OpenSSL 栈。
2. 请求头：应停止发送 `Cosy-ClientIp`、补上 `Cosy-MachineCode`，并更新默认版本号。
3. 签名：需要单独评估迁移到 1.1.83 的 WASM 签名流程。
