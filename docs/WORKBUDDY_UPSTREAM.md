# 内置 WorkBuddy 账号与模型接入

WorkBuddy 账号管理、令牌刷新、模型同步和推理转发均内置在当前 Rust 后端中。桌面版和 headless / Docker 使用同一实现，不需要安装、启动或连接独立的 WorkBuddy Manager。

## 使用

1. 打开「账号管理 → WorkBuddy → 添加账号」，选择国内版或国际版，打开官方授权页面完成登录。当前页面会自动收取授权结果并同步模型。
2. 也可以导入自己的 WorkBuddy auth JSON。支持 `{ "account": { "uid": "…", "nickname": "…" }, "auth": { "accessToken": "…", "refreshToken": "…", "expiresAt": 1790000000, "realm": "cn" } }`，以及相同字段的扁平格式（camelCase 或 snake_case）。国际版使用 `realm: "global"`。`expiresAt` 是 Unix 秒；缺省时必须有有效 refreshToken 才能刷新。
3. 首次同步且尚无配置时，使用原生模型 ID（如 `glm-5.3-flash`）生成同名候选路由并启用 WorkBuddy，保持现有调用行为。候选模型来自目录同步，不再向上游发送固定短句做可用性探测。已有配置保留管理员的启停选择、自定义名称和模型价格。
4. 在「API 反代」启动服务。无需 Google 账号即可使用已启用的 WorkBuddy 路由。
5. 在「用户管理 → 模型定价」设置**客户端请求的模型名**的价格。所有下游用户 Key 默认按人民币余额和 token / 按次价格计费；该 Key 勾选的免费模型免扣余额，其他未定价模型禁止调用。`usage.credit` 不参与计费，也不向下游透传。

账号支持启停、删除、手动刷新凭据及同步模型。请求前自动刷新即将到期的凭据。同一账号最多两条并发请求，优先选择空闲账号；错误后短暂冷却。已经收到上游 HTTP 响应的失败（含鉴权失败、渠道校验、429 和内容拒绝）停在当前账号，不换号重放。只有在还没收到 HTTP 响应的连接失败时，才会再试一个账号。停用或删除不打断已经开始的请求，保留已有路由；没有匹配的可用账号会明确报错。

### 把国内版与国际版 ID 绑定到同一个公开模型

国内版与国际版常用不同的原生 ID（如 `claude-sonnet-4-6` 与 `claude-sonnet-4-6-sg`）。在模型发布里把它们绑定到同一个公开名后，客户端只需请求一个名字：

- 第一个 ID 是主 ID（`native_model`），其余写入 `extra_native_models`。所有绑定的 ID 共用同一个账号池，按现有调度策略平等调度，不分主次。
- 网关把每个请求发给被选中账号自己支持的那个原生 ID；账号必须在模型目录里列出该 ID。
- 计费、统计和对外模型名始终使用公开名，不使用原生 ID。
- headless 配置：在发布条目上增加同名 JSON 字段 `"extra_native_models": ["claude-sonnet-4-6-sg"]`。Headless / Docker 管理员可通过管理 API `save_model_registry`（`POST /api/model-registry`）提交，字段与下例相同；也可直接编辑 `model_registry.json`。不使用该字段时文件与行为完全不变。
- 回退与降级：去掉字段（保存发布时不带额外 ID）后只使用主 ID。**降级到不认识该字段的旧版本之前必须先清空所有发布的额外 ID**：旧版本会拒绝带有 `extra_native_models` 的 `model_registry.json`，注册表加载失败，所有推理请求都会被拒绝。从未使用该字段的注册表不受影响。

```json
{
  "public_id": "wb-sonnet",
  "source_id": "workbuddy:default",
  "native_model": "claude-sonnet-4-6",
  "extra_native_models": ["claude-sonnet-4-6-sg"]
}
```

## 获取可用模型

上游目录不等于当前账号的使用权限。验证结果按账号保存，调度排除确认不可用的账号/模型组合；固定账号模式只验证并展示该账号的模型。明确不可用的旧路由保留配置以便重新验证，但不展示、不选用，也不回退到其他供应商。超时、网络错误等未能完成验证的结果不作为禁止调用的依据。

