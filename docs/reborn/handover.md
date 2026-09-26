# 当前交接：本机主线回合、天权角色 bundle 与项目 scope（2026-09-26）

IronClaw 以本机 `main` 为集成真源，已合入并验证 `codex/novel-studio-runtime` 的运行时改动；本地主线当前包含合并提交 `7467920d4909a5a155c33935dc6d4a30b22a31b5` 与 Docker build-context 修复 `d880da4fa39d226805dcb2cf408fc33912be0cb6`。TianQuan 同时实现项目 scope 透传与接收端 MCP 验签。两仓没有删除分支或 worktree，TianQuan D 盘既有未提交任务改动继续保留。

## 2026-09-26 本轮回合、验证与部署

- `codex/novel-studio-runtime` 的 13 个功能提交已通过普通 no-ff 合并进入本地 `main`，分支本身与其干净 worktree 保留；`git branch --no-merged main` 为空。合并使用本机主线树解决冲突，保留 typed CAS Todo store 与 typed `ThreadPlanUpdate` contract，并纳入只读 TianQuan SOUL bundle loader、可信 MCP 出站签名和 caller 侧项目 scope。
- TianQuan role bundle 运行测试对真实 bundle 做目录/hash 验证：novelist allowlist 为空，worldsmith 仅有 `tianquan-graph.run_world_patch`，market researcher 仅有 `builtin.http`。通过 `ironclaw_tianquan_subagents` bundle 测试 9/9、novelist composition/provider-capture 1/1、MCP adapter/signer/dispatch 59/59；最终 system、User task/handoff 与压缩/resume 后 prompt 的受影响 runner 测试通过。
- 受影响回归：thread/Todo contracts 6/6、Reborn services 227/227、WebChat inbound 31/31、loop host 相关全套与 runner 相关回归通过。IronClaw 受影响 crates 的严格 Clippy 和 42 个修改 Rust 文件的 rustfmt check 通过。
- `ironclaw-reborn:tianquan` 已从本机主线通过 Git archive 构建并重建到 Gateway；image ID `sha256:8c41204511590cbb86bc9aab6c765db017176be0fd3b99f35e31ebd2ac89f8`，revision label `d880da4fa39d226805dcb2cf408fc33912be0cb6`。Windows 工作树的 pnpm junction 在 BuildKit context scan 会报不可访问；本轮按 Git archive 生成干净构建上下文，镜像成功且缓存复建退出 0，未清理或改写 `.pnpm-store/`。
- TianQuan 当前 API 镜像 `tianquan-api:latest` 为 `sha256:3d0ca0e4b4e75d9169a9a0b0ec206d0241e068a3cc882bd196787e514ae8b366`；本机实时 MCP 联验为 initialize/tools-list 200、未签名工具调用 401、同 scope 签名到 handler、跨 project 403、nonce replay 401。旧 thread scope 的 isolated PostgreSQL rollover 4/4。
- 主机 80、8181、API `/api/health`、Gateway 3000 与 5188 均 HTTP 200。API/Gateway 已在本地重建；本轮没有重启或重置 PostgreSQL/Rena。此前隔离 Todo、真实子 Agent 状态/消息/返回与父子权限、整页 UI 控件、零额度、正文直接 PUT/force 422 及活跃续期证据仍见天权交接 2026-09-25 项。
- `ISSUE-IRONCLAW-011` 的运行时与调用装配代码已修并合入，但该单按其验收条件仍 open：需在隔离新项目用当前 Gateway 生成新 novelist run、保存 dossier 并做独立质量回归。provider-capture、旧 run 和静态 fixture 不充当 L3。`ISSUE-IRONCLAW-013` 保持 closed。
- Windows `ironclaw_runner` 全 targets 中 `jsonl_durable_log_replays_loop_model_reply_milestones` 仍失败；在未合入前的干净基线 worktree 重跑得到同一失败，故记录为既有缺陷，不声称全目标测试完全通过。除该项外 runner 其他目标跳过此既有用例后通过。
- 本地 `main` 的 two-parent/fork ancestry 已包含先前自维护 fork 回合；代码及本轮交接以普通推送同步到个人 fork `fork/main`，已核实 push 成功。nearai upstream `origin/main` 不作为发布目标。D 盘 IronClaw 的未跟踪 `.pnpm-store/` 与 TianQuan 所有现存工作树均保留。

## 实测与部署

- IronClaw run-trace 单测 7/7、spawn port 请求/响应断言 1/1、runner 仅终态压缩策略 1/1；Linux Gateway 集成 13/13。新增普通 `builtin.time` 调用仍只输出一对关联事件，避免通用 exporter 重复采集。
- 最新本地 Gateway 镜像 `ironclaw-reborn:tianquan` 为 `sha256:0a470a38bd907b35b27d949b830b8c30b679ecf4b90e9437857be5dfbf7ab022`，revision label `0ed934f4b`；天权 API dossier 镜像 revision `af7e49c`。Compose Gateway 已按此镜像重建运行，日志确认 `tianquan-graph` 扩展激活。API、Gateway、Web、5188 预览均 HTTP 200；主 PostgreSQL 未重启/重置，主 Rena 保持关闭。
- 新隔离真实 run `14617be9-a52f-4493-a561-16498eb90121` 的天权 dossier 返回 8 events/4 steps，触发 spawn 的首个模型交换、同 invocation 工具调用、resume 的 result_read 与终答均完整。子线程 `subagent-3bb3769526e6462aaf0b0315b342a076` 为 Completed，真实 user/assistant 两条消息；父子检阅、返回、刷新和权限浏览器用例 2/2 通过。模型请求只要求固定短句，未写正文、文件或图谱，不重置账户余额。
- 隔离 Todo run 的最新 API dossier 读取确认 `todo_write/read` 三轮 revision 1→2→3，最终空步骤；另一线程为 `plan: null`。真实历史切换、刷新与错误/空态恢复浏览器测试 2/2 通过。零额度/API 与正文直接写门的细节及控件覆盖见 TianQuan `docs/handover.md` 顶部。

