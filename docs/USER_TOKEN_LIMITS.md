# 用户 Token 配额、模型权限与用量中心

此定制修改基于官方 `v4.9.6`，基础提交为 `b601f5e7375343343922ccab5e50653bc3450c3b`。

## 使用方式

在「用户 Token」的创建和编辑窗口设置：

- **最大并发数**：单个 Key 同时处理的请求数。`0` 表示不限；流式响应持续占用名额，直到结束或断开并完成用量入账。
- **允许使用的模型**：选择全部或勾选指定模型，可搜索并输入自定义模型 ID；创建、编辑均可设置。选择指定模型时至少选一项。
- **累计 Token 总额度**：界面以 M（百万）为单位填写，使用量显示为 M tokens（1 M = 100 万，0.5 M = 50 万），支持最多 6 位小数。按网关记录的输入 + 输出 Token 累计。`0` 表示不限，不按天/月重置。

达到额度后，新请求返回 HTTP 429，错误码为 `token_quota_exhausted`。已放行的请求正常结束，最后一批请求可能超过额度。并发满时返回 HTTP 429，错误码为 `concurrency_limit_exceeded`，其他 Key 不受影响。

已有 Key 默认不限并发、不限额度，原 Key 和累计用量保留。给已有 Key 设置额度会包含历史用量；额度小于或等于已用量时，立即阻止新请求。编辑额度、续期均不会清零；提高额度或设置为 0 后恢复放行。降低并发不会中断已有请求。

列表显示已用/总额度、剩余额度或“额度已用完”、最大并发数和模型权限。

## 用户用量中心

部署后访问同域名 `/usage`，或点击管理页「用户用量页」。用户以自己的 API Token 登录，默认进入「用量概览」。顶部通过「用量概览 / 可用模型 / 用量统计 / 调用日志」切换内容，查看：

- 累计已用、剩余额度、总额度、请求数、并发限制和到期状态；Token 数统一使用 M tokens。
- 允许使用的模型 ID；不限制模型的 Key 也列出当前公开模型目录。
- 每个模型的请求数、失败数、输入、输出、缓存命中及总用量，支持搜索。
- 日统计（最近 30 天）和周统计（最近 12 周），包含当前周期和无请求周期。按 UTC+8 切日，周一开始；当前周期尚未结束。统计不重置累计额度。
- 按时间倒序的调用日志，每页 20 条：时间、模型、状态码、输入、输出、缓存命中、合计。支持 UTC+8 起止时间筛选（精确到秒、包含两个边界），可只填一端；筛选后回到第一页，翻页和刷新保留已应用范围。该范围只影响调用日志，不改变累计或日/周统计。日志包括已进入模型处理流程的成功或失败调用；鉴权、限额、模型权限校验阶段拒绝的请求不在此列。

缓存命中包含在输入 Token 中，不重复计入总用量。旧日志和上游未提供缓存计数的请求保留 `NULL`，界面显示「未记录」；明确返回 0 才显示 0。混合新旧数据的汇总显示「部分记录缺失」，仅累计已记录的缓存数。

概览下方展示 OpenAI Chat、Responses、Anthropic、Gemini 的客户端 Base URL 和请求路径，地址来自当前页面 origin。当前登录 Token 默认遮蔽，支持显示/隐藏和复制；无需由后端重新返回完整 Key。

Token 只保存在当前页面内存，刷新页面需要重新登录。接口仅返回该 Key 的用量，不返回完整 Key、IP、其他用户信息或请求/响应正文。过期或额度耗尽的 Key 仍可查看历史；禁用或删除后无法登录。查看用量不占并发或扣额度。

模型权限以客户端请求的完整模型 ID 为准，管理员的模型映射继续生效。未允许的模型返回 403 `model_not_allowed`，公开模型列表也会按权限过滤。受模型限制的请求必须明确指定模型。新增用量统计及调用日志支持 HTTP/SSE；原版不受限制的 WebSocket 调用不会经过该用量记录流程。配置了模型、并发或额度限制的 Key 只支持 HTTP/SSE；旧 WebSocket 通道绕过逐帧统计，握手被拒绝为 403 `restricted_key_requires_http`。

## API 与兼容性

现有管理 API 保持不变：

- `POST /api/user-tokens` 创建：新增 `max_concurrency`、`token_quota`（省略为 0）与 `allowed_models`（省略为 `[]`，即全部模型）。
- `PATCH /api/user-tokens/:id` 编辑：省略字段保持原值，显式传 0 解除相应限制。
- 新字段同时支持桌面 Tauri 命令与 headless 管理接口。`allowed_models` 省略保持原值，显式 `[]` 解除模型限制；不支持通配符。
- `GET /api/user-token-models`：管理员获取当前模型目录。
- `GET /api/token-portal/me?page=1`：使用用户 Key 的 Bearer 鉴权，返回概览、模型目录、模型汇总、日/周统计和分页日志；page 必须是 1–1000000；可传 `start_time` / `end_time`（Unix 秒，包含边界），反向或非法时间范围返回 400。数据归属只由 Key 确定，忽略调用方提供的 token_id。响应 `Cache-Control: no-store`。

