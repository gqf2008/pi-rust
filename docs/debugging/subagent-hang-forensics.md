> 注（2026-10-21）：编译期一方插件通道已退役；文中 `extensions/kiqi-*` 路径与 `extension-kiqi-*` feature 是历史记录，现行实现在 `extensions/wasm/kiqi-*`。

# 「subagent 挂起后父 run 无法恢复」排查记录

排查日期：2026-10-19 ~ 2026-10-20
涉及仓库：`pi-rust`（`extensions/kiqi-orchestrator`、`crates/kiqi-cli`、`crates/kiqi-harness`）

本文记录两次真实挂起的现象、已经**排除**的假设、已经**修掉**的无界等待，以及
**尚未定位**的部分。目的与 `llm-repetition-forensics.md` 相同：给一份可复核的断点
清单，下次出现同类症状时照着逐段验证，而不是从零再猜一遍。

> 状态：**未结案**。已知一个真实的无界等待已被消除（§4），但端到端的根因没有复现，
> 因此不对根因下结论。

---

## 一、症状（实测记录）

两次，形态一致：

| 观测 | 判据 |
|---|---|
| 主会话停在最后一条 `tool_started`，此后零新条目 | 会话文件 mtime 与大小不再变化；第一次挂起 17 分钟、第二次 29 分钟 |
| 子进程 CPU 0.0%、TIME 约 1.1s、11 个线程中 10 个 `__psynch_cvwait`、1 个 `kevent` | `sample <child> 2 1` |
| 子进程无网络、无子进程、持有 5 个 PIPE | `lsof -p <child> -a -i` 为空；`pgrep -P <child>` 为空 |
| **SIGKILL 子进程后父 run 仍不恢复** | `kill` 无效 → `kill -9` → 子进程变 `Z`（僵尸），父进程仍 `R+` |
| 僵尸只在父进程退出后才被回收 | 对父进程 SIGTERM 后 `Z` 消失 |
| TUI 仍渲染（spinner 在动），但按键无响应 | 目测 + 当时无 `KI_DEBUG_KEYS` 记录 |

最后两条是**最有价值的线索**，也是本文的重点：父侧的卡点不在「等子进程退出」这一个点上。

---

## 二、已排除的假设

每条都有反向证据，不要重复怀疑。

### 2.1 「插件锁把父侧卡住了」

**排除**。`ExtensionHost` 没有任何 `Mutex`：

- `crates/kiqi-cli/src/extensions.rs:836-856` 的 `struct ExtensionHost { id, extension: Box<dyn Extension>, untrusted }`
- 所有方法都是 `&self`，`call_tool` 直接 `self.guard_result("call", ...)`

而且工具调用本身跑在独立线程上（`crates/kiqi-cli/src/extensions.rs:574-592` 的
`spawn_extension_call` 用 `std::thread::Builder`），宿主持有的是 `oneshot::Receiver`。

### 2.2 「`inner` 锁跨 `.await` 持有」

**排除**。全文件 46 处 `self.inner.lock()`，用作用域分析扫过，唯一两处看起来跨
await 的（`agent_harness.rs:3195`、`:3929`）实为同一行内取用：

```rust
let recovery_hook = self.inner.lock().unwrap().before_tool_call.clone();
```

守卫在语句结束即释放，后续 `.await` 不持锁。

### 2.3 「stdin 写入把父侧堵住」

**排除**。`write_stdin` 是无取消检查点的同步写，但 task 上限是 20 KB
（`extensions/kiqi-orchestrator/src/graph.rs:14` 的 `MAX_TASK_BYTES`），远小于管道容量，
不会背压。

---

## 三、已修：一个真实的无界等待（`join_reader`）

**这是唯一一条能直接解释「子进程已死、父侧不返回」的代码路径。**

原实现（`extensions/kiqi-orchestrator/src/spawn.rs`）：

```rust
fn join_reader(handle: Option<JoinHandle<String>>) -> String {
    handle.map(|h| h.join().unwrap_or_default()).unwrap_or_default()
}
```

reader 线程只在管道 EOF 时返回。若子进程已死但仍有进程持有写端，`join()` **永久阻塞**，
且这个 join 发生在 `kill_and_reap` 之后、任何取消检查点之外。

修复（`15273e3`，现 `extensions/kiqi-orchestrator/src/spawn.rs:572`）：reader 写入共享
sink（`OutputSink`），调用方有界等待 `READER_JOIN_TIMEOUT = 5s`，超时后保留已读
字节、丢弃 JoinHandle。

诊断（`13fc9a2`）：超时不再静默——`join_reader` 返回 `(String, bool)`
（`spawn.rs:572`），阶段日志记 `reader-join-timeout stream=<Out|Err> waited=<dur>
bytes=<n>`（格式见 `spawn.rs:568` 的 `reader_join_timeout_line`）与 `readers-abandoned`。

