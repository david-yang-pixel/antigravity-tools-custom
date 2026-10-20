# 用户 Token 余额计费、免费模型与用量中心

管理员创建和用户注册的 Key 均默认启用余额计费，初始余额为 0，免费模型名单为空。累计 Token 数继续统计，不再作为总额度限制。

## 使用方式

在「用户 Token」创建和编辑窗口设置：

- **免费模型**：勾选的完整模型 ID 对该 Key 免费，不扣余额；默认全部不选。未勾选的模型按管理员配置的价格计费。未定价且未授予免费的模型禁止调用。
- **余额**：以人民币展示，API 以微元整数存储（1 元 = 1,000,000 微元）。没有关闭余额计费的开关。
- **最大并发数**：单个 Key 同时处理的请求数，0 表示不限。流式响应持续占用名额，直到结束或断开并完成用量入账。降低并发不会中断已有请求。

收费模型余额不足返回 HTTP 402 `insufficient_balance`；未定价且非免费模型返回 HTTP 403 `model_not_priced`。并发满时返回 HTTP 429 `concurrency_limit_exceeded`。免费模型可在零余额或负余额时使用，仍遵守账号启停、有效期、IP、宵禁和并发限制。

模型按客户端请求的完整 ID 判断免费授权和价格，管理员的模型映射继续生效。按次计费仅对成功完成的请求扣费；按 Token 计费沿用已采集用量结算。请求放行后才知道完整用量，余额可能因结算或并发请求变为少量负数。

## 用户用量中心

访问同域名 `/usage`，可使用自己的 API Token 或注册账号登录。顶部包含用量概览、余额、可用模型、用量统计和调用日志：

- 概览展示余额、累计 Token 用量、请求数、并发和到期状态；Token 数以 M tokens（百万）展示。
- 余额页提供充值及充值记录；有效期内的待支付订单可继续支付并显示倒计时。
- 可用模型只展示管理员已添加、当前有账号通过测试且已定价的模型。免费授权不代替定价；价格可以设置为 0。
- 每模型汇总请求、失败、输入、输出、缓存及总用量；日统计为最近 30 天，周统计为最近 12 周，按 UTC+8 切日、周一开始。
- 调用日志每页 20 条，包含时间、模型、状态码、输入、输出、缓存、总 Token 和实际扣费金额。支持 UTC+8 起止时间筛选，包含边界。鉴权或计费准入阶段拒绝的请求不进入调用日志。

缓存命中包含在输入 Token 中，不重复计入总用量。旧日志和未提供缓存计数的请求显示「未记录」；混合汇总显示「部分记录缺失」。

概览下方保留 OpenAI Chat、Responses、Anthropic、Gemini 的客户端地址和 Token 显示/复制入口。用量接口只返回当前身份的数据，不返回其他用户信息或请求/响应正文。过期或余额不足的 Key 仍可查看历史；禁用或删除后无法以 Key 登录。查看用量不占并发或扣余额。

用户 Key 使用 HTTP/SSE。旧 WebSocket 通道无法逐帧执行计费和统计，因此用户 Key 的 WebSocket 握手返回 403 `restricted_key_requires_http`。

## API 与兼容性

- `POST /api/user-tokens`：`max_concurrency` 默认 0，`balance_micros` 默认 0，`free_models` 默认 `[]`。
- `PATCH /api/user-tokens/:id`：省略字段保持原值；`free_models: []` 清空免费名单；`max_concurrency: 0` 取消并发限制。`balance_micros` 为数值时设置余额，`null` 无效，不会关闭计费。
- 免费名单只接受完整模型 ID，不支持通配符。最大并发数接受 0–2147483647 的整数。金额必须为支持范围内的整数，不能用浮点数代替微元。
- `GET /api/user-token-models`：管理员获取当前模型目录。
- `GET /api/token-portal/me?page=1`：返回 `balance_micros`、`free_models`、`max_concurrency`、模型目录和价格、用量汇总及分页日志。page 为 1–1000000；`start_time` / `end_time` 为 Unix 秒，反向或非法范围返回 400；响应为 `Cache-Control: no-store`。

管理和注册入口不再使用 `token_quota`、`allowed_models` 或旧注册模型白名单。门户响应不再提供 `token_quota`、`remaining_tokens`、`allowed_models`。数据库中的旧列仅为兼容旧数据保留，不参与准入，也不会转换成免费授权。

启动时增量迁移保留原 Key、余额、累计用量和启停状态；旧的空余额转换为 0，免费名单默认空。并发名额保存在进程内，适用于当前单实例部署。多实例共享限额需要独立的分布式计数方案。

