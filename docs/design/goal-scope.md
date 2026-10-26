# 方案：goal 范围（解锁 decision-fallback 阶段 1b 的前置决策）

> 状态：**决策稿（2026-10-20），待选型**。本文不改代码，只为 `decision-fallback.md`
> §9 剩余第 1 条（「goal 范围」的精确边界）给出**可选项与建议**，因为该问题不定，
> 阶段 1b（goal active 范围内自动授权）无法开工。
> 关联：`decision-fallback.md` §8.3、`permissions`（`crates/kiqi-cli/src/permissions.rs`）、
> `goal` 插件（`extensions/wasm/kiqi-goal/src/lib.rs`）。

## 1. 为什么原问题问错了

`decision-fallback.md` §9 剩余第 1 条假定「goal 范围」是三者取交集：

> goal notes、permissions 规则、显式 allowlist 三者的优先级

实核后发现这个前提站不住，而且还有一个更根本的事实让 1b 无从落地。

### 1.1 `goal.notes` 是自由文本，且宿主根本不读它

| 事实 | 判据 |
|---|---|
| goal 的持久化字段是 `title`/`status`/`notes`/`started_at`/`updated_at`/`turns_used`/`max_turns`/`deadline_ms`，其中 `notes` 是 `String` | `extensions/wasm/kiqi-goal/src/lib.rs:104-118` |
| 宿主侧视图 `ActiveGoal` 只有 `title`/`started_at`/`turns_used`/`max_turns`/`deadline_ms` | `crates/kiqi-cli/src/goal_drive.rs:24-31` |
| `active_goal()` 解析时**完全不读 `notes`** | `crates/kiqi-cli/src/goal_drive.rs:68-93`（无任何 `notes` 取值） |

结论：把 `notes` 当作范围来源，不只是「语义未定」，而是**执行点上拿不到**。要拿它得先改
`ActiveGoal` 与解析器，那等于新建一个字段通道——既然如此，不如直接建一个结构化字段，
而不是去解析自由文本。

### 1.2 权限规则是机器可判的

| 事实 | 判据 |
|---|---|
| 规则是字符串 glob：`Rules { allow: Vec<String>, deny: Vec<String> }` | `crates/kiqi-cli/src/permissions.rs:26-31` |
| 判定 `decide()`：deny 优先；一旦某条 allow 规则点名了该工具，不匹配任何 allow 的调用即被拒 | `crates/kiqi-cli/src/permissions.rs:240-275` |
| 规则匹配形如 `Tool(pattern)` 的 glob | `crates/kiqi-cli/src/permissions.rs:228-231` |

这部分完全可判，不需要人参与，也不该由 Jev 或 goal 覆盖。

### 1.3 决定性事实：权限门是「拦截」，不是「询问」

| 事实 | 判据 |
|---|---|
| `BeforeToolCall` 回调先跑宿主策略，命中即 `block: true` 返回 | `crates/kiqi-cli/src/session.rs:948-958`（`blocking_reason_for` → `BeforeToolCallResult { block: true, .. }`） |
| `blocking_reason_for` 只可能返回 `None`（放行）或 `Some(reason)`（拦截），**没有第三种「去问人」** | `crates/kiqi-cli/src/permissions.rs:514-551` |
| 权限路径里**没有任何** `ask_user` 调用 | `grep -rn "ask_user" crates/kiqi-cli/src/permissions.rs crates/kiqi-cli/src/session.rs` 仅命中会话对话框邮箱与 `--exclude-tools` 示例，与权限门无关 |
| 交互层没有权限审批提示 | `grep -rn "approve\|Approval" crates/kiqi-cli/src/interactive_tui.rs` 无相关命中（`session.rs` 里的 `--approve` 是项目信任，不是工具审批） |

这说明 1b 的表述「goal active 范围内自动授权」**缺少被授权方**：

- 规则放行 → 本来就通过，不存在「要不要授权」；
- 规则拒绝 → 现在直接 block，**没有任何对话框可以「自动回答」**；
- `ask_user` 是模型主动发起的工具，与权限门是两条互不相干的通道。

因此 1b 不是「把 `ask_user` 的 TTL 改短」，而是「先造一条审批通道，再决定 goal 模式能否
自动回答它」。这是新功能，不是 1a 的自然延伸。

## 2. 可选方案

### 方案 A：不做审批通道，把 goal 范围做成「显式缩小版权限规则」

- goal 新增可选结构化字段（例如 `scope: { allow: [...], deny: [...] }`），语义与
  `permissions` 规则**完全相同**（同一套 glob、同一套 `decide()`）。
- 判定：`effective_rules = permissions ∩ goal.scope`，即两者都必须放行才放行；
  deny 仍然永远优先。goal 只能**收紧**，永不能放宽。
- 不需要任何对话框、不需要 Jev、不需要 TTL 参与。
- 人不在时行为：能过双门的就执行，过不了的就**照旧被拦**（模型看到 block 原因）。

**优点**：实现最小；语义只有一条「交集只会更严」；没有新的信任面；与 1a 完全解耦。
**缺点**：不解决「模型想问一句这能不能做」的场景——但那本来就不是权限门的问题。

### 方案 B：新增审批通道（权限门可「问」），goal 模式允许自动回答

- 规则从二值（allow/deny）扩为三值：`allow` / `deny` / `ask`。
- `ask` 命中时走对话框（复用 `ask_user` 的 UI 通道），goal active 且问题落在
  `goal.scope` 内时**宿主直接回答 yes**，不弹框。
- 需要：三值规则、对话框接入 `BeforeToolCall`（该回调目前是同步 block，改成可等待需要
  仔细处理泵/死锁）、goal scope 字段、审计。

