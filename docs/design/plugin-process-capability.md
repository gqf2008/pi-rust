# 方案：wasm 插件层的「调用内交互式子进程」能力（`proc`，ABI 0.7.0）

> 状态：**方案（待评审）**。目标是把 `rpi-codegraph` 迁到 wasm；`rpi-orchestrator` **不在本方案范围内**
> （理由见 §2 与 §9，它是执行模型错配，不是缺一个能力）。
> 关联：`plugin-abi.md` §5（权限模型）/§6.2（不迁名单）/§6.30（exec cwd scope 那一轮）。

## 1. 要解决什么（实核证据）

`rpi-codegraph` 的 `codegraph_search` 不是「跑一条命令拿输出」，而是**在一次工具调用内**做一场
stdio 会话：

| 事实 | 证据 |
|---|---|
| 每次调用现起 MCP 服务进程 | `plugins/rpi-codegraph/src/lib.rs:315` `let mut child = spawn_server(&root)?;`（在 `call_codegraph` 内） |
| 服务命令 | `lib.rs:161-171` `codegraph serve --mcp --path <root>` |
| 在子进程 stdin/stdout 上做 JSON-RPC | `lib.rs:179-181`（写）、`:335-346`（按行读）、`:197`/`:222`（会话超时/提前关闭） |
| 调用结束即收 | `lib.rs:385-387` `drop(stdin); child.kill(); child.wait();` |
| 索引准备是一次性命令 | `lib.rs:92-123` `codegraph init` / `codegraph sync`（一次性、有终止） |

**结论：需要的是「调用内可交互的子进程句柄」**——起、写、按行读、等、杀，**全在一次工具调用内完成**。
不需要跨调用存活（这正是本方案敢做的原因；跨调用是另一件事，见 §9）。

现有的 `exec` 表达不了它：`exec.run` 是**一次性**的（进去、命令退出、返回输出），
中间没有跨多次往返的句柄；`net-sockets` 的 `u32` 句柄范式（`plugins/kiqi-plugin.wit:254-262`）
证明这个 ABI 已经有「宿主持句柄、guest 只拿编号」的成熟写法，`proc` 照抄这个形状即可。

## 2. 本方案解决 / 不解决

| | 插件 | 现状缺口 | 本方案 |
|---|---|---|---|
| ✅ | `rpi-codegraph` | 调用内交互式 stdio 会话 | 解决 → 可迁 wasm |
| ❌ | `rpi-orchestrator` | ① 子代理可活 1800s（exec 上限 300s）② `dispatch` 后台节点要**跨调用**存活 ③ 进程组收杀 ④ spawn `current_exe()` | **不解决**，且不该硬凑（§9） |

## 3. 设计取舍

### 3.1 新能力 `proc`，而不是给 `exec` 加参数

- §5 的原则是**策略随能力走**：`exec` 的授权语义是「跑一条有界的命令」，`proc` 是「开一场会话」——
  危险画像不同（后者能交互、能长时间占着 stdin/stdout），把两者塞进同一个 grant，
  用户批准时的心理模型就失真了。
- 代价：用户可能要为同一程序批两次（`--command codegraph` 出现在两个 grant 里）。
  缓解：`ki plugin grant` 一次传多个能力时复用同一份 `--command/--cwd` 值（CLI 侧小改）。
- 备选（不推荐）：`exec` grant 加 `interactive: true`——把「有界」这个关键承诺变成可选字段。

### 3.2 句柄用 `u32` + 宿主 map，而不是 WIT `resource`

- 与 `net-sockets` 完全一致（`HashMap<u32, TcpStream>`，`crates/kiqi-plugin-host/src/lib.rs:218-219`），
  一致性能让 SDK 与文档只讲一套心智模型。
- `resource` 的类型安全与自动 drop 更漂亮，但会引入这个 ABI 尚未使用的第二种句柄习语；
  且**真正的生命周期保障来自调用结束时的统一收尸**（§5），不来自句柄的 drop。
- 陈旧句柄的风险被限定在一次调用内（调用结束即全部关闭），且宿主对未知句柄返回 `failed`。

### 3.3 读是「按行」，不是「任意字节」

