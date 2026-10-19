# 方案：MCP 内建 —— `mcpServers` 作为宿主级能力

> 状态：**已立项，方案待评审**（2026-10-16）。目标：把 MCP 客户端从「插件」提升为 **ki 宿主级能力**
> ——外部 MCP server 在 `settings.json` 里配置、连接随会话存活、工具展开成原生工具；ACP 的
> `session/new mcpServers` 从拒绝改为接受。
> 关联：`plugin-abi.md` §6.2（mcp-adapter 不迁 wasm 的裁定：长连/起子进程，宿主级功能）；
> `ki-acp-serve.md`（ACP 面，批次 3 在此合流）。前身实现：`rpi-package/packages/kiqi-mcp-adapter`
> （手写客户端 2299 行，本机曾以 dylib 运行，已于 2026-10-16 退役）。

## 1. 要解决什么（实核证据）

| 事实 | 证据 |
|---|---|
| MCP 连接必须跨工具调用存活 | 前身实现按 `(host_scope, identity, name)` 键把连接存进程级 map，跨调用复用（`runtime.rs:134-185`）；stdio server 是长活子进程 |
| wasm ABI 表达不了 | `exec`/`proc` 句柄随调用收（`plugin-abi.md` §6.32/§6.34）；`net-http` 请求级 |
| ACP 协议面本就期待宿主管 MCP | `session/new` 携带 `mcpServers`（schema `v1/agent.rs:2659`）；ki 现为**响亮拒绝**（`acp/mod.rs:266-276, 374-383`） |
| 宿主内建的所有机制已就位 | 内建工具 trait（`kiqi-agent/src/agent_tool.rs:29`）、装配 seam（`session.rs:371-378` 之间）、会话级长活资源先例（ReloadContext/ACP LiveSession） |
| 生态有官方 SDK | `rmcp`（modelcontextprotocol/rust-sdk，Apache-2.0）；ABB 已在本组织内用 1.8.0 跑通 stdio 全链路（`abb/crates/abb-agent/src/mcp.rs`，1253 行） |
| MSRV 不是障碍（实核） | ki 声明 `rust-version = "1.78"` 已陈旧——已在依赖中的 `agent-client-protocol 3.3.0` 自身要求 `1.88.0`；实际工具链 1.99.0 |

## 2. 目标 / 非目标

| | 项 | 本方案 |
|---|---|---|
| ✅ | 配置驱动的 MCP（全局 + 项目 settings.json） | 批次 1（stdio） |
| ✅ | 工具展开为原生工具（`{server}__{tool}`），模型直接可见可调 | 批次 1 |
| ✅ | 连接随会话存活、每条收尾路径回收（含进程树） | 批次 1 |
| ✅ | resources（list/read/subscribe）与 list_changed 刷新 | 批次 2 |
| ✅ | HTTP/Streamable + SSE | 批次 2 |
| ✅ | ACP `mcpServers` 接受 + `mcpCapabilities` 如实声明 | 批次 3 |
| ❌ | meta-tool 形态（`mcp_call`/`mcp_list`/`mcp_request` 这类） | §8：以展开工具取代 |
| ❌ | prompts / sampling / elicitation / roots | §8：无需要，且扩大攻击面 |

## 3. 三个参照物

- **前身 `kiqi-mcp-adapter`**（手写 2299 行）：配置 schema（`{mcpServers:{name:{command,args,env,cwd,enabled,timeoutSeconds}|{url,headers,…}}}`，`config.rs:6-47`）、stdio 行帧 + 16MiB 上限 + 进程树收杀（`transport.rs`）、逐请求 deadline 1..=300s、失败即弃连下次重建——**语义蓝本**。它的三个已知弱点不继承：`exposure` 是死配置（从不执行）、配置路径每调用传参无默认、全局静态 SESSIONS（无会话归属）。
- **ABB 的 `rmcp` 用法**：官方 SDK 1.8.0（`client` + `transport-child-process`）跑通 initialize/工具分页列举/调用/取消/resources 订阅/通知处理/连接与结果预算/子进程 env 卫生——**引擎选型的直接证据**；其「用 serde_json 中转而非类型字段」的手法用来吸收 rmcp 的类型漂移。
- **ki 自身**：内建工具机制（`AgentTool` + `build_tools` + seam）、settings 解析（`Settings`，Deserialize-only、无 deny_unknown_fields）、会话级资源与收尾先例（各模式结尾 + `LiveSession::close` + Drop 兜底）、信任模型先例（插件授权文件：声明归作者、授权归用户、执行归宿主）。

## 4. 设计取舍

### 4.1 宿主级，不是插件（已裁定，不重复论证）
`plugin-abi.md` §6.2 已裁：长连 + 起子进程，宿主级功能。本机已据此退役 mcp-adapter dylib。