这些规则适用于用户 Token，包括管理员创建的用户 Key；全局代理主 Key 保持原有权限。生产环境应开启代理鉴权；关闭鉴权会允许不提供 Key 的请求。已提供的有效用户 Key 即使在 auth Off 模式下仍执行其计费限制。

累计用量优先使用响应 usage，缺少输入统计时沿用已有估算。中断流、缺失 usage 或进程意外退出可能无法获得完整实际用量，因此不等同于上游账单。数据库入账失败会在当前进程内暂停该 Key 后续请求（503 `token_accounting_unavailable`）；重启不能恢复遗漏用量。

## 验证与部署

`deploy/stellar/smoke.py` 在隔离实例中检查默认余额、免费授权、未定价拒绝、余额不足、目录过滤、余额编辑和鉴权隔离。它直接复用服务器部署助手的门户响应校验，避免接口更新后两套检查不同步。不得将该冒烟脚本用于生产数据。

部署助手只读校验实际镜像、健康状态、公开 HTTPS、鉴权和现有用户门户，并检查 `balance_micros`、`free_models`、`max_concurrency` 数据库列。更新仓库中的 `deploy.py` 后，必须同步安装服务器上的 `/usr/local/sbin/stellar-ai-deploy`。

部署前备份镜像引用、Compose 和数据目录。自动回滚恢复旧镜像并保留数据库；回滚后的计费行为取决于该旧版本，不应通过删除兼容列或清零用量来处理。完整快照恢复需要单独评估。

### Model discovery, testing, selection and pricing

`proxy.only_raw_quota_models` defaults to `true`. In this mode, **API Proxy →
Supported Models & Integration → Fetch Models** opens the model manager:

1. Choose **Antigravity** or **WorkBuddy** and fetch candidate IDs. Discovery does
   not prove permission and does not run inference.
2. Select candidates and test individually or in a batch. Checks consume upstream
   quota. A successful test never adds a model automatically.
3. Select successful models and click **Add Selected**. Added IDs become available
   to administrators for model pricing, including models not yet priced.
4. Use **Model Pricing** to assign per-call or per-token prices. Only added,
   currently verified models matching a price rule appear in the public protocol
   model-list APIs and the usage portal. Zero prices and wildcard price rules are
   supported. A per-key free grant alone does not make an unpriced model visible.

Selection is stored in `published_models.json` under the data directory, with one
provider per model ID. Remove an ID before adding it from another provider.
Existing accounts require verification and explicit selection when upgrading;
there is no automatic import from the upstream catalog. Configure selections and
prices before serving traffic. Removing a selection does not erase price rules.
Disabling the setting restores the legacy expanded catalog/routing behavior;
public lists still require pricing. Settings apply without a restart.

The headless admin API uses the same workflow (admin authentication required):

- `POST /api/model-catalog/candidates` with
  `{"provider":"antigravity","refresh":true}` fetches candidates. Omit `refresh`
  or use `false` to read cached results without contacting upstream.
- `POST /api/model-catalog/test` with
  `{"provider":"antigravity","model_ids":["exact-upstream-id"]}` starts tests.
  Poll `GET /api/model-catalog/test?provider=antigravity` for progress.
- `POST /api/model-catalog/selection` with
  `{"provider":"antigravity","add":["exact-upstream-id"],"remove":[]}` changes
  the manual selection; unverified additions are rejected. Use `workbuddy` as the
  provider for WorkBuddy accounts.
- `GET /api/user-token-models` returns the admin's added IDs, including unpriced
  IDs. Existing model-pricing endpoints then configure prices.

Jobs are limited to 500 account/model pairs, one inference at a time, 30 seconds
per probe and 30 minutes overall. Antigravity results persist after each probe;
WorkBuddy saves completed accounts. Account credentials/project changes invalidate
Antigravity evidence. Token refresh and quota sync preserve newer verification
results. Provider selection prevents success on one provider being treated as
proof for another, and inference only selects verified eligible accounts.

Generated output establishes point-in-time support, not a guarantee of future
capacity or every optional feature. Explicit model permission/not-found errors
revoke support; throttling, quota exhaustion, transport errors and request
parameter errors remain inconclusive and preserve earlier successes. Exact mode
keeps upstream IDs without automatic version/model substitution. Unselected or
unsupported IDs return HTTP 400 `model_not_supported` with a link-free instruction
to check Available Models, before billing or inference.

Antigravity penalty compatibility is shared by all incoming protocols: zero
`presencePenalty` / `frequencyPenalty` values are omitted. If upstream returns
HTTP 400 with `Penalty is not enabled for this model`, the gateway removes only
these two parameters and retries once on the same endpoint, model and account.
Nonzero penalty preferences are ignored only after this explicit rejection;
other errors and unrelated generation settings are preserved. This does not
claim to fix every HTTP 400 or make an unsupported model usable.