- 实际需求就是行帧（MCP/JSON-RPC over lines：`lib.rs:335-346` 用 `BufReader::lines()`）。
- 提供一个 `read-line(handle, max-bytes) -> option<string>`（`none` = EOF 且无残留）加一个
  `read-stderr(handle, max-bytes) -> list<u8>`（诊断用；codegraph 用 stderr 做诊断文本，`lib.rs:300-303`）。
- 任意的字节流读写（含二进制协议）不是本能力的目标——需要时再单独立项，不要把契约一次做胖。

## 4. 接口草案（WIT，加到 `plugins/kiqi-plugin.wit`）

```wit
/// 一场调用内的交互式子进程会话。WASI 没有进程接口，宿主用与 `exec` 同一套
/// 强制手段实现它：裸命令名白名单、cwd 监禁、清空环境只回填 PATH、整组收杀。
/// 句柄的寿命**限于发起它的那次工具调用**：调用返回（正常/失败/panic/取消）时，
/// 宿主关掉全部句柄并杀掉各自的进程组。
interface proc {
    use types.{capability-error};

    /// 起一个子进程。cwd 与 `exec` 同一套 scope 规则（plugin-data | project，
    /// 相对路径、监狱内外一律同款检查）。返回句柄。
    spawn: func(cmd: string, args: list<string>, cwd: option<string>) -> result<u32, capability-error>;

    /// 写 stdin。返回被接受的字节数（宿主对缓冲有上限，写满即 backpressure，
    /// 不是无限吞噬）。
    write: func(handle: u32, bytes: list<u8>) -> result<u64, capability-error>;

    /// 读 stdout 的一行（含行尾剥离）。`none` = 流已结束。
    /// `max-bytes` 是这一行的上限，超长行按截断处理而不是把宿主内存拉爆。
    read-line: func(handle: u32, max-bytes: u64) -> result<option<string>, capability-error>;

    /// 读 stderr 的可用字节（不阻塞：有多少给多少，没有则空列表）。
    read-stderr: func(handle: u32, max-bytes: u64) -> result<list<u8>, capability-error>;

    /// 等子进程退出，最多等 `timeout-ms`。`none` = 还在跑。
    wait: func(handle: u32, timeout-ms: u64) -> result<option<s32>, capability-error>;

    /// 杀进程组并关闭句柄（幂等）。
    kill: func(handle: u32) -> result<_, capability-error>;
}
```

世界面（`plugins/kiqi-plugin.wit:301-312`）加 `import proc;`；包版本 `0.6.0 → 0.7.0`。

## 5. 生命周期与收尸（本方案最需要写清的部分）

1. **句柄寿命 = 一次工具调用。** 宿主已经在调用期维护线程局部上下文（`install_context`），
   句柄 map 挂在同一层：调用进入时为空，调用退出时清空。
2. **调用退出的四条路径都要收**：正常返回、返回错误、guest panic（trap）、宿主取消
   （harness 的 CancellationToken，§6.30 那轮已把它接进 `HostCaps::cancel`）。
   实现上照 `exec` 的 `run_with_limits`：独立进程组 + 结束时 `kill(-pgid)`，只是这里在调用收尾时对**全部**句柄做一遍。
3. **guest 不 close 也不会泄漏**：调用结束即收，所以「不 close」的后果是「本次调用内多占一个子进程」，
   不是永久泄漏。
4. **并发上限**：一张调用内同时存活的句柄数上限，**已定 100**（超限 `denied` 并说明理由）。
   照实说：这不是沙箱替你兜的资源，100 个子进程吃的是本机的进程表与内存。
5. **不跨调用、不跨会话**：会话切换/插件卸载时不会有遗留子进程（因为调用结束就没了）。
   这是本能力与「会话级句柄」的分界线，也是它敢先做的原因。
6. **guest 观察不到「取消」**（wasm 通道没有取消通道，`HostCaps::cancel` 只给编译期插件）。
   取消的语义因此落在调用层：宿主取消 → 调用终止（guest 连同它的等待一起消失）+ 收尸杀组。
   guest 侧不需要（也无法）配合——它自己设的 deadline 是第二道保险，不是唯一一道。

## 6. 授权与强制（与 `exec` 共用一套实现）

新可授权能力 `proc`，policy 形状与 `exec` 同构（复用 `ExecCwd` 与 `allows()`）：