## 分支与发布边界

个人 fork 的 `fork/main` 已与本地 IronClaw `main` 做两亲历史合并，合并树以本地为准；原 fork `main` 提交作为第二父历史保留，未强推或删除其提交。合并提交为 `2ccf9eaa9`，基于 `72fd21bbd`，第二父为 `342bb8560`；本地主线与 fork `main` 随后普通快进同步。fork 侧的 61 个独有提交没有被删除，但其文件树更改未覆盖本地代码。nearai `origin/main` 仍未合入或推送，不是 010/012 的关闭条件。没有删除分支或 worktree；UX 隔离容器/5189 在验收后停止但保留容器和卷，5188 持续可访问。本轮是本地验证，不是生产发布。

---

# 当前交接：已审核本地主线 Gateway 镜像部署（2026-09-24）

Reborn Gateway 已从 IronClaw 本地 `main` `932ae4236` 重建，镜像 `ironclaw-reborn:tianquan` 当前 ID 为 `bd9f16055fa1`；TianQuan `tianquan-graph` 扩展也从主线镜像重新落地，init 容器退出码 0。Gateway 日志确认扩展激活，运行态目录包含 `create_project`、`run_state_writeback`、`run_novelist_validator` 等能力。部署过程没有发模型请求或写正文。

## 联合运行状态

- 本地 Compose 项目 `docker` 的 API、Web、Gateway 已按 TianQuan 主线重建运行；API 返回 `/api/health=200`，Nginx `/api/health=200`，Web 80/8181=200，5188 预览=200。PostgreSQL 原容器与 `docker_postgres-data` 卷没有重启或重置；主 Rena 仍关闭。
- UX 验收用的隔离 API/Gateway/PostgreSQL/Rena/init 容器在验收结束后全部停止；5189 临时 Vite 也已关闭。隔离真实委派生成一个 `Completed` 的 `chapter-reviewer` 子线程，父 timeline 与子线程 API 返回实际 user/assistant 消息；父子隔离及用户越权检查通过。测试提示只要求返回固定验收短句，并禁止文件、图谱及正文写入；未重置余额。
- TianQuan 最新主线为 `71d2494`（本轮 API 运行时代码最后提交 `c28c8c6`）；其 316 个 API 库用例通过，拒绝未知正文、签名回执、force 不激活正文以及隔离 HTTP 直接 PUT/force 422 均有新鲜证据。详细证据和 Rena 测试 fixture 边界见天权 `docs/handover.md` 最新入口。

## 尚未闭环

- 本地 IronClaw `main` `932ae4236` 与 upstream `origin/main` `f7da7dd7b` 分叉（本地独有 71、远端独有 35）。Gateway 按已审阅的本地主线构建；未将未审 upstream 提交带入部署，也未向 upstream 主线强推。个人 fork 评审分支继续保留。
- TianQuan `ISSUE-IRONCLAW-013` 的 blocking spawn dossier 首步缺失仍开放；实际子线程和权限正确，不代表该诊断采集问题单已关闭。
- 本地 WebChat 运行配置报告 `LocalDev`/`DevOnly`，只适用于本地开发；未将此容器称为生产发布。本轮没有发布 tag。

# 当前交接：本地分支回合与远端推送（2026-09-23）

本轮按用户要求检查本地分支并回合到 `main`；没有删除任何分支或 worktree。当前 IronClaw `main` 为 `a787b2490`。Todo/runtime 与子运行隔离已在 `61bbca3fd` 合入；`codex/novel-studio-runtime`、`feat/tianquan-timeout-lease-spawn-fixes`、`tianquan-soul-v1` 均可从 main 到达。旧 `tianquan-soul` 通过 `a787b2490` 的合并父链收口，但保留 main 文件树：该线的 20 项 flavor 与当前实际支持的 16 项 resolver 不一致，且其 composition 冲突会关闭 local-dev 所有网络目标的私网 IP 防护；这些效果不应覆盖当前目录与网络边界。原分支仍在，完整提交历史仍可追溯。

远端状态：个人 fork 的 `fork/main` 为 `342bb8560`，与本地 main 分叉（本地 69 个独有提交、远端 61 个独有提交）；上游 `origin/main` 获取本轮遇到 Schannel early EOF，且 origin 指向 nearai 上游。为避免强推或丢弃 fork 上的提交，本地主线已推送到个人 fork 的新评审分支 `codex/tianquan-mainline-integration-20260923`；远端 `main` 保持不变。没有强推，也没有删除远端分支。

验证边界：本地 main 最近代码门控已在前序交接记录；本次额外核对所有本地分支均为 main 祖先、main 工作树干净。此次旧 SOUL 合并树与原 main 完全一致，没有新增代码。任何上线仍须独立发布镜像并复验，不因本地分支回合而宣称生产运行时已更新。

---

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
