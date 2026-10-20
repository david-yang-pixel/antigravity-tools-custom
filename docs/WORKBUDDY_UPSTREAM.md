# 内置 WorkBuddy 账号与模型接入

WorkBuddy 账号管理、令牌刷新、模型同步和推理转发均内置在当前 Rust 后端中。桌面版和 headless / Docker 使用同一实现，不需要安装、启动或连接独立的 WorkBuddy Manager。

## 使用

1. 打开「账号管理 → WorkBuddy → 添加账号」，选择国内版或国际版，打开官方授权页面完成登录。当前页面会自动收取授权结果并同步模型。
2. 也可以导入自己的 WorkBuddy auth JSON。支持 `{ "account": { "uid": "…", "nickname": "…" }, "auth": { "accessToken": "…", "refreshToken": "…", "expiresAt": 1790000000, "realm": "cn" } }`，以及相同字段的扁平格式（camelCase 或 snake_case）。国际版使用 `realm: "global"`。`expiresAt` 是 Unix 秒；缺省时必须有有效 refreshToken 才能刷新。
3. 首次同步且尚无配置时，使用原生模型 ID（如 `glm-5.3-flash`）生成同名候选路由并启用 WorkBuddy，保持现有调用行为。在「API 反代 → 模型配置 → WorkBuddy 模型路由」可点击「获取可用模型」，对当前调度范围内的启用账号逐个发送短请求，获取后的结果列表只展示返回完整文本的模型。已有配置保留管理员的启停选择、自定义名称和模型价格。
4. 在「API 反代」启动服务。无需 Google 账号即可使用已启用的 WorkBuddy 路由。
5. 在「用户管理 → 模型定价」设置**客户端请求的模型名**的价格。所有下游用户 Key 默认按人民币余额和 token / 按次价格计费；该 Key 勾选的免费模型免扣余额，其他未定价模型禁止调用。`usage.credit` 不参与计费，也不向下游透传。

账号支持启停、删除、手动刷新凭据及同步模型。请求前自动刷新即将到期的凭据。同一账号最多两条并发请求，优先选择空闲账号；错误后短暂冷却。不会把失败请求自动切换到另一个账号重新生成。停用或删除不打断已经开始的请求，保留已有路由；没有匹配的可用账号会明确报错。

## 获取可用模型

上游目录不等于当前账号的使用权限。验证结果按账号保存，调度排除确认不可用的账号/模型组合；固定账号模式只验证并展示该账号的模型。明确不可用的旧路由保留配置以便重新验证，但不展示、不选用，也不回退到其他供应商。超时、网络错误等未能完成验证的结果不作为禁止调用的依据。

「获取可用模型」在后台执行，可离开页面后查看进度。每次最多验证 500 个账号/模型组合，串行执行；单次生成最多 512 tokens、30 秒，整个任务最多 30 分钟。验证占用正常账号池并发槽并尊重冷却，不计入调用健康度、不扣下游余额，但可能消耗上游积分。中断或超时的模型不算验证成功；错误和未验证数量单独展示。

**升级不会要求旧账号先手动验证才能调用；同一 realm + uid 重新授权时保留模型验证结果。** 从未实测的目录沿用现有路由，并在界面标明“待验证”；主动获取后的结果列表只展示验证通过的模型。定时同步只更新候选目录和积分，不自动生成请求，不重新加入被拒绝的模型。已验证表示最近一次短文本调用成功，不能保证之后的余额、权限、网络或工具/图片能力；账号套餐变化或调用异常后应重新获取。

## 账号纳管

新增及已有账号默认自动纳管，后台任务在桌面版和 headless 启动时运行，无需保持浏览器打开。可以单独关闭自动纳管、自动签到或自动保活；停用账号会暂停后台任务。