```json
{ "commands": ["codegraph"], "cwd": "project", "timeout-seconds": 60, "max-open": 4 }
```

- **命令**：裸名、白名单、精确匹配（与 `exec` 同一函数、同一条「必须不是路径」的检查）。
- **cwd**：`plugin-data`（默认）| `project`，与 §6.30 完全同一对 `jail_under` / `symlink_checked_under`。
- **环境**：清空后只回填 `PATH`（与 `exec` 同）。
- **超时**：`proc` 的预算**上界**是单次子进程的墙钟预算。已定：**默认 600s**。注意这**超过**
  `exec` 的 300s 上限（`EXEC_TIMEOUT_MAX_SECONDS`），所以 `proc` 用自己的常量
  （`PROC_TIMEOUT_DEFAULT_SECONDS = 600`，cap 取 **1800s**：与 orchestrator 的子代理上限同值，
  且给默认值留出上调空间；要改成 600 就改一个常量）；
- **并发**：已定 **同时最多 100 个句柄**。照实说清代价：这是「一个 guest 能在一次调用里同时开
  100 个子进程」，宿主会照此放行——它吃的是本机进程/内存，不是沙箱能替你兜的资源；
  另外整个工具调用仍受宿主既有约束（不是无限持有）。
- **输出**：按行上限 + 每句柄总预算（防止 guest 用 `read-line` 把宿主当无限缓冲读）；`write` 同理有缓冲上限。
- **CLI**：`ki plugin grant <id> proc --command codegraph --cwd project --timeout 60`；
  `ki insmod`/安装期的声明提示把 `proc` 纳入（`GRANTABLE`/`ENFORCED_CAPABILITIES` 两处列表同步）。

## 7. 契约兼容与协商

- **加法**：`0.6.0 → 0.7.0` 只新增一个 interface，已装的 0.6.0 组件不受影响（它们不 import `proc`，
  组件模型会剪掉未使用的 import）。
- **反向**：一个 import 了 `proc` 的组件在**旧宿主**上会**实例化失败**（链接不到 import）——
  这正是「要求更新的宿主」的表达。安装期诊断要能说人话：
  `declared_capabilities` 把 `proc` 报出来，宿主不认时按现有「本构建无法执行的能力」那条路径提示
  （`ki plugin install` 已有这段逻辑：`capability_is_enforced`）。
- **文档**：`plugin-abi.md` 加一轮 §6.32；handover §3 能力表加 `proc` 行；CHANGELOG 记 ABI 版本。

## 8. 触达与验收（§6.3 门禁逐条落地）

| 门禁 | 本能力的落地 |
|---|---|
| 面覆盖断言 | conformance fixture 新增 `proc` 触达：起一个脚本子进程、写一行、读一行、等、杀 |
| 能力拒绝双态 | 未授予 → `denied`；命令不在白名单 → `denied` 且理由指名；cwd 越狱 → `failed`（与 `exec` 同款） |
| 隔离 | 收尸测试：调用返回后进程组**确已死**（照 `exec_does_not_hang_when_the_command_leaves_a_background_child` 的写法，用 `sh` 后台子进程验证整组被收） |
| 泄漏 | 「不 close 也不泄漏」：调用结束即收，测试断言宿主句柄表清空 + 子进程不存在 |
| 取消 | 宿主取消（CancellationToken）→ 调用内的子进程被杀，与 `exec` 的取消路径同测 |
| 性能 | 一次往返的额外开销出数字（对齐 §6.3 第 8 条的门禁 8 基线） |

**零网络、零外部依赖**：测试用 `sh -c 'read line; echo "reply:$line"'` 这类脚本当「假 MCP 服务」，
既能验证往返语义，又全程 hermetic。

## 9. 明确不做（边界）

1. **不给 `rpi-orchestrator` 用。** 它要的是「调用之间还在跑」——那需要宿主拥有调度权
   （DAG 引擎搬进宿主、子代理生命周期由宿主管、1800s 长活与进程组语义在宿主侧定义），
   是一次**重新设计**而不是给插件加一层能力。本方案不做，也不假装它能。
