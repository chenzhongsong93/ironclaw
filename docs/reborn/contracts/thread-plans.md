# Reborn 线程任务计划与运行状态读取契约

## 权威来源与分层

`ironclaw_threads::plan` 拥有任务计划结构、校验和原子持久化。`FilesystemThreadPlanStore` 使用已组合的 `ScopedFilesystem`，不自行选择数据库；调用 scope 的 tenant/user/agent/project/mission/thread 六轴共同隔离计划，invocation 不参与持久化键。记录位于 `/threads/plans/`，由现有 libSQL/PostgreSQL/内存后端承载。

`builtin.todo_read` 和 `builtin.todo_write` 经既有 first-party 能力授权、调度和 host runtime 执行。工具参数不能指定用户、租户或线程；写入必须提交完整 `title` 与 `steps`。`expected_revision` 可用于拒绝陈旧覆盖。每次成功写入生成递增 revision 的完整 `plan_update` 记录；`steps: []` 明确清空计划，保留 revision。没有创建过计划才返回 `null`。

`tianquan` 租户原有能力面板默认过滤文件效果。通用 host-runtime 的按能力 ID 可见效果上限仅对 `builtin.todo_read` 与 `builtin.todo_write` 补充所需的读取/写入效果；任意路径读写、shell 及其他能力仍保留原过滤。该例外只让声明的工具进入模型目录，实际调用继续经过授权、批准、作用域与同一个持久存储。回归：`ironclaw_host_runtime/tests/tool_surface_contract.rs::visible_surface_allows_exact_capability_effect_override_without_widening_others` 与 `tianquan_capability_policy::tests::todo_effect_override_only_advertises_scoped_task_tools`。

这是当前状态的单一 CAS 记录，不是追加历史日志。写入后读取校验，相同或更高 revision 才确认成功；校验失败返回 `UnverifiedWrite`，记录可能已经持久化，消费端应重读后再决定重试。本轮没有新增 SSE 任务事件；前端在切换线程、刷新、工具结果/turn 完成时重取，也可有界轮询，不能用工具调用次数推断任务进度。

## WebChat v2 读取

- `GET /api/webchat/v2/threads/{thread_id}/plan`：返回 `{ "plan": ThreadPlanUpdate | null }`。`ThreadPlanUpdate` 为域类型，包含 `kind: "plan_update"`、`thread_id`、`title`、`steps: [{ index, title, status }]`、`revision`、`updated_at` 和可选 `run_id`。步骤 status 为 `pending/in_progress/completed/failed`。
- `GET /api/webchat/v2/threads/{thread_id}/runs/{run_id}`：复用既有 `RebornServicesApi::get_run_state` 与 `RebornGetRunStateResponse`，读取实际 TurnCoordinator 记录。status 保持既有 PascalCase 序列化（如 `Running`、`BlockedApproval`、`Completed`），不推导工具完成与子 Agent 完成的关系。
- 子线程消息继续使用既有 `/threads/{child_thread_id}/timeline`；父子关联使用 canonical thread `metadata_json`，不新增 legacy engine DTO。

两个 GET 都必须携带 Bearer，继承同一 read descriptor（同源、无请求体、每调用者 120/60 秒）与 caller scope。产品 facade 先用既有线程服务核验身份及线程归属，授权失败与不存在均为 404，不能触碰任务存储或运行记录。自动化线程沿用已有 creator scope 解析。未接入存储、读取错误或损坏记录返回安全 503，不能将错误编码为 `plan:null`。

计划读取只暴露 `ThreadPlanReader` 只读端口；写权限留在工具链。composition 的 local-dev、libSQL 和 PostgreSQL profile 将 reader 与工具绑定到同一个权威 filesystem。未配置 reader 的自定义 facade 明确 unavailable。

## 验证与限制

- `cargo test -p ironclaw_threads --test thread_plan_contract`：实际 CAS 创建、更新、清空、重开、并发、陈旧 revision 与六轴隔离。
- `cargo test --test reborn_integration_tool_call todo_`：SDK seam 脚本模型执行实际 Todo 工具链，校验持久化读写与线程隔离。
- `cargo test -p ironclaw_host_runtime --test first_party_builtin_tools todo_`：通过 registry/handler 的输入、scope、缺失接线与真实存储调用。
- `cargo test -p ironclaw_product_workflow --test reborn_services_contract thread_plan_`：产品 facade 读取实际内存 filesystem；重建 facade 恢复；跨线程空态；用户/租户/agent/project 拒绝；失败与空态区分。
- `cargo test -p ironclaw_product_workflow --test reborn_services_contract get_run_state_`：canonical run 读取与跨用户拒绝。
- `cargo test -p ironclaw_webui --test webui_v2_handlers_contract get_thread_plan_` 和 `get_run_state_`：真实 Router 的 path/caller/响应与错误转发。
- `cargo test -p ironclaw_webui --test webui_v2_descriptors_contract`：路由策略表锁定。
- `cargo test -p ironclaw_architecture`：层依赖检查。运行态模型选择、实际委派和浏览器链路属于隔离环境 L3；上述内存后端/Router 验证本身不等同于模型实测。

## 兼容与回滚

新 API 与能力为增量；现有 timeline、运行状态序列化和对话数据不迁移。回滚移除新能力、读取路由与接线即可，旧版本忽略新 plan 记录，无需删除用户任务数据。没有外部观测组件、模型命令入口或产品专属任务逻辑。