- 每 15 分钟同步候选模型目录和上游可用积分。积分查询失败保留上次结果并标明时间和错误，不把未知余额当作零。
- 国内账号按北京时间每日签到；重复签到返回“今日已签到”，失败间隔至少一小时重试。国际版没有该签到接口，明确显示不支持。
- 每天主动刷新凭据保活，并在到期前 10 分钟续期。刷新失败至少间隔 5 分钟重试，不因一次网络错误自动删除账号。请求时刷新仍是最后的保护。
- 展示凭据有效期、最近成功保活、签到状态、下次同步及有余额套餐的最近到期时间。积分仅供账号管理，不改变下游人民币账本。
- 调用健康度统计本次服务启动以来已完成的真实推理成功/失败；尚未调用显示待验证。客户端主动中断不算成功或账号故障。上游失败按统一熔断器退避阶梯冷却（优先尊重更长的 Retry-After，安全上限 24 小时），界面显示倒计时及原因。重启后调用统计及短期冷却重置，签到日期和保活结果持久保存。

`POST /api/workbuddy-accounts/update` 支持 `managed`、`auto_checkin`、`keepalive` 布尔字段；`checkin: true` 立即签到，`refresh: true` 立即保活并同步。只传 `id` 会同步模型及积分，不主动签到。

### 国内版活动奖励

展开账号行中的「活动奖励」，可查询任务、连续登录奖励、旅行到站奖励和免费抽奖次数。「领取可领奖励」只领取上游明确标记已达标的奖励；不接受或代做任务，不上报任务事件，不出发旅行，也不购买抽奖次数。每日签到仍沿用上面的独立开关。

- 「自动领取奖励」和「自动免费抽奖」各自独立，新增及旧账号均默认关闭。免费抽奖开关同时控制手动领取时是否抽奖；未打开时手动领取也不会抽奖。国际版明确显示不支持。
- 启用账号、自动纳管及相应活动开关后，每小时检查一次。关闭纳管或停用账号暂停自动活动；运行中关闭活动开关会在下一次操作前生效，已发出的请求仍会记录结果。
- 查询和领取在后台执行，页面通过账号列表轮询进度。最多同时处理两个账号，同一账号不排队重复执行；每次最多三分钟、20 项奖励及 5 次抽奖请求。手动领取间隔至少 30 秒，查询不推迟自动领取时间。
- 展示最近查询、领取尝试、下次执行、活动状态及最近 50 条领取记录。上游没返回的奖励金额显示未知，不根据余额差值推算。领取后刷新上游积分，所有活动均不改变下游模型价格或人民币账本。
- 执行记录和待确认抽奖请求写入加密账号文件。重启或中断后显示未完成，可再次查询/领取。抽奖响应丢失时复用原请求标识核验，包括剩余次数已经为零的情况；无法确认仍显示未确认，不当作成功，也不换新标识再次抽奖。

Headless 与桌面端使用同一后台任务。管理员可向 `POST /api/workbuddy-accounts/update` 发送 `{ "id": "账号 ID", "auto_rewards": true }` 或 `auto_lottery: true` 保存开关；`{ "id": "账号 ID", "activity": "query" }` 只查询活动，`activity: "claim"` 立即领取。返回账号的 `management.activity` 提供进度、活动和历史记录，不返回内部抽奖请求标识。

活动协议参考下方固定版本源码；上游活动可能结束或变化。接口不支持、身份失效及未知响应会分别保留状态，不把上游目录或 HTTP 200 当作奖励到账证明。自动化回归使用模拟上游，真实活动领取仍需用有资格的国内账号验证。

## 调度与会话

「API 反代 → 账号轮换与会话调度」的调度模式和熔断配置对两个账号池生效，保存后热更新，不必重启：

