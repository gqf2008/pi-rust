# 给重写 agent 的交接说明

你接手的是 `ki`（Rust 编码代理）的 **Zig 重写**。本目录不是重写的成品，而是从 Rust 版
真实故障里提炼出来的控制面骨架：**照抄结构、保留不变量、适配 API 名**，不要重新发明。
本地工具链与重写目标一致：**Zig 0.17.0**。

## 1. 开工前的阅读顺序

1. [`../porting-traps.md`](../porting-traps.md) — 33 条陷阱，按控制面组织
   （§1 ACP/控制面、§2 MCP/连接生命周期、§3 权限门、§4 wasm/插件层、§5 工程与验证、
   §6 最小测试集）。这些不是风格建议，每一条都对应 Rust 版的一次真实故障或一次
   对抗审查挖出的洞。
2. [`README.md`](README.md) — 两个骨架各固定了哪些不变量，以及 0.17 的实测 API 面
   （见「0.17 surface actually used」一节）。
3. 本文件 — 立场、语义真源、验证方式。

## 2. 两个骨架：直接用

| 文件 | 内容 | 已验证 |
|---|---|---|
| [`turn_dispatch.zig`](turn_dispatch.zig) | ACP 回合分发：同步抢 `in_flight`（:13）→ spawn worker（:70）→ 移交 responder；`cancel()` 空闲时无操作（:24）；`run()` 在首步之前与步与步之间检查取消（:48） | `zig test` 2 passed（:91、:101） |
| [`process_tree.zig`](process_tree.zig) | 进程树收整：`spawn` 父子两侧 `setpgid`（:19）；`groupAlive` 用 `kill(-pgid, 0)` 探测（:32）；`killTree` = TERM 组 → 宽限轮询 → KILL 组 → 回收直接子进程（:40） | `zig test` 1 passed（:61） |

改 API 名可以，**改不变量不行**：

- 回合：取消可能比 run 注册先到，必须由 `begin()` 的「先置 `in_flight` 再清 `cancelled`」
  兜住——这就是 Rust 版里被迫用 100 ms 重发 abort 的那个 `NoActiveRun` 窗口的根因修法。
- 进程树：直接子进程**未回收**时它的 pgid 不会失效，收整组必须在这个窗口里做；
  先 SIGTERM 给宽限，再 SIGKILL，最后 `waitpid`，否则孙进程活着、直接子进程变僵尸。

## 3. 0.17 实测面（不是推断，是编译器报出来的）

完整对照表见 [`README.md`](README.md) 的「0.17 surface actually used」一节。要点：

- `std.posix` 的 `fork` / `setpgid` / `execveZ` / `waitpid` / `exit` 全部移除 → `std.c.*`
  （`std.c.waitpid(pid, ?*c_int, options)`、`std.c.W.NOHANG`）。
- 睡眠归 `std.Io`；骨架用空 pollfd 集 + `std.posix.poll` 过渡。
- 数组重复 `T{v} ** N` 已删除 → comptime 填充用 `@splat`。
- `std.posix.kill` / `SIG` / `W` / `poll` **仍在**，`groupAlive` 保留带类型的
  `KillError.ProcessNotFound` 判断，不要退化成 errno。
- 子进程一律**显式传 `envp`**（最小白名单），绝不继承 provider key 或代理变量。

原则：进程控制层 = libc 绑定 + `kill`/`poll`；所有 fd 形态的东西（pipe / Child / 时钟 /
睡眠）走 `std.Io`。层越薄，API 迁移越碰不到不变量。

## 4. 架构立场（不要搞出第三条通道）

按仓库 `AGENTS.md` 的裁决：

- **宿主持有进程级能力**：ACP、MCP、permissions（策略存宿主、在工具门求值）。
- **插件全部是 wasm guest**（契约 `plugins/kiqi-plugin.wit`，跑 wasmtime 沙箱，按授权拿能力）。
- Rust 的编译期 `kiqi_plugin::Plugin` trait **只是 Rust 侧的扩展面**，不是要在 Zig 里
  复刻的第三层级；默认构建已不编入任何编译期插件。

## 5. 语义真源（别在本目录里猜行为）

| 领域 | 文档 |
|---|---|
| ACP 宿主级服务的取舍与批次记录 | [`../ki-acp-serve.md`](../ki-acp-serve.md) |
| MCP：传输、订阅、通知边界、信任 | [`../ki-mcp-builtin.md`](../ki-mcp-builtin.md) |
| wasm 契约与能力模型（唯一真源，§7 是待定项） | [`../plugin-abi.md`](../plugin-abi.md) |
| 插件层现状/能力表/已知限制 | [`../plugin-abi-handover.md`](../plugin-abi-handover.md) |

权限策略的两条硬约束别漏：规则语义是 glob + deny 优先 + 无 allow 列表即全放行；
**策略文件本身不可被 agent 写**（否则 agent 能改自己受审的规则）。

## 6. 怎么验证

```bash
zig test turn_dispatch.zig
zig test process_tree.zig
zig fmt --check turn_dispatch.zig process_tree.zig
```

全部应通过（2 + 1 个测试）。若某条不变量被改动，先让测试变红，再决定是否真的要改。

## 7. 提交历史

- `5951279` — porting-traps 清单
- `25e85fe` — 两个骨架（首次落地）
- `97fc80f` — 0.17 适配（`**` → `@splat`）+ README 实测面
- `84b36d8` — 删除被取代的合稿草稿
