# Reborn 创作台运行时交接（2026-09-23）

本轮服务天权 P1 自营标杆与平台授权。通用能力留在 IronClaw，天权只做鉴权与 UI 薄消费；保持终端单进程可交付，不引入外部观测组件。改动来自 `feat/tianquan-timeout-lease-spawn-fixes` 工作树，提交与主分支状态以 Git 引用为准；正式部署另需独立发布门控。

## 已实现的契约

- `builtin.todo_read` / `builtin.todo_write` 通过调用者和线程范围读写完整计划快照。`ironclaw_threads::plan` 使用 scoped filesystem 与 revision CAS；`steps: []` 表示有 revision 的清空快照，与 `plan: null` 区分。WebChat v2 提供 `/threads/{id}/plan` 权威读取，故障显式返回错误。详见 [thread-plans.md](contracts/thread-plans.md)。
- `CapabilitySurfacePolicy` 支持按精确 capability ID 的 effect 可见性例外，仅天权装配中的两个 Todo 工具获准读写其计划文件；通用文件、shell 与 coding 禁令保持原状。详见 [host-runtime.md](contracts/host-runtime.md)。
- WebChat 命令目录与处理器共用注册表；只读 `/help`、`/status` 等命令不创建模型 turn，技能目录来自运行态。命令结果带 `source=command`，供消费侧与模型正文分开。
- 天权装配只向模型公布 16 个实际可解析的 SOUL，排除 `general` 等四个内置 profile；`chapter-reviewer` 仍可派生。目录与 resolver 同源，不扩大子 Agent 文件或 shell 权限。
- 工作区包名现为 `ironclaw_tianquan_subagents`，符合架构命名门；保留 `tianquan_subagents` 依赖别名和 lib 名，既有 Rust 调用点无需改变。

## 验证与边界

- Linux caller：精确 effect 可见性 1/1、SOUL 目录含 `chapter-reviewer` 且排除 `general` 1/1。宿主组合 `cargo check` 通过；独立候选 Gateway 镜像 `sha256:ce6ee6759d11db2cf34b3876707f6c6c044e92783281c812140af39fa91d11a4` 构建退出码 0。
- 合并前的 Linux 回归：`ironclaw_architecture` 全套退出码 0（Windows 挂载的 composition snapshot 在测试容器内按原仓 LF 内容归一）、`ironclaw_llm --lib` 953/953、`ironclaw_runner --lib` 323/323、包名调整后的 SOUL 目录 1/1；`scripts/test-pre-commit-safety.sh` 49/49 与当前暂存差异的 `scripts/pre-commit-safety.sh` 均通过。受影响十个 crate 的 Linux --all-targets --all-features -D warnings 退出码 0；全工作区 Linux Docker clippy --locked --workspace --all-targets --all-features -- -D warnings 亦退出码 0。补修后的 Todo whole-path integration 1/1 真执行通过（revision 2 与空步骤权威读回）。
- 天权本地 main 已在合并提交 `80d99ff` 后以交接修正提交 `c76c2b2` 落定；其原子化任务的八个脏文件哈希核对一致，`tests/e2e/auth.ts` 的并行 WIP 已重新应用且留有一个 Git stash 备份。天权合并树 API 283/283、严格 Clippy、前端构建和 Vitest 499/499 通过。
- 天权隔离 compose 使用独立 API/PG/Rena/home/正文卷。真实模型创建 Todo revision 1、更新 revision 2、清空 revision 3；另一真实线程为 `plan: null`，浏览器刷新与错误恢复通过。修复版 Gateway 的全新父 run `c083fafe-202f-4b38-92e5-5510f21fcd85` 实际派出 `chapter-reviewer`，子线程 `subagent-a5d573ac78f44cd886d54fa3269b2440` 为 `Completed`，含实际 user/assistant 两条消息；错误 parent 404、他用户 403、未认证 401。另有真实取消到 `Cancelled` 的链路。隔离测试账号套餐标识已恢复，不重置余额或用量。
- 已发现 blocking spawn 的父会话 timeline 有 completed 工具预览，但 dossier 漏首个模型交换与 spawn 工具步骤；该诊断缺口已在天权 `ISSUE-IRONCLAW-013-reborn-dossier-misses-spawn-first-step.md` 按真实 run 立单。子线程持久消息和权限未见异常。修复前排障仍先取 dossier，再对照父 timeline 与 child thread。
- 本地多 crate `cargo fmt --check` 被既有 egress、llm_admin、directions/flavors 等格式差异阻断；本轮新增断言已按 rustfmt 排版。合并代码不等于生产切换，现有 5188 所接 API 尚未加载新目录；隔离链路使用 5189。
- WebUI v2 的描述符、handler 与 caller 测试文件已经超过 1,500 行；本轮只补有限的计划/命令契约，沿现有 axum fixture 验证。按 Reborn 产品面迁移计划 #3031 跟进拆分，四处 `arch-exempt: large_file` 仅豁免本轮局部增量，不声明大文件债务已清。
- 既有 Anthropic OAuth provider 也超过 1,500 行；本次为工作区严格 Clippy 只折叠一处等价 SSE 判断，按计划 #3031 的 provider transport 复审再拆分，`arch-exempt: large_file` 不代表已消除该债。
- 合回本地 main 的架构门将现存 `get_thread_plan` 与三项 LLM subject key 操作纳入一次性 92 方法快照；它们是现有 feature 分支产品面负债，不新增开放式扩张规则。计划 #3031 把这些读/凭据操作迁回 view/capability descriptor，之后 ratchet 只能缩减。

本地 IronClaw main 已快进到功能提交 f085641bc 及 Todo 测试补修 1740029be；天权本地 main 的合并与交接提交见 80d99ff、c76c2b2。两仓仅本地提交与合并，尚未推送远端或替换正式运行容器。5188 与隔离 5189 预览均经 HTTP 200 复验。

回滚：恢复部署前 Gateway 镜像即可撤去新增 Todo 工具/读取路由；持久计划数据留在独立 scoped 路径，不影响旧会话消息。天权消费器遇旧版 Gateway 的计划路由缺失会显示读取失败，不把工具活动伪装成 Todo。主分支合入后仍需发布版本对齐与正式隔离账号复验。