- 缓存优先：同一会话保持账号，繁忙或冷却时按最大等待时长等待，超时返回不可用。0 秒表示不等待；最多 300 秒，且受请求超时约束。
- 平衡轮换：优先复用会话账号；新请求发现该账号繁忙或冷却时选择其他可用账号。
- 性能优先：不绑定会话，按并发负载及最近使用顺序分配账号。
- WorkBuddy 固定账号在同一面板独立选择；选定后不使用其他 WorkBuddy 账号，账号停用、删除或不支持目标模型时明确报错。选择“自动调度”取消固定。
- “清除会话绑定”和“清除限流记录”同时处理两个账号池。熔断启停及退避阶梯应用于 WorkBuddy；Antigravity 专用的 5h / 周配额锁定规则不映射为 WorkBuddy 积分。

会话标识由下游 Token 身份和规范化后的第一条用户消息哈希组成，并按目标模型隔离；空闲一小时失效，最多保留一万条。失败生成不重放；平衡模式切换作用于后续请求。已经开始的请求保留当时的调度配置。

排队、等待凭据及生成共用请求截止时间。请求超时后释放账号并发槽；已开始的凭据刷新仍完成保存，避免上游已轮换的 refreshToken 丢失，刷新本身最多等待 25 秒。

Headless 使用 `gui_config.json` 中 `proxy.scheduling` 和 `circuit_breaker`；固定 WorkBuddy 账号保存在 `workbuddy.json` 的 `preferred_account_id`，或通过 `POST /api/workbuddy-settings` 发送 `{ "preferred_account_id": "账号 ID" }` 设置，发送 `null` 取消。部分更新不覆盖已有模型路由。

## 配置、存储和 headless 接口

数据目录沿用 `ABV_DATA_DIR`。文件：

- `workbuddy.json`：`enabled`、`models` 和 `preferred_account_id`（对外模型名到原生 ID 的 JSON 对象）。旧的外部 Manager `base_url` / `api_key` 字段不再使用，下次保存配置时移除。
- `workbuddy_accounts.json`：整个账号集合经 AES-256-GCM 加密后的信封。
- `workbuddy_accounts.key`：随机生成、持久保存的加密密钥（Unix 权限 0600）。**备份或迁移时必须同时保存此文件与账号文件**；Docker 必须持久化整个数据目录。密钥不依赖容器 ID。

可覆盖 `ABV_WORKBUDDY_ENABLED=true|false` 和 `ABV_WORKBUDDY_MODELS='{"glm-5.3-flash":"glm-5.3-flash"}'`。被覆盖的字段在界面锁定。旧 `ABV_WORKBUDDY_BASE_URL` 和 `ABV_WORKBUDDY_API_KEY` 已无作用。

所有管理接口使用当前服务的管理员鉴权（`Authorization: Bearer <管理员凭据>`），普通下游用户 Key 无权限。客户端和返回值均不读取或返回已保存的 accessToken / refreshToken。

| 接口 | 请求 |
| --- | --- |
| `GET /api/workbuddy-settings` | 读取路由、账号数、当前调度范围内已验证模型 |
| `POST /api/workbuddy-settings` | `{ "enabled": true, "models": { "native-id": "native-id" } }（字段可单独更新）` |
| `POST /api/workbuddy-models/verify` | 启动后台可用性验证；正在执行时返回同一任务状态，不重复启动 |
| `GET /api/workbuddy-models/verify` | 读取进度、结果数量及错误；任务进度仅保留在当前进程，账号验证结果持久保存 |
| `GET /api/workbuddy-accounts` | 账号列表，无凭据 |
| `POST /api/workbuddy-accounts` | `{ "auth": <自己的 auth JSON> }`，导入并同步模型 |
| `POST /api/workbuddy-accounts/update` | `{ "id": "…", "enabled": false }` 启停；仅 id 同步模型；`refresh: true` 先刷新凭据 |
| `POST /api/workbuddy-accounts/delete` | `{ "id": "…" }` |
| `POST /api/workbuddy-login/start` | `{ "realm": "cn" }` 或 global，返回短期授权链接和会话 id |
| `POST /api/workbuddy-login/poll` | `{ "id": "…" }`，返回 waiting / expired / ready |
| `POST /api/workbuddy-login/cancel` | `{ "id": "…" }` |

