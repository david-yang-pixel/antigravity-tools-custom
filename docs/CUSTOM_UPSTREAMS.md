# 自定义 OpenAI 兼容上游（Base URL + API Key）

除内置的 Antigravity 和 WorkBuddy 外，可以添加任意数量的 OpenAI 兼容上游（OpenAI 官方、各类中转/聚合服务、DeepSeek、vLLM、Ollama 等）。每个上游都是模型目录中的一个独立来源，模型的获取、测试、上架、别名和计费与内置来源完全一致。

## 使用

1. 「API 反代 → 模型配置 → 高级 → 来源配置 → OpenAI 兼容上游」点击「添加上游」，填写：
   - **名称**：仅用于显示，可随时修改。
   - **Base URL**：服务 `/chat/completions` 与 `/models` 的 API 根路径，例如 `https://api.example.com/v1`。粘贴完整的 `…/v1/chat/completions` 也会自动去掉后缀。
   - **API Key**：以 `Authorization: Bearer <key>` 发送；本地无鉴权服务可留空。保存后不再回显，只显示末四位；留空保存表示保持原密钥。
   - **手动声明模型**（可选）：上游没有 `/models` 列表时填写模型 ID。
2. 保存后，在「获取上游模型」中选择该上游：读取 `/models` 并合并手动声明的模型（仅元数据，不推理）。
3. 对候选模型「测试」（发送一次真实的基础文本请求，可能产生费用），再「添加」上架为公开模型名。只有上架后的模型名才允许调用。

上游的 ID 由首次填写的名称生成（如 `my-relay`），之后不再改变；来源 ID 为 `openai-compatible:<id>`。仍有上架模型绑定的上游不能删除，需先在目录中移除这些上架记录。停用上游会保留上架记录，但其模型请求返回 503。

## 协议与限制

- 客户端可通过 Chat Completions（`/v1/chat/completions`）、Responses（`/v1/responses`，HTTP）和 Claude Messages（`/v1/messages`）调用，网关统一转换为 Chat Completions 发往上游。Gemini 原生接口、图片生成、音频及 Responses WebSocket 不支持这类来源。
- 上游请求固定为流式并带 `stream_options.include_usage`，非流式客户端得到聚合结果；若上游忽略流式直接返回 JSON，也会被正确转换。
- 支持文本、图片 URL / base64、函数工具（含 Responses `custom` / `namespace` 工具），以及 Chat `response_format` 和 Responses `text.format`（JSON object / JSON schema）。`developer` 角色转为 `system`，Claude `thinking` 参数不转发。
- 用量按上游返回的 `usage` 计入统一计费；Token 统计中按 `<id>@openai-compatible` 归属到每个上游。
- 出站请求只遵循全局「上游代理」设置（不读取 `HTTP(S)_PROXY` 环境变量）；不跟随重定向，避免把 API Key 发往其他主机。
- 上游的 401/403 返回 502（不回显上游原文），429 原样返回 429，400/404/413/422 原样返回状态码并附带经脱敏的上游错误信息。

## Headless / Docker

配置保存在数据目录的 `custom_upstreams.json`，可直接编辑（手写的明文 API Key 会被接受，并在下次从界面保存时加密）：

```json
{
  "upstreams": [
    {
      "id": "my-relay",
      "name": "My relay",
      "base_url": "https://api.example.com/v1",
      "api_key": "sk-...",
      "enabled": true,
      "models": ["gpt-4o-mini"]
    }
  ]
}
```

也可以用环境变量 `ABV_CUSTOM_UPSTREAMS` 传入同样结构的 JSON **数组**（即上面 `upstreams` 的值）。设置该变量后它完全取代文件中的列表，界面只读。

管理 API：`GET /api/custom-upstreams` 返回列表（不含密钥），`POST /api/custom-upstreams` 以 `{"upstreams":[…]}` 整体替换；条目省略 `api_key` 表示保留原密钥，`"api_key": ""` 表示清除。