「获取可用模型」不再向上游发送生成的验证短句。目录同步只更新候选列表；某个模型是否可调用，以下游真实请求的结果为准。

模型管理「上游模型」里的 WorkBuddy「测试」只发送管理员当场填写的系统提示和用户消息（每项必填，最多 16 KiB），通过列出该模型的账号发起一次真实请求，不使用固定短句，也不附加 `max_tokens`；system 开头命中第三方 agent 特征时，与真实请求一样会在最前面加一行中性文本。接口为 `verify_model_candidates` 带 `prompt: { "system": "…", "user": "…" }`；WorkBuddy 候选缺少 `prompt` 时直接拒绝，其他来源带 `prompt` 也会拒绝，WorkBuddy 与其他来源不能放在同一次测试里。测试内容只随这一次请求发送，服务端不保存；浏览器在本地记住上次填写的内容。

测试结果：调用通过记为已验证；上游明确返回无权限或模型不存在（11158 / 11102）记为不可用；其他失败只在本次结果里显示，不写入记录，也不撤销已有证据。旧版本固定短句探测留下的「验证失败」记录不再显示，这些模型显示为「未验证」。

**升级不会要求旧账号先手动验证才能调用；同一 realm + uid 重新授权时保留已有的模型验证结果。** 从未实测的目录沿用现有路由，并在界面标明“待验证”。定时同步只更新候选目录和积分，不自动生成对话请求，不重新加入被拒绝的模型。历史验证记录只说明当时的短文本调用结果，不能保证之后的余额、权限、网络或工具/图片能力。

## 账号纳管

新增及已有账号默认自动纳管，后台任务在桌面版和 headless 启动时运行，无需保持浏览器打开。自动签到默认关闭；以前随默认打开的签到不会继续执行，需要管理员重新打开该开关。可以单独关闭自动纳管或自动保活；停用账号会暂停后台任务。

- 候选模型目录和上游积分只在该账号最近约 6 小时内有过成功的真实生成后才后台同步，间隔约 15 分钟并带小幅抖动。没有调用的账号不再周期性访问上游。管理员手动同步不受这个限制。积分查询失败保留上次结果并标明时间和错误，不把未知余额当作零。
- 国内账号在管理员打开自动签到后，仅近 6 小时有真实推理调用时，按北京时间的固定白天时段（约 09:00–21:00）签到。闲置账号不会被签到任务唤醒。成功或上游确认“今日已签到”后，当天不再请求，包括手动重复操作。国际版没有该签到接口。手动签到不要求打开自动开关，但账号必须启用，并与自动任务共用以下持久化保护：每日最多 3 次尝试；失败等待约 60–80 分钟后重试，第二次失败的等待加倍；上游 `Retry-After` 支持秒数和 HTTP 日期，取更长的等待时间，跨天也不缩短。尝试在凭据准备和网络请求之前记入加密账号文件，重启、取消、重新授权或切换开关不会清空次数和等待时间。
- 签到的 HTTP 401 暂停签到，重新授权后才可恢复；其他 4xx（除 408、429）及未知业务拒绝暂停签到，管理员检查错误后在账号详情选择“恢复签到”。408、429、5xx 和连接/响应异常受次数及退避限制。暂停只针对签到，不把业务拒绝推断为账号封禁，也不修改推理权限。签到失败后同一次任务不再继续请求模型和积分。恢复操作不发网络请求，不清空当日次数或缩短冷却时间。
- 保活只在令牌进入到期前约 30 分钟的窗口时刷新，不再每天额外刷新一次。推理热路径仅在令牌已过期或剩余不足约 30 秒时才阻塞刷新，有效期内的请求可顺带触发后台续期（不阻塞首包）。刷新失败至少间隔 5 分钟重试，不因一次网络错误自动删除账号。请求时刷新仍是最后的保护。
- 每个账号持久保存机器码、会话号和客户端版本；新账号可通过 auth JSON 同级的 `client` 对象导入 `product_version`、`cli_version`、`machine_id`、`session_id`，缺失的机器码/会话号本地生成并保持稳定。`deviceToken` / `device_token` 仅接受真实客户端导出的不透明值，支持 auth 对象内或顶层字段；重新授权时可更新该值。旧版本生成的 UUID 占位设备令牌会被清除，不再生成代替 Turing SDK 输出。不再使用由账号 ID 哈希出来的公开派生值，也不再按账号散开产品版本。未配置真实出站代理时，上游仍看到同一出口 IP，但每个账号使用自己的 HTTP 客户端，不与其他账号共用一条连接。启用出口后，每个账号独占订阅里的一个节点，已绑定的节点不会因为失败而改绑，也不会再分给别的账号。出口地址来自该节点，程序不分配公网 IP。默认出站改为官方 Linux 5.6.2 自带 Electron/Node fetch 的 HTTP/1.1 配置，有本地 ClientHello 对照；账号与节点共同决定连接缓存，改绑节点后新请求重建连接。签到/积分使用单独的 AuthService 头集，不附带推理会话、机器码、Origin 或 CLI 头。取证、版本、可复现方式与尚未对齐的 Axios ALPN 差异见 [客户端对齐记录](WORKBUDDY_CLIENT_PROFILE.md)。紧急回退可设置 `ABV_WORKBUDDY_LEGACY_CHROME=true` 并重启，失败不会自动切换协议重放。
- 展示凭据有效期、最近成功保活、签到状态、下次同步及有余额套餐的最近到期时间。积分仅供账号管理，不改变下游人民币账本。
- 调用健康度统计本次服务启动以来已完成的真实推理成功/失败；尚未调用显示待验证。客户端主动中断不算成功或账号故障。上游失败按统一熔断器退避阶梯冷却（优先尊重更长的 Retry-After，安全上限 24 小时），界面显示倒计时及原因。上游 HTTP 429 只冷却该账号上的这个模型，同账号的其他模型照常调度，界面在健康度下单独显示「某模型被限流」倒计时。重启后调用统计及短期冷却重置，签到日期和保活结果持久保存。