**优点**：真正实现「人不在也能推进会做但有边界」。
**缺点**：这是本仓库最危险的一类改动——把「确定性拦截」换成「可能被自动回答的询问」。
一个把 deny 误写成 ask 的配置就从「拒绝」退化成「无人值守时自动同意」。而且 `BeforeToolCall`
目前是同步决策，改成异步等待对话框会影响整条工具调度路径。

### 方案 C：维持现状 + 只做可见性

- 不改任何判定。goal active 时把「因权限被拦」的记录汇总进 pending decision 列表（阶段 2/3
  的 inbox），人回来后一次看全。
- 优点：零风险。
- 缺点：无人值守时确实推进不了（但这也可能正是期望行为）。

## 3. 建议

**选 A，并把 B 明确记为「不做」。**

理由按重要性排序：

1. **A 让 goal 只能收紧、不能放宽**，这条性质可以用一条断言钉死（`effective` 永远
   ⊆ `permissions`），而 B 的核心风险是「配置写错就静默放宽」，很难用测试兜住。
2. 1b 的真实诉求是「人不在时别无限挂起」，这个已经由 1a 的 60s TTL 解决。A 额外给的是
   「goal 声明过的窄范围之外，无人值守时也别乱动」——这是安全收益，不是功能收益。
3. B 需要改 `BeforeToolCall` 的同步契约，属于会波及工具调度的高风险改动，不该挂在
   「兜底」这个议题下顺带做。若将来确实需要，应单独立项并单独评审。
4. §9 剩余第 3 条（goal 自建 worktree/分支清理算不算范围内）在 A 下有一个干净的答案：
   **只有当 `goal.scope` 显式写了那条规则才算**，否则一律走 `permissions`——不再需要
   「倾向于算」这种含糊表述。

## 4. 若采纳 A，落地形状（供后续开卡）

1. `extensions/wasm/kiqi-goal` + 编译期两侧：goal 增加可选 `scope: { allow, deny }`；
   schema 与 lenient 解析同时改（老文件无该字段 = 不额外收紧）。
2. `crates/kiqi-cli/src/goal_drive.rs`：`ActiveGoal` 增加 `scope`，解析它（这是 1.1 指出的
   必须补的通道）。
3. `crates/kiqi-cli/src/permissions.rs`：新增 `decide_with_scope(rules, scope, tool, target)`，
   实现「两道门都放行才放行」，并保留一条断言：`scope` 永不使某调用从 Deny 变 Allow。
4. 测试：scope 收紧生效；scope 无法放宽 deny；无 scope 时行为与现在完全一致。

### 4.1 成本评估（2026-10-20 实核，不是估计）

A 比预想的便宜：它不需要任何规则合并算法，只是对**已有的纯函数做一次合取**。

```rust
// 形状，不是最终代码
fn decide_with_scope(perms: &Rules, scope: &Rules, tool: &str, target: &str) -> Decision {
    match decide(perms, tool, target) {
        // 权限拒绝先返回：deny 永远优先，scope 无法翻案
        Decision::Deny { reason } => Decision::Deny { reason },
        // 权限放行后，scope 还有一次否决权
        Decision::Allow => decide(scope, tool, target),
    }
}
```

支撑这三行的事实：

| 事实 | 判据 |
|---|---|
| `decide` 是纯函数，只依赖 `&Rules` 与 `tool`/`target` | `crates/kiqi-cli/src/permissions.rs:245` |
| 空规则集**放行一切**（因此空 `scope` ＝ 不额外收紧，与今日行为逐字相同） | `permissions.rs` 的测试 `default_allows_without_allow_list`（约 :820-826），断言 `Rules::default()` 对 `rm -rf /` 也返 `Allow` |
| deny 优先已由函数自身保证，无需在合取层重做 | `decide` 先查 deny 再查 allow 名单（`permissions.rs:245-275`） |

**「只能收紧」是可证的，不是靠测试抽样**：返回 `Allow` 当且仅当两个 `decide` 都返
`Allow`，所以 `scope` 只能把 `Allow` 变成 `Deny`，永远不能反向。

这条不是纸面推理——2026-10-20 用一个一次性探针在 `permissions.rs` 的测试模块里实测过
（探针已还原，未入库），四条断言全绿：

1. **空 scope 与今日行为逐字相同**：对 `bash git status`、`bash git push origin main`、
   `read /etc/passwd` 三种，`decide_with_scope(perms, Rules::default(), …)` 与
   `decide(perms, …)` 完全相等。
2. **放行的 scope 翻不了 deny**：`scope = {"allow": ["*(*)"]}` 时，
   `bash git push origin main` 仍被 `permissions` 的 deny 拦下。
3. **scope 点名工具时确实收紧**：`scope = {"allow": ["Bash(git status)"]}` 下
   `bash git status` 放行、`bash git log` 被拒。
4. **scope 也翻不了 permissions 的 allow 名单**：`scope` 放行一切时，
   `bash rm -rf /` 仍被 `permissions` 的 allow-list 拒绝。

若采纳 A，这四条就是实现的验收断言（写进 `permissions.rs` 的 `mod tests`）。

真正的工作量不在这个函数，而在 §4 的第 1、2 步（goal 两侧 schema + 宿主解析新增
可选字段）。与方案 B 相比，A 尤其不碰 `BeforeToolCall` 的同步契约。

## 5. 待你决定

1. 选 **A**（建议）、**B**（高价值高风险）还是 **C**（零风险但无人值守不推进）？
2. 若选 A：`scope` 是写在 goal 状态文件里（随 goal 生命周期），还是复用 `permissions` 的
   项目文件、只多一条「本 goal 生效」的标记？前者自包含，后者不重复规则语法。
