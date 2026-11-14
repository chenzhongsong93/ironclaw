# 明确请求级预算停止与普通provider拒绝兼容（最终窄设计）

旧freeze01将全部标准403硬停的过宽提案已被兼容性审查淘汰；原件不改、不作为当前缺陷证明。本版仅消费完整403/type=error/error.type=permission_error/error.code=run_budget_exceeded。该code报告配置endpoint的请求级停止意图，不证明真实额度或现金已结清。普通无code、任意code、旧budget_exceeded保持RequestFailed及已配置backup能力。

## 具体传播链与最薄修改

1. `ironclaw_llm/lib.rs::route_anthropic` Bearer→OAuth；`send_request`和`send_request_streaming`仅403进入有界标准envelope分类；官方401刷新后的第二403亦共用它，customBearer401原credential门不动。
2. 新`provider_policy.rs`最多8192B、application/json、HTTP framed body EOF；serde拒known重复字段/尾部垃圾/截断/缺字段。只有精确run_budget_exceeded返回固定ProviderPolicyDeniedReason::PermissionDenied。上游message/code只RAM验证，不进typed错误Display/Debug或持久事件。未知403仍RequestFailed；401/429/5xx/200 SSE error/半流不扩分类。
3. `retry::is_retryable`/`FailoverProvider::try_providers`已有封闭白名单，PolicyDenied自然不retry、不进backup；普通permission继续backup。两者生产代码不改。
4. `runner/model_gateway.rs::map_provider_error`新PolicyDenied→现HostManagedModelErrorKind::PolicyDenied；`loop_host/lib.rs::model_error_kind`→现AgentLoopHostErrorKind::PolicyDenied；`agent_loop/executor/mapping.rs::model_error_class`→None，ModelStage::process直接返回HostUnavailableWithDiagnostics，不调用recovery。
5. PlannedDriver::map_executor_error→AgentLoopDriverError→RebornTurnRunExecutor→TurnRunScheduler::result_to_outcome/record_terminal_failure沿原失败记录。不给新scheduler旁路或手写Failed/Completed。InvalidRequest会映Unavailable、BudgetExceeded被当ContextOverflow，不复用它们，也不伪401/413。

## 消费侧私有因果ticket

每个_handle唯一创建无入参_RequestBudgetTicket。forward先observe；只有紧邻本次真实guard.reserve的except捕获exact GuardDenied确定三码时，才把本guard及本exception identity绑本ticket。read/observe/未知供方异常不绑定。handler只在尚未发success headers时consume：exact私类、guard/error均is相同、未消费；私lock保证并发至多一次，消费即清引用。成功才回小标准403和run_budget_exceeded。last_denial只诊断，不是authority。direct forward无ticket语义不变，无body/header可控标志、新factory或callback。

## 实际证明与未完成门

- 当前过宽版真实普通403→配置backup反例RED：backup0、successfalse；修后普通无/任意/旧code均backup1成功。
- 明确run-stop完整HTTP、stream/nonstream、retry、backup0及泄漏/边界：8项GREEN；形状/8KiB界4项GREEN；旧customBearer401四项GREEN。
- F真实_handle+真实guard：三本地预算拒绝无新Original；stale字符串/旧真实error从read与observe重放3项RED后修复，含交叉ticket/并发双消费/direct无ticket共13项，85联合回归通过。
- runner adapter/agent恢复及compaction的新测试已新增但尚未编译执行；真实gateway durableFailed/SSE EOF G4 NOT_RUN；不能以provider库NoModel测试合签终态或case43因果。Rust源变更后消费七件native资格须刷新。

## 资源

只在实际配对I，复用已核custom-bearer-401-green01同checkout/layout/pair17a build.lock的target：D:/code_repo/TianQuan/.codex/cache/17a6175750ab84591fb76d989d50e43b5ec57ec64a37ae7411e6a0cd39ba137d/target。相同GNU/flags/offline/jobs1，持原锁，无A并发编译、不建target。初始D余150.65GB/RAM约4.56GB，单次预算15min/新增20GiB，低RAM不追加任务。仅本地NoModel fixture；所有已跑Cargo/fixture/原句柄与锁已收尾，原日志和故障采集器partial保留，不删数据或旧证据。