`POST /api/workbuddy-accounts/update` 支持 `managed`、`auto_checkin`、`keepalive` 布尔字段；`checkin: true` 在上述保护允许时立即签到（不附带模型/积分查询），`resume_checkin: true` 仅解除需要人工检查的签到暂停（401 必须重新授权），`refresh: true` 立即保活并同步。只传 `id` 会同步模型及积分，不主动签到。

### 国内版活动奖励

展开账号行中的「活动奖励」，可查询任务、连续登录奖励、旅行到站奖励和免费抽奖次数。「领取可领奖励」只领取上游明确标记已达标的奖励；不接受或代做任务，不上报任务事件，不出发旅行，也不购买抽奖次数。每日签到仍沿用上面的独立开关。

- 「自动领取奖励」和「自动免费抽奖」各自独立，新增及旧账号均默认关闭。免费抽奖开关同时控制手动领取时是否抽奖；未打开时手动领取也不会抽奖。国际版明确显示不支持。
- 启用账号、自动纳管及相应活动开关后：真实推理成功时可在约 45 分钟冷却后顺带领取；后台兜底约每 2–4 小时检查一次（账号间错开），连续无可领项时拉长间隔（最长约 12 小时）。关闭纳管或停用账号暂停自动活动；运行中关闭活动开关会在下一次操作前生效，已发出的请求仍会记录结果。
- 查询和领取在后台执行，页面通过账号列表轮询进度。最多同时处理两个账号，同一账号不排队重复执行；每次最多三分钟、20 项奖励及 5 次抽奖请求。手动领取间隔至少 30 秒，查询不推迟自动领取时间。
- 展示最近查询、领取尝试、下次执行、活动状态及最近 50 条领取记录。上游没返回的奖励金额显示未知，不根据余额差值推算。领取后刷新上游积分，所有活动均不改变下游模型价格或人民币账本。
- 执行记录和待确认抽奖请求写入加密账号文件。重启或中断后显示未完成，可再次查询/领取。抽奖响应丢失时复用原请求标识核验，包括剩余次数已经为零的情况；无法确认仍显示未确认，不当作成功，也不换新标识再次抽奖。