headless 授权可通过这些接口发起，然后在任意浏览器打开返回的官方链接，完成后轮询并调用账号同步接口。授权会话最多保留 10 分钟；取消后不能继续导入。公开网站请继续使用现有 HTTPS 和管理员鉴权。

## 协议范围

支持 Chat Completions、HTTP Responses（含 `/responses` 别名）和 Anthropic Messages 的 JSON 与 SSE。腾讯原生接口为 Chat SSE；边界适配器完成参数、工具调用、图片内容及响应事件转换，共享中间件继续执行鉴权、模型权限、并发准入、日志和扣费。Google 专用 Gemini 流水线保持原路径。

- 支持文本、图片输入、函数工具、多轮工具结果、推理文本和缓存 token 用量。
- Responses 需要发送完整对话历史。不支持 `previous_response_id`、服务端 conversation / background、内置搜索等 hosted tools、custom tools 或 JSON schema 输出；明确返回参数错误，不会悄悄丢弃历史。函数工具可用。
- 不支持 WorkBuddy 的 Gemini 原生、WebSocket、旧 Completions、独立计数、图片生成和音频接口。Gemini 模型列表不展示 WorkBuddy 模型。
- 主动获取后的可用目录只展示经短文本请求验证通过的腾讯模型；尚未实测的旧目录单独标明待验证。用户请求缺少首条 system 消息时补入中性提示，保留已有 system / developer 内容以满足原生接口要求。
- `prompt_tokens` 已包含缓存命中，不重复累加；`prompt_cache_hit_tokens` 规范化为缓存读取量。没有真实 usage 或中断时，受共享监控现有估算和已收到内容范围限制。
- 流中途报错或未完成就断开时，调用日志和结算标记失败，按次价格不扣费；已采集的用量仍按现有 token 价格规则结算。响应头已经发出时，客户端收到的是流读取错误，原 HTTP 200 无法改写。

仅向固定腾讯官方 HTTPS 地址发送 WorkBuddy 凭据，不传递下游 Key、Cookie、用户 IP；禁止重定向。生成请求不自动重试网络失败。仅 `/v2/chat/completions` 在生成前明确返回渠道错误码 `11128` / `11-128` 时，尝试同一官方服务的 `/console/chat/completions` 兼容端点；普通 403、404、429 或已经开始的流不会重放。

## 验证和回退

本地测试覆盖加密持久化、重新授权、过期刷新并发、账号池释放、三种协议的 JSON/SSE 与真实网关账本结算。测试上游为模拟服务，不能证明某个真实账号的模型授权。首次添加真实账号后，仍需确认模型同步和一次真实模型调用。

关闭 WorkBuddy 路由即停止新的推理请求；其他上游不受影响。后台纳管独立运行，可停用账号或取消“自动纳管”暂停。回退代码前备份上述三个文件。不要使用旧的外部 Manager 版本读取新的账号文件。

WorkBuddy 配置损坏时，可识别的 WorkBuddy 模型返回配置错误，其他供应商继续原路径。判别使用当前进程最后有效的路由、可读的路由键和已同步的账号模型；配置完全损坏后重启且没有这些证据时，无法恢复丢失的自定义别名，应从备份恢复配置。

## 来源

原生端点、字段和兼容约定参考 [WorkBuddy Manager](https://github.com/ithtelab/workbuddy-manager/tree/ec7eccba04106e2f75b054cd17b175313704f3e2) 的 `server/services/tencent.py`、`realm.py`，以及该项目 [v2.6.3 release](https://github.com/ithtelab/workbuddy-manager/releases/tag/v2.6.3) 附带的 workbuddy2api `internal/upstream` / `internal/auth` 源码。当前实现为 Rust 原生集成，没有嵌入或启动其 Python / Go 服务。相关 MIT 许可保留于 [WORKBUDDY_NOTICES.md](WORKBUDDY_NOTICES.md)。