管理 API 和数据库仍使用精确的单个 Token 数；界面与后端间自动换算，不改变已保存额度。

最大并发数接受 0–2147483647 的整数；总额度接受 0–9007199254740991 的整数。负数、非整数或超出范围的输入被拒绝。

启动时对 `user_tokens.db` 做事务内的增量迁移，只添加用户表三列和用量日志的可空缓存列，不删除原数据。累计用量存储在 SQLite；并发名额保存在进程内，适用于当前单实例部署。多实例共享限额需要独立的分布式计数方案。

限制适用于用户 Token；管理员/全局代理 Key 维持原有权限。生产环境应开启代理鉴权；关闭鉴权意味着调用方可以完全不提交 Key。已提供的有效用户 Key 即使在 auth Off 模式下仍执行其限制。

累计用量沿用网关现有统计口径，优先使用响应 usage，缺少输入统计时使用原有估算。中断流、缺失上游 usage 或进程意外退出可能无法获得完整实际用量，因此它不是上游账单精确结算。HTTP 错误响应不会凭空按请求内容估算扣减输入 Token。已收到的实际 usage 仍会记录。

数据库入账失败会记录错误，并在当前进程内暂停该 Key 后续请求（HTTP 503 `token_accounting_unavailable`），避免静默漏记继续放行。管理员需修复数据库、核对遗漏用量，再重启服务；重启本身不能恢复遗漏的 usage。

## 已验证

- `npm run build`：通过 TypeScript 检查及 Vite 构建。
- `cargo test --lib user_token --no-default-features`：24 项通过，覆盖迁移、旧请求兼容、创建/编辑/解除限制、累计用量保留、并行入账、并发竞争、Key 隔离、流式生命周期及三种认证请求头、模型限制与绕过拦截、门户身份隔离、日志分页、UTC+8 日/周边界及缓存数据缺失。
- `cargo test --lib proxy::middleware::monitor::tests --no-default-features`：7 项通过，覆盖 OpenAI Chat/Responses、Anthropic、Gemini 用量解析和客户端断开。
- `cargo build --bin antigravity-tools --no-default-features`：macOS 本地后端编译通过。
- Chrome + 独立本地 headless 服务：创建并发 2 / 额度 100 万的测试 Key，编辑为并发 3 / 额度 200 万，刷新及 API 回读一致；负额度、非整数及超范围额度返回 400/422。

Chrome 本地用户页联测：模型列表、各模型用量、缓存缺失提示、日/周统计、20 条分页日志；390px 手机视口无页面横向溢出。管理页模型选项创建/编辑后回读一致；用户 Key 访问管理 API 为 401，未授权模型为 403。无上游账号的真实本地失败调用落库且不误扣 Token。

验证使用临时本地数据和测试 Key，未使用生产账号，未调用付费模型。未构建 Linux Docker 镜像、未部署首尔服务器，亦未验证生产上游调用。原版编译警告及 Vite 大 chunk 提示仍存在。

## 部署与回退准备

部署前需备份现有镜像引用、Compose 以及数据目录，并核对服务器已有本地补丁，避免覆盖其行为。用本分支源码构建单独的镜像标签，保持现有数据卷和监听地址；不要直接替换官方 latest 标签。

首次启动会自动迁移表结构。部署后需用专门测试 Key 验证创建/编辑、并发拒绝、额度拒绝和一次真实流式请求的入账，确认后再设置实际 Key 限额。

如回退原版镜像，新增字段可以保留，不需要删除数据库或清零用量；原版不执行新增的并发、额度和模型限制，也不提供用户用量中心。

### Upstream model catalogs

`proxy.only_raw_quota_models` defaults to `true` for new configurations. Enable
**Upstream Models Only** on existing installations to use exact model IDs reported
by accounts in the proxy pool. The admin integration table, CLI model choices,
Token model selector, protocol model-list APIs and usage portal share the backend
catalog. No built-in models, derived names, normalized quota buckets or version
baseline substitutions are added in this mode. Empty pools produce empty catalogs.

The portal intersects this catalog with each key's saved model permissions. Saved
permissions and request routing are not rewritten when catalog entries change.
Disabling the option restores the legacy expanded catalog. Web/Docker settings
apply immediately, without restarting the service.

An upstream catalog is not a health check: a listed model may still be limited or
routed to another model. In the 2026-10-14 production checks, 17 of 18 upstream IDs
returned generated content across the checks; `gemini-2.5-pro` returned HTTP 429.
The three Gemini 2.5 Flash IDs returned `gemini-3.1-flash-lite` in response metadata.
These observations are point-in-time, not permanent capability guarantees.