Headless 与桌面端使用同一后台任务。管理员可向 `POST /api/workbuddy-accounts/update` 发送 `{ "id": "账号 ID", "auto_rewards": true }` 或 `auto_lottery: true` 保存开关；`{ "id": "账号 ID", "activity": "query" }` 只查询活动，`activity: "claim"` 立即领取。返回账号的 `management.activity` 提供进度、活动和历史记录，不返回内部抽奖请求标识。

活动协议参考下方固定版本源码；上游活动可能结束或变化。接口不支持、身份失效及未知响应会分别保留状态，不把上游目录或 HTTP 200 当作奖励到账证明。自动化回归使用模拟上游，真实活动领取仍需用有资格的国内账号验证。

## 调度与会话

「API 反代 → 账号轮换与会话调度」的调度模式和熔断配置对 Antigravity、WorkBuddy 和 Qoder 账号池生效，保存后热更新，不必重启：

- 缓存优先：同一会话保持账号，繁忙或冷却时按最大等待时长等待，超时返回不可用。0 秒表示不等待；最多 300 秒，且受请求超时约束。
- 平衡轮换：优先复用会话账号；新请求发现该账号繁忙或冷却时选择其他可用账号。
- 性能优先：不绑定会话，按并发负载及最近使用顺序分配账号。
- WorkBuddy 固定账号在同一面板独立选择；选定后不使用其他 WorkBuddy 账号，账号停用、删除或不支持目标模型时明确报错。选择“自动调度”取消固定。
- “清除会话绑定”和“清除限流记录”同时处理上述账号池。熔断启停及退避阶梯统一应用；Antigravity 专用的 5h / 周配额锁定规则不映射为其他来源的积分。

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
| `POST /api/workbuddy-login/start` | `{ "realm": "cn" }` 或 global，返回短期授权链接和会话 id；重新授权已有账号时传 `{ "realm": "cn", "account_id": "…" }`，realm 以该账号为准 |
| `POST /api/workbuddy-login/poll` | `{ "id": "…" }`，返回 waiting / expired / ready；拒绝导入时返回 wrong_account（重新授权时登录了别的账号）或 reauthorize_from_row（该账号已绑定其他节点） |
| `POST /api/workbuddy-login/cancel` | `{ "id": "…" }` |
| `GET /api/workbuddy-egress` | 出口设置、mihomo 状态、节点列表（含最近延迟）和账号绑定；订阅链接只返回域名 |
| `POST /api/workbuddy-egress` | `{ "enabled": true, "subscription_url": "https://…", "include": "", "exclude": "…", "auto_assign": true }`（字段可单独更新；`subscription_url: ""` 清除） |
| `POST /api/workbuddy-egress/refresh` | 立即重新拉取订阅 |
| `POST /api/workbuddy-egress/test-nodes` | 测试全部节点延迟 |
| `POST /api/workbuddy-egress/account` | `{ "id": "…", "node": "节点名" }` 固定节点；`"DIRECT"` 直连；`null` 交给自动分配 |
| `POST /api/workbuddy-egress/account/test` | `{ "id": "…" }`，沿该账号实际出口测到其 WorkBuddy 端点的延迟 |

## 账号出口节点（机场订阅）

启用后，每个账号的对话、凭据刷新、模型同步、积分、签到和登录请求都经由机场订阅中的一个节点发出。程序托管一个 mihomo 子进程：订阅作为 proxy-provider 由 mihomo 解析（支持其全部协议与订阅格式），每个账号一个 select 组，通过仅监听 `127.0.0.1` 的认证 SOCKS 入站按用户名（`IN-USER`）分流，其余流量 `REJECT`。

