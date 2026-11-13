# 明确request-scope预算停止测试SSOT（普通403不能全停）

| ID | 因子与边界 | 验收/实际 |
|---|---|---|
| R1 | 当前过宽403 classifier+普通permission无code+配置backup | 实际HTTP RED backup0/successfalse，原件保；不是“普通403应不重试”的旧提案 |
| G1 | 完整403/error/permission_error/code精确run_budget_exceeded，流式和非流式 | typed PolicyDenied、HTTP1、retry0额外派发 |
| G2 | 明确run-stop+已配置backup | backup0；普通无code/任意code/旧budget_exceeded均backup1成功 |
| G3 | 上游message/code canary | 不进typed Display/Debug/持久事件；只固定reason/provider |
| G4 | known重复字段、validprefix+尾部垃圾、缺字段、body limit+1、ContentLength过长截断、非permission | 不冒明确stop；保RequestFailed/未知 |
| G5 | exact8192B界 | 精确界合法、界+1拒绝 |
| G6 | 原customBearer401凭据恢复门 | 四原用例保持GREEN |
| H1 | runner真实model-port adapter及agent recovery/compaction | 新测试已加，NOT_RUN，不靠静态None分支冒运行通过 |
| H2 | 真实gateway durable Failed/SSE EOF与资源正常闭合 | NOT_RUN，不伪Completed |

旧provider-policy-stop-http-red01无code/旧code期望1属于已淘汰过宽提案，只保历史，不作为当前契约bug证据。新的compat-red01直接证明误伤已配置backup；F stale witness另有真实_handle RED。所有fixture均0真实凭据/供方；不授normalActor、不签case43因果或费用结清。reqwest只提供HTTP framing后的body视图，不证明ContentLength外物理TCP无额外字节。