### 4.2 引擎：`rmcp` 1.x，pin 精确版本
采用 `rmcp = { version = "1.8", default-features = false, features = ["client", "transport-child-process"] }`（与 ABB 同为 1.x 线；3.x 近周级 churn + 1.88 MSRV，v1 不冒险）。协议值经 `serde_json` 中转，不依赖 rmcp 的 typed fields。HTTP 传输留批次 2（rmcp 的 HTTP 要 reqwest 0.13，与 ki 的 0.12 重复——届时裁决「接受重复」或「自写 HTTP 层」，前身实现有现成的 SSE 解码器）。

### 4.3 工具展开，而不是 meta-tool
每个发现的 MCP 工具直接注册成原生工具，命名 `{server}__{tool}`（ABB 先例）。理由：模型直接看见可用工具（前身实现全部塞进 `mcp_call`，可见性差）；`__` 命名空间天然避开与内置/插件工具撞名。真撞名（恶意 server 自命名 `read`）→ 跳过该工具并在 stderr 记明。**注意**：这会替换用户已熟悉的 `mcp_list/mcp_call/mcp_request/mcp_resource` 四点面——迁移差异写进 CHANGELOG。

### 4.4 配置：settings.json 的 `mcpServers` 段，schema 沿用前身
- 全局 `~/.ki/agent/settings.json` + 项目 `<cwd>/.ki/settings.json`（项目段先于全局？——合并规则：同名以项目为准，逐项覆盖）。
- schema 直接沿用前身形状（`command/args/env/cwd/enabled/timeoutSeconds` | `url/headers/…`），**用户既有配置文件可平移**；**不装 `exposure` 这种死配置**。
- 载入时显式校验：name 字符集（字母数字 `-` `_`）、command/url 非空、`timeoutSeconds` 夹 1..=300、未知判别字段拒绝。`Settings` 无 `Serialize`：本方案只读不写该段（`save_settings_to_path` 不动）。

### 4.5 信任立场（最高风险项）
宿主级 MCP 比 exec 授权更危险：起任意进程 + 出网 + **默认会继承宿主环境（含 provider API key）**。因此：
1. **授权即用户在自己文件里写的条目**（同 grants.json 规则：宿主绝不在调用方名下授权）；**项目声明的 server 需 `project_trusted`**（复用现成机制），未信任项目 → 整段跳过 + stderr 记明。
2. **env 卫生**：`env_clear()` + 白名单透传（PATH/HOME/LANG…），**provider 密钥默认不进 server**。
3. **每次 spawn 与每次出网都记日志**（command+args / host，stderr，WARN 级），失败不静默。
4. **进程树收杀**在每条路径（含 panic/Drop 兜底）；不养孤儿。
5. 批次 3 的 ACP 客户端注入另立策略（§4.7）。

### 4.6 生命周期
- 启动：`build_with_selection`（async）里建连接、做 initialize 握手，**在 seam（`session.rs:371` 编译期插件合并之后、`:378` active 过滤之前）注入工具**——一个 server 握手失败 = 跳过 + WARN，不拖死会话。
- 存活：连接所有权随会话（工具持 `Arc`），**不用全局静态 map**（前身弱点 7）。
- 收尾：print/json/interactive/rpc 四种模式的结尾 + ACP `LiveSession::close`，统一走一个 `McpManager::shutdown()`；`Drop` 兜底。

### 4.7 ACP 合流的边界
批次 3 才动：解码 `McpServerStdio`（`Http`/`Sse` 变体在 HTTP 落地前响亮拒绝）、经 `SessionParams`/`SessionDeps` 进 `build_session`、启用手握在 `session::build` 之前、`close` 收尾、翻转两条契约测试、`initialize` 如实声明 `mcpCapabilities`（stdio 恒真，http 视实现）。**客户端注入的授权策略（直接接受 vs 配置 allowlist）是批次 3 的待定项，需业主裁决。**

## 5. 接口草案

```jsonc
// ~/.ki/agent/settings.json 或 <cwd>/.ki/settings.json
{
  "mcpServers": {
    "codegraph": { "command": "codegraph", "args": ["serve", "--mcp"],
                   "env": {}, "cwd": null, "enabled": true, "timeoutSeconds": 60 },
    "docs":      { "url": "https://example/mcp", "headers": {"Authorization": "Bearer …"},
                   "enabled": true, "timeoutSeconds": 30 }
  }
}
```

- 工具命名：`{server}__{tool}`；schema 原样透传 server 的 `inputSchema`。
- 可见性：受 `--tools/--exclude-tools/--no-tools`（`tool_name_allowed`）过滤，与内置工具同规则。

## 6. 触达与验收