- 新账号或未绑定账号首次使用时自动分配一个尚未被占用的节点：符合 `include`、不符合 `exclude`，测速成功的优先，测速失败的跳过。已经分给其他账号、或正被一次未完成授权占用的节点不会再分。没有空闲节点时分配失败，不会把第二个账号塞进已有节点。账号授权全程使用同一节点，授权完成后账号沿用该节点。授权占用的节点在会话结束时释放；放弃的授权最迟在链接过期时自动释放，不会一直占着节点。
- 已有账号请在账号行点「重新授权」（或给 start 传 `account_id`）。授权直接走该账号已绑定的节点，节点全部占满时也能完成；未绑定的账号按新账号分配空闲节点。通过「添加账号」登录一个已绑定其他节点的已有账号，或重新授权时登录了另一个账号，都不会导入凭据，避免同一账号的令牌从另一个出口签发。
- 账号列表可为每个账号手动选择空闲节点、`直连` 或 `自动分配`，并测试该账号出口到其 WorkBuddy 端点的延迟。手动指定也不能把两个账号放到同一个节点。
- 绑定节点不可用时请求直接失败并切换账号，**不会改绑到别的节点，也不会回退直连**。出口地址是该节点当前的地址，由订阅提供；程序不分配公网 IP。节点换地址或几条线路共用一个地址，取决于订阅本身。未启用出口时，账号仍从这台服务器的地址出去。
- 配置保存在 `workbuddy_egress.json`（含订阅链接和绑定）；mihomo 工作目录为数据目录下 `egress/`（配置、日志 `mihomo.log`、订阅缓存）。
- 订阅链接、节点筛选和自动分配与 Qoder 共用，同一个 mihomo 进程同时服务两个来源；开关和节点绑定按来源独立。同一来源内一个节点只给一个账号，WorkBuddy 账号与 Qoder 账号可以使用同一节点（上游是不同服务）。Qoder 的绑定存于同一文件的 `pools.qoder`。
- 环境变量：`ABV_WORKBUDDY_EGRESS_ENABLED=true|false`、`ABV_EGRESS_SUBSCRIPTION=<订阅链接>`（仍兼容 `ABV_WORKBUDDY_EGRESS_SUBSCRIPTION`；被覆盖字段在界面锁定）、`ABV_MIHOMO_PATH=<可执行文件>`。Qoder 开关为 `ABV_QODER_EGRESS_ENABLED`。
- mihomo 查找顺序：`ABV_MIHOMO_PATH` / 配置文件 `mihomo_path` → `<数据目录>/egress/bin/mihomo` → 程序同目录 → `PATH`。Docker 镜像已内置固定版本并校验 sha256；管理接口不能修改可执行文件路径。

headless 授权可通过这些接口发起，然后在任意浏览器打开返回的官方链接，完成后轮询并调用账号同步接口。授权会话最多保留 10 分钟；取消后不能继续导入。公开网站请继续使用现有 HTTPS 和管理员鉴权。

## 协议范围

支持 Chat Completions、HTTP Responses（含 `/responses` 别名）和 Anthropic Messages 的 JSON 与 SSE。腾讯原生接口为 Chat SSE；边界适配器完成参数、工具调用、图片内容及响应事件转换，共享中间件继续执行鉴权、模型权限、并发准入、日志和扣费。Google 专用 Gemini 流水线保持原路径。

- 支持文本、图片输入、函数工具、多轮工具结果、推理文本和缓存 token 用量。
- Responses 需要发送完整对话历史。不支持 `previous_response_id`、服务端 conversation / background、内置搜索等 hosted tools、custom tools 或 JSON schema 输出；明确返回参数错误，不会悄悄丢弃历史。函数工具可用。
- 不支持 WorkBuddy 的 Gemini 原生、WebSocket、旧 Completions、独立计数、图片生成和音频接口。Gemini 模型列表不展示 WorkBuddy 模型。
- 候选目录来自模型同步。调用方没有提供 system 消息时直接拒绝，不代写固定提示词；已有的 system / developer 内容会保留，并在需要时移到首条。
- 国内网关会扫描每条 system 消息的开头，命中其「已知第三方 agent 提示」清单时返回 11128。发送前，网关检查每条 system 消息开头 512 个字符：某一行以 `You are` / `You're` / `Act as` / `你是` / `您是` 开场，或出现 `x-…billing…:`、`cc_version=` 这类客户端计费伪 Header，就在这条 system 消息最前面加一行中性文本 `Follow the instructions below.`。原文不删不改，未命中的消息逐字节不变，点名 WorkBuddy / CodeBuddy 的开头不处理。环境变量 `ABV_WORKBUDDY_SYSTEM_LEAD` 可以替换这行文本，设为空字符串则关闭此处理。腾讯的清单不公开，这是按常见 agent 开头做的广谱识别，不能保证覆盖全部。
- `prompt_tokens` 已包含缓存命中，不重复累加；`prompt_cache_hit_tokens` 规范化为缓存读取量。没有真实 usage 或中断时，受共享监控现有估算和已收到内容范围限制。
- 流中途报错或未完成就断开时，调用日志和结算标记失败，按次价格不扣费；已采集的用量仍按现有 token 价格规则结算。响应头已经发出时，客户端收到的是流读取错误，原 HTTP 200 无法改写。