### 3.1 为什么不能声称「这就是根因」

我尝试端到端复现，**没有成功**：

1. 用 `setsid` 造脱组孙进程——**macOS 没有 `setsid`**，脚本报 `command not found`、
   什么都没持有，测试于是「通过」。这是一次假绿（详见 §5）。
2. 改用 `python3 -c 'os.setpgrp()'`：确认脱组进程能存活并持有管道（`ps` 显示其
   pgid 等于自身 pid），reader 确实会阻塞在它上面。
3. 但走 `run_with_exe` 时，`kill_descendants` 的 `pgrep -P` 在 reparent 之前就抓到了
   它，所以端到端没挂。

结论：`join_reader` 的无界等待是**真实的缺陷**，但现有证据不足以证明它就是两次事故的
那条路径。修掉它是对的，声称「已修好事故」不是。

---

## 四、未定位：父侧卡点在哪

`SIGKILL` 子进程后父 run 仍不恢复，说明卡点**不在**等子进程退出。剩下的候选：

| 候选 | 如何验证 |
|---|---|
| reader 阻塞在管道（已修，需确认是否仍可发生） | 阶段日志出现 `readers-abandoned` 即为命中 |
| 父侧 `wait`/reaper 路径 | 阶段日志停在 `wait` 且无 `reaped` 行 |
| TUI 按键线程死亡导致无法交互（渲染仍在跑） | 见 §5.1：需要确认按键循环 panic 后是否会静默退出 |

### 5.1 一个尚未验证但机制成立的假设：按键线程 panic

TUI 的按键循环跑在 `tokio::task::spawn_blocking`（`crates/kiqi-cli/src/interactive_tui.rs:1747`），
其 handle 在退出路径上以 `let _ = key_handle.await;`（`:3337`）等待——**panic 被静默吞掉**。
若某个按键处理器 panic 并 unwind 出循环，该 task 死亡，此后**没有任何按键会被分发**，
而渲染是独立路径，仍会继续刷新。

这与「spinner 还在动、按键不处理」逐条吻合。但：

- 没有当时的 `KI_DEBUG_KEYS` 记录，无法证明按键是「没到达」还是「到达但没被处理」；
- 我写过一个单元测试来「证明」它，但那个测试只是复述我自己构造的模型、没有约束任何
  生产代码，等于装饰。已删除，不留在仓库里冒充证据。

**下次复现请同时开** `KI_DEBUG_KEYS=1`（记录按键是否到达 TUI）与
`KI_DEBUG_SUBAGENT=1`（记录 spawn 阶段），两者对照即可区分上表的三行。

---

## 五、本次排查中的假绿（方法教训）

同类错误在本仓库已出现四次，形状一致：**报告成功，但什么都没验**。逐条留档。

| 假绿 | 原因 | 后果 |
|---|---|---|
| `setsid` 脱组测试 | macOS 无此命令，脚本失败但测试仍「通过」 | 以为复现了挂起，其实什么都没持有 |
| 陈旧二进制 | worktree 与主 checkout 共用 `CARGO_TARGET_DIR`，源文件 mtime 早于缓存产物，cargo 判 fresh 复用 | `cargo build` 0.64s 未编译，`strings` 里找不到新代码 |
| `grep -o 'extension-kiqi-[a-z0-9-]*'` | 字符类含 `-`，grep 在连字符处切断，输出裸 token；`-f` 模式文件里一个空行就让整个查找失效 | 特性清单守卫永不失败 |
| CI 命令续行 | `cargo build --locked \` 换行，而 grep 在同一行找 `-p` | standalone 清单取到空，比对恒等 |

**统一教训**：校验器与复现脚本自身必须被一个「应当失败」的用例验过。
验不过就等于没有。

---

## 六、复现时需要收集的东西

```bash
# 1. spawn 阶段（卡在 wait 还是 readers-joined）
KI_DEBUG_SUBAGENT=1 ki ...

# 2. 按键是否到达 TUI
KI_DEBUG_KEYS=1 ki ...

# 3. 挂住时：父侧与子侧各采一次栈
sample <parent_pid> 2 1
sample <child_pid> 2 1

# 4. 管道归属：谁还持有子进程的 stdout/stderr 写端
lsof -p <child_pid>
pgrep -P <child_pid>

# 5. 僵尸与父侧状态
ps -o pid=,ppid=,stat=,command= -p <child_pid>,<parent_pid>
```

判据对应关系：

- 日志停在 `wait`、无 `reaped` → 父侧 wait/reaper 路径
- 日志有 `readers-abandoned` → 管道被无法触达的进程持有（本仓库已加 5s 界，不会再挂死）
- 按键日志在挂起后仍增加 → 按键到达但分发被阻塞
- 按键日志在挂起后停止增加而 spinner 仍在动 → 按键线程已死（§5.1 假设）
