# 方案：权限策略的后台写入与 OS 沙箱

> 状态：**设计记录（2026-10-20）**。当前已落地词法守卫 + 调用前后快照/恢复；
> 本文件记录仍无法在纯进程内关闭的残余，以及把残余关掉需要的 OS 能力。
> 关联：`crates/kiqi-cli/src/permissions.rs`、`docs/design/signal-driven-agent.md`。

## 1. 现在已经覆盖的

| 机制 | 位置 | 覆盖 |
|---|---|---|
| 词法 shell 守卫 | `crates/kiqi-cli/src/permissions.rs` | 明确拼写的删除/覆盖/重定向/包装器 |
| 调用后恢复 | `PolicySnapshot` / `PolicyGuard`（`permissions.rs`） | shell 命令在返回前改写的策略文件 |
| 跨调用基线 | `PolicyGuard::check_baseline` | 两次工具调用之间（含后台进程）写坏策略 |
| 合法编辑识别 | 每文件写代数（`save` 递增） | `permissions` 工具的合法修改不被基线覆盖 |

## 2. 仍存在的残余

1. **调用返回之后的后台写入**：`some-daemon &`、`nohup ... &`、延迟脚本在
   `after_tool_call` 返回后才写 `.ki/permissions.json`。下一次工具调用会用基线
   恢复它；但如果之后没有工具调用（会话结束/空闲），文件会保持被改状态。
2. **新会话的基线**：新进程的 `PolicyGuard::new` 把当前磁盘内容当作可信基线；
   上一个会话结束后被后台改写的策略会被当作用户的合法现状。
3. **外部编辑无法与攻击区分**：用户本来就可以在会话外编辑策略（错误文案明确
   建议这样做）。因此“持久化哈希并在新会话恢复”会误杀合法编辑，不能作为方案。

## 3. 关闭残余需要的 OS 能力

| 平台 | 机制 | 形态 |
|---|---|---|
| macOS | `sandbox-exec`/`sandbox_init` profile，或 App Sandbox | 给 bash/powershell 子进程树一个 profile：允许工作区读写，拒绝写策略路径（`.ki`、`.pi`、`~/.ki/agent`） |
| Linux | Landlock（unprivileged）或 seccomp | 对子进程树 deny `open(O_WRONLY|O_CREAT|O_TRUNC)`/`unlink` 策略 inode |
| 通用 | 容器/namespace | shell 工具跑在只挂载工作区、策略只读的沙箱里 |

原则：**沙箱是唯一 sound 的关闭方式**；继续在词法层加拼写只能降低概率，不能消除。

## 4. 建议的落地顺序（未实现）

1. 在 `kiqi-tools` 的 shell 执行点引入一个可选的 `SandboxSpec`：策略路径只读、
   工作区可写；默认关闭，配置开启。
2. macOS 先做（目标机器），Linux 用 Landlock；无沙箱能力时回退到当前基线机制，
   并在启动时明确告知残余。
3. 验收：
   - 后台进程 `sleep 1; echo '{}' > .ki/permissions.json` 在沙箱开启时失败；
   - 正常 `cargo test`、`git`、编辑工作区不受影响；
   - 沙箱不可用时行为与当前一致，且 `KI_DEBUG_DECISION`/日志明确说明。

## 5. 当前结论

本轮已把可在宿主内关闭的部分做完（调用前基线、调用后恢复、合法编辑代数）。
后台写入/新会话残余保持文档化；不引入持久化哈希恢复，避免误杀用户的外部编辑。