仅向固定腾讯官方 HTTPS 地址发送 WorkBuddy 凭据，不传递下游 Key、Cookie、用户 IP；禁止重定向。生成请求只打 `/v2/chat/completions`。渠道校验、403、404、429 和已经开始的流都不会换路径或换账号重放。还没收到 HTTP 响应的连接失败可以再试一个账号。上游 HTTP 429 时，下游收到 HTTP 429 `workbuddy_upstream_rate_limited`，带 `Retry-After`，说明是模型额度或上游限流，与授权无关。同一模型在 120 秒内有 2 个不同账号收到相同结果码的 429（池里只有 1 个可用账号或固定账号时 1 次即可），网关判定为模型级限流并开启模型熔断：熔断期内该模型的请求在本地直接返回 HTTP 429 `workbuddy_model_rate_limited` 和 `Retry-After`，不再消耗其他账号；时长按熔断退避阶梯递增（默认 60 → 300 → 1800 → 7200 秒，上游 Retry-After 更长时取更长者）。到期后只放行一个探测请求（不等待会话绑定的账号，直接选空闲账号），上游接受即关闭熔断，再次 429 则进入下一档。所有可用账号都在冷却该模型时，同样返回 HTTP 429 `workbuddy_model_rate_limited`，`Retry-After` 为最早恢复的账号剩余时间；账号仅繁忙时仍返回 503 `workbuddy_account_unavailable`。关闭熔断器时不启用模型冷却与模型熔断；「清除限流记录」同时清除模型冷却与模型熔断。上游返回 11128（渠道校验）时，下游收到 HTTP 400 `workbuddy_channel_rejected`，说明是 system 提示开头命中了上游的第三方 agent 识别，需要调整提示开头；与账号、额度、网络无关，不冷却账号，换账号也不会改变结果。

## 验证和回退

本地测试覆盖加密持久化、重新授权、过期刷新并发、账号池释放、三种协议的 JSON/SSE 与真实网关账本结算。测试上游为模拟服务，不能证明某个真实账号的模型授权。首次添加真实账号后，仍需确认模型同步和一次真实模型调用。

关闭 WorkBuddy 路由即停止新的推理请求；其他上游不受影响。后台纳管独立运行，可停用账号或取消“自动纳管”暂停。回退代码前备份上述三个文件。不要使用旧的外部 Manager 版本读取新的账号文件。

WorkBuddy 配置损坏时，可识别的 WorkBuddy 模型返回配置错误，其他供应商继续原路径。判别使用当前进程最后有效的路由、可读的路由键和已同步的账号模型；配置完全损坏后重启且没有这些证据时，无法恢复丢失的自定义别名，应从备份恢复配置。

## 来源

原生端点、字段和兼容约定参考 [WorkBuddy Manager](https://github.com/ithtelab/workbuddy-manager/tree/ec7eccba04106e2f75b054cd17b175313704f3e2) 的 `server/services/tencent.py`、`realm.py`，以及该项目 [v2.6.3 release](https://github.com/ithtelab/workbuddy-manager/releases/tag/v2.6.3) 附带的 workbuddy2api `internal/upstream` / `internal/auth` 源码。当前实现为 Rust 原生集成，没有嵌入或启动其 Python / Go 服务。相关 MIT 许可保留于 [WORKBUDDY_NOTICES.md](WORKBUDDY_NOTICES.md)。
