# 方案：权限策略的后台写入与 OS 沙箱

> 状态：**macOS 已落地（2026-10-20）**。词法守卫 + 调用前后快照/恢复 + macOS
> `sandbox-exec` 三层；Linux（Landlock）仍是后续项。本文件记录实现边界和残余。
> 关联：`crates/kiqi-cli/src/permissions.rs`、`crates/kiqi-tools/src/sandbox.rs`、
> `docs/design/signal-driven-agent.md`。

## 1. 现在已经覆盖的

| 机制 | 位置 | 覆盖 |
|---|---|---|
| 词法 shell 守卫 | `crates/kiqi-cli/src/permissions.rs` | 明确拼写的删除/覆盖/重定向/包装器 |
| 调用后恢复 | `PolicySnapshot` / `PolicyGuard`（`permissions.rs`） | shell 命令在返回前改写的策略文件 |
| 跨调用基线 | `PolicyGuard::check_baseline` | 两次工具调用之间（含后台进程）写坏策略 |
| 合法编辑识别 | 每文件写代数（`save` 递增） | `permissions` 工具的合法修改不被基线覆盖 |
| macOS OS 沙箱 | `kiqi-tools/src/sandbox.rs`（`ShellSandbox`） | 每次 `bash` 子进程树都被 `sandbox-exec` profile 限制：策略文件与所在目录只读，其余 `(allow default)` |

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

## 4. 已实现：macOS `sandbox-exec`

`BashToolOptions` 增加 `sandbox: Option<ShellSandbox>`。`bash_options(cwd)` 在
macOS 上默认构造它：

- 保护路径来自 `permissions::shell_sandbox_paths(cwd)`：本地 `.ki/permissions.json`、
  `.pi/permissions.json`、全局 `~/.ki/agent/…/permissions.json`，以及它们所在的目录。
- profile 为 `(version 1) (allow default) (deny file-write* (literal "…"))`。
  `literal` 只匹配路径本身，不匹配子项：`.ki/sessions`、`.ki/goals.json` 仍可写，
  但 `permissions.json` 本身、`mv .ki …`、`rm -rf .ki` 会被拒绝。
- 路径先 `canonicalize`（`/var` → `/private/var`），因为 `sandbox-exec` 匹配解析后的
  vnode；策略文件尚不存在时解析其父目录。
- shell 覆盖为 `/usr/bin/sandbox-exec -p <profile> /bin/bash -c <command>`，
  由现有 `ShellOverride` 拼进 `OsExecutionEnv`，不新增执行路径。
- `KI_SHELL_SANDBOX=0` 显式关闭；非 macOS 或找不到 `sandbox-exec` 时
  `macos_for` 返回 `None`，行为退回前两层。
- 启动时先探测能否真正 apply 一个 profile：**在已有沙箱内再套 `sandbox-exec` 会
  `sandbox_apply: EPERM`**（外层只要有任何 `deny file-write*` 规则就会触发，
  见 `an_outer_deny_rule_blocks_nested_sandbox_apply`）。探测失败时返回 `None`，
  由外层沙箱继续约束；这样从 `ki` 的 bash 里再启动 `ki` 不会让每条命令都 spawn 失败。

验收测试：`crates/kiqi-tools/src/sandbox.rs`（profile 与直接执行）和
`crates/kiqi-tools/tests/bash.rs::the_os_sandbox_wraps_the_real_bash_tool`
（走真实 `BashTool` → `OsExecutionEnv`，验证普通命令可跑、策略写/删被拒）。

残余（有意保留）：

1. **硬链接/重命名竞态**：沙箱按路径拒绝，不能按 inode 拒绝；一个已有目录写权限的
   进程可先把策略 `ln` 到别处再写。这是路径沙箱的边界，不是词法守卫能补的。
2. **Linux**：尚未接 Landlock；Linux 上仍是前两层。
3. **非 bash 的写入面**：宿主自己的 `write`/`edit` 工具仍由词法/路径检查保护；
   沙箱只包 `bash` 子进程树（`powershell` 仅 Windows，有独立执行点）。
4. **嵌套目录**：嵌套 `ki` 已由外层沙箱约束，但它若换了工作目录，新目录的本地
   `.ki/permissions.json` 只受词法守卫与调用后恢复保护——外层 profile 不能追加规则。

## 5. 当前结论

macOS 上「后台写入/新会话」残余已由 OS 沙箱关闭：命令返回后，即使留下后台进程，
它也不能写策略文件或把策略目录搬走。Linux 仍保持文档化的残余，待 Landlock。
不引入持久化哈希恢复，避免误杀用户的外部编辑。