- **离线测试（批次 1 主验收）**：假 stdio server（`sh -c 'read line; echo …'` 形态，codegraph 测试先例）——起服务、列工具、调工具、isError 映射、超时、取消、收尾后**无孤儿进程**（OS 级断言）。
- **门禁**：全量 fmt/check/test + features 门 + 文档快照，与历批一致。
- **契约翻转**（批次 3）：`mcp_servers_are_rejected_and_unknown_sessions_fail`、`load_rejects_session_level_mcp_and_additional_directories` 两条测试改为接受语义。
- **人工**：用真实 MCP server（如 codegraph 或任一公开 server）跑一轮。

## 7. 明确不做

- 不做 meta-tool 形态（`mcp_call` 等）——展开工具取代之，迁移差异进 CHANGELOG。
- 不做 prompts / sampling / elicitation / roots（前身与 ABB 都不做；ki 的 ask-user 已有同类能力）。
- 批次 1 不做 HTTP/SSE/resources（批次 2）；不做 ACP 合流（批次 3）。
- 不写回 settings.json（`Settings` 无 `Serialize`，本方案只读）。

## 8. 批次拆分

| 批次 | 内容 | 验收 |
|---|---|---|
| 1 | rmcp 1.x + 配置解析/校验 + stdio 传输 + 工具展开 + seam 注入 + 生命周期收尾 + 信任（env 白名单/日志/进程树）+ 假 server 离线 e2e | 门禁全绿；假 server e2e 全过、无孤儿 |
| 2 | resources（list/read/subscribe + `resources/updated` 注入）+ list_changed 刷新 + HTTP/Streamable（reqwest 重复裁决） | 同上 + HTTP 假 server |
| 3 | ACP 合流（接受 mcpServers、能力如实、契约翻转）+ 客户端注入策略裁决 + 文档/CHANGELOG + 真实 server 实测 | 契约测试新语义全绿；一轮实测记录在案 |

## 9. 已定 / 待定

**已定**：宿主级（§4.1）；rmcp 1.x pin（§4.2，MSRV 实核无碍）；工具展开 `{server}__{tool}`（§4.3）；
配置沿用前身 schema、不装死配置（§4.4）；信任立场五条（§4.5）；seam 与生命周期（§4.6）。

**待定**：
- 项目段 vs 全局段的合并细则（同名覆盖方向）——批次 1 定并在方案补记；
- HTTP 落地时 reqwest 0.12/0.13 重复的取舍（接受重复 vs 自写 HTTP 层）——批次 2；
- ACP 客户端注入 server 的授权策略（直接接受 vs `settings` allowlist）——批次 3，需业主裁决；
- 工具展开后 `--tools` 过滤与插件命名冲突的最终形态——批次 1 实测后补记。

## 10. 批次 1 状态（2026-10-16，完成）

提交 `07d808a`（分支 `feat/mcp-builtin`，已 ff-合入 main）。交付：新 crate `crates/kiqi-mcp`
（config/manager/tool，~1.5k 行）+ kiqi-cli 接线（Settings 只读解析 `mcpServers`、
`start_mcp_manager`、seam 注入 `merge_mcp_tools`、ReloadContext 持有、各模式收尾 +
ACP close + Drop 兜底）。引擎 `rmcp 1.8.0`（pin）；测试 21 单测 + 10 e2e（含进程树
OS 级断言：server pid 与孙进程都须消失）。

**审查 9 条（去重 6）全部修复**，两条高危：`/reload` 重建工具集时静默丢掉全部 MCP
工具（改为 load/reload 共用 `merge_mcp_tools`）；工具名允许 `.` 且无长度上限会产生
provider 非法名（收紧 `[A-Za-z0-9_-]` + 64，按 Anthropic 规则）。

**实现期挖出的 rmcp 事实（写下来备查）**：① rmcp 的优雅关闭只收干净退出的 server，
**孙进程会成孤儿**——实现用进程组 SIGTERM（pgid 仍在手上时）兜住，Windows 走 job object；
② rmcp 的读路径**没有长度上限**（`AsyncRwTransport::receive` 用无界 `read_until`，codec
的 max_length 只管写侧）——自建 `ChildTransport` + 16MiB `CappedReader` 补上（前身
adapter 的 16MiB 约定）。

**批次 2/3 待做**：HTTP/Streamable + SSE、resources（list/read/subscribe +
`resources/updated` 注入）、list_changed 刷新；ACP 合流（`session/new mcpServers`
由拒绝改接受、`mcpCapabilities` 如实声明、两条契约测试翻转、客户端注入策略裁决）。

## 11. 批次 2/3 状态（2026-10-17）

