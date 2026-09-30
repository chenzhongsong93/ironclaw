# WebChat v2 历史工具结果只读契约

`GET /api/webchat/v2/threads/{thread_id}/runs/{run_id}/results?result_ref=...&project_id=...` 通过现有 bearer 认证、只读限流与 `RebornServicesApi` 门面提供历史完整工具结果。查询参数 project_id 与 timeline/run-state 的既有项目选择契约一致：参数只能选择 canonical thread 的项目作用域，不能授予线程所有权。

门面验证 typed thread/run/result_ref，并通过现有 session thread owner scope（含既有 automation owner fallback）读取历史。真实项目作用域必须与所选项目一致；结果引用必须绑定同线程、同 run 的 finalized、未删改、completed `CapabilityDisplayPreviewEnvelope`，且 preview/result_ref 与 transcript/result_ref 一致。任意 ref、错 run、错项目、他人线程均不读取正文。底层 opaque 持久结果按 24 KiB 窗口读取，校验稳定总长度与连续 offset；超过 4 MiB、缺失或不完整结果明确失败。

响应字段为 `result_ref`、`run_id`、`invocation_id` 和 `content`。content 为完整结果经安全脱敏后的 JSON 字符串。结构化脱敏递归处理对象字段、数组、字符串中的嵌套 JSON；凭据字段与安全规则命中的文本仍脱敏。嵌套 JSON 保持合法语法，不恢复被旧预览隐藏的敏感值，不从预览推断持久正文。

该 API 不执行工具，不修改 thread/run 或持久 schema。旧结果和旧客户端兼容；未持久化的 inline-only 结果返回不可用。可回退到旧 gateway 镜像，历史 opaque 数据不受影响。