2. **不做会话级句柄**（跨调用持有）。理由同上：跨调用的存活意味着宿主要对「会话结束时收谁」负责，
   与调用级收尸是两种不同的错误面。将来若真要，另立方案。
3. **不做 PTY、不做任意 shell、不做二进制流协议**（§3.3）。
4. **不顺手改 `exec` 的既有语义**（它的一次性是有意为之的承诺）。

## 10. 批次拆分与工作量

| 批次 | 内容 | 产出 | 规模（估） |
|---|---|---|---|
| **1** | WIT `proc` + 版本 0.7.0；宿主实现（句柄表/收尸/上限）；`grants` 加能力 + CLI flags；conformance 触达；e2e（往返/拒绝/收尸/取消/泄漏）；文档 §6.32 | 能力可用，暂无插件使用 | 中（WIT 小、宿主 ~250 行 + 测试） |
| **2** | 迁 `rpi-codegraph` → `plugins/wasm/kiqi-codegraph-guest`：`init`/`sync` 走 `exec`、`search` 走 `proc`；差异清单（Windows 上 `codegraph` 的 powershell shim 解析在 wasm 里做不到，裸名规则下会失败——文档写明）；e2e；把它移出默认构建 | 第四个「无 wasm 版」的编译期插件退场 | 中 |
| 3（另议） | 会话级句柄 + 宿主持调度（为 orchestrator 之类） | —— | 大，且是重新设计 |

批次 1 与 2 各自可独立合并、可回退；批次 1 就算批次 2 不做也有价值（将来任何「壳一层外部 CLI」的插件都用得上）。

## 11. 已定（2026-10-15）

1. **能力命名**：`proc`。同构的 grant 形状照 `exec`（§3.1 的「同一程序在两个 grant 里各批一次」
   被接受）。
2. **默认值**：子进程墙钟预算 **600s**（cap 1800s，`proc` 自己的常量——600 已超 `exec` 的 300s 上限，
   不能复用后者）；同时最多 **100** 个句柄。代价见 §6。
3. **跟踪方式**：**不建任何上游 issue**——`bigfish1913/pi-rust` 已分叉且与本项目无关，
   该事实已写入 `AGENTS.md`（本项目没有上游）。跟踪落在本方案文件 + 提交信息上。

> 批次 1 的落地范围见 §10；`AGENTS.md` 另记了本地门禁与两条插件通道的约定。

## 12. 批次 3 状态（2026-10-15，进行中）

**已落**：`proc.close-stdin`（ABI 0.7.1，分支 `feat/orchestrator-wasm`）——任务走 stdin 的子
进程需要 EOF 信号；宿主实现为弃写端（写线程自然收尾），幂等。

**设计已定、实现待续**（下一次会话从此继续，勿重开设计）：
- guest `plugins/wasm/kiqi-orchestrator-guest`：图/计划/清单逻辑自带副本（lens/goal 先例）；
- dispatch 形态 = **阻塞波**：一次调用内起齐 ready 节点（并行由 `proc` 的 max-open 承载，
  轮询各子进程 stdout 到 EOF 后收尸提交），`waitSeconds:0` 明确报错（子进程不能跨调用存活），
  `waitSeconds:N` 到点杀余、节点记失败；每节点 `timeout_secs` 自带期限（轮询时检查、超时杀组）；
- `expectFile`/`verify` 走 `exec`（`test -f`/`sh -c`，README 记入 exec 白名单要求）；
- 子代理程序默认 `ki`（PATH 解析，无 current_exe），参数 `program` 覆写 = hermetic 测试缝合点
  （e2e 用 `cat` 回显任务）；`maxTurns` 记录不强制（env 无 proc 等价物）；
- 指引恒注入（guest 无项目 fs、无 env——goal 联动与 KI_ORCHESTRATOR_GUIDE 均不可表达）；
- 会话切换清图保留（before_agent_start 解析 sessionId）；
- **默认构建翻转为仅 `rpi-permissions`**（orchestrator 移出默认），CI opt-in 清单加
  `plugin-rpi-orchestrator`，清单测试同步门控。
- 教训一条（写给下次）：批量编辑脚本必须逐条落盘——统一落盘 + 中途断言失败 = 静默丢整批改动
  （本次 orchestrator guest 的 900 行工作文件就是这样丢的）。