**批次 2**（`fb3ae8b`，已合入）：resources（`{server}__read_resource`，20k 截断）+ 订阅
（上游 main 语义含 PR #21/#23：幂等 subscribe、mark-before-RPC、重连按序恢复、退订失败
退役传输）+ **通知进 LLM**（业主语义：`resources/updated` → `McpNoticeMailbox` 按
(server,uri) 合并 + 抑制计数 → 五个宿主的回合边界排干为一条批量 custom entry；绝不自动读
全文，读取经展开工具按需）。审查 11 条（去重 8）全处理，含 rotate_lifetime 锁外清队列、
list_changed 误删撞名工具、server 名含 `__` 破坏前缀解析等；`--subscribe` 配置数组驱动
自动订阅。**对比上游的刻意差异**：上游收到通知后程序自动 `resources/read` 并注入全文；
ki 只投递通知（业主裁定）。

**批次 3**（`c6114d6`，已合入）：ACP 合流。`session/new`/`session/load` 的 `mcpServers`
由拒绝改为接受（stdio；**POLICY 1**：ACP 客户端即用户本人在编辑器里的授权，不做项目信任
门，每个接受都在 stderr 响亮记录）；HTTP/SSE 变体 -32602 直到传输落地；
`additional_directories` 维持拒绝。资源更新的**用户侧副本**：客户端声明 `notices` 能力时
边界排干同时发 `SessionUpdate::Notice`（模型副本不变，不双投）。契约测试两条翻转 + decode
单测（stdio 含 env 转换、Http/Sse 拒绝）。

**已知缺口（批次 3 记录）**：ACP 级「客户端 server 的工具经真实启动路径出现」未做端到端
（假 server 二进制对 kiqi-cli 测试不可达、ACP 测试工厂为桩）；该链路由 decode 单测 + kiqi-mcp
启动路径 e2e + 类型化接线共同覆盖。

**剩余**：HTTP/Streamable + SSE 传输（届时 HTTP 订阅自然解锁）、prompts/sampling（明确不做）、
`autoRead` 配置项（若要把部分资源改为程序自动读）。

## 12. 批次 4 状态（2026-10-17，完成）：HTTP/Streamable 传输

提交 `4eef4d5`（分支 `feat/mcp-http`，ff 合入 main）。选型按侦察结论的 option d：
保留 rmcp 的会话/SSE 状态机（`transport-streamable-http-client`，**不开** `-reqwest`
变体），由仓库自实现 `StreamableHttpClient`（`crates/kiqi-mcp/src/http.rs`，近逐字移植
rmcp 0.13 的 reqwest 实现）——整棵树仍是唯一 reqwest 0.12.28（`cargo tree -i reqwest`
核实），不引入第二个大版本与 aws-lc-rs。

客户端纪律：16MiB 总上限（含 Content-Length、流式读取、POST-SSE 回复与 GET 流单事件；
裸 CR/LF/CRLF 均按行终止）、同源重定向（跨源拒绝且不接触目标）、typed 401/403、
404=SessionExpired、URL 拒绝内嵌 userinfo 且日志不打印完整 URL；代理/超时沿用
`kiqi_ai::http` builder（`httpIdleTimeout` 经 `start_with_http_policy` 接入）。

订阅在 HTTP 上解锁：standalone GET SSE 的 `resources/updated` 进 `McpNoticeMailbox`；
**404 重初始化后恢复订阅**（审查高危——rmcp 就地 reinit 不会重建 ki 侧订阅，修复补了
恢复路径）。`autoRead`（每 server 可选 uri 列表）：命中者在边界读取全文（20k 截断）并入
通知条目，整批有总时限、超时降级轻通知。

ACP：`session/new`/`session/load` 接受 `McpServer::Http`（headers 透传、接受即响亮记录），
`Sse` 维持 -32602；`initialize` 声明 `mcpCapabilities.http=true`。

测试：进程内 loopback 假 HTTP peer（JSON/SSE、GET SSE、订阅、404 过期、DELETE、分块与
SSE 超限、跨源重定向、会话头校验）+ 5 个 http e2e；19 个 stdio e2e 与 44 单测保持全绿；
代理环境变量下测试 hermetic（loopback 绕行）。审查 11 条（1 高危/2 中/8 低）全部修复。

**剩余**：legacy SSE 传输（ACP `Sse` 变体）仍不做；prompts/sampling 不做；`.wasm` 签名与
permissions 宿主化各自立批（见 §7 / 侦察记录）。

## 13. 信号驱动唤醒（2026-10-19）

通知的宿主侧唤醒策略（`subscribe` 即唤醒意图、预算、**记录与触发分离**、智能体处置）
见 `signal-driven-agent.md`；阶段 1 已在交互式 TUI 实现。`kiqi-mcp` 仍只负责投递、
合并与 `autoRead`，唤醒状态在 `crates/kiqi-cli/src/signal_wake.rs`。
