> 注（2026-10-21）：编译期一方插件通道已退役；文中 `extensions/kiqi-*` 路径与 `extension-kiqi-*` feature 是历史记录，现行实现在 `extensions/wasm/kiqi-*`。

# 方案：ask_user 兜底与 JEV 决策顾问

> 状态：**阶段 1a 已落地（2026-10-20，`886aac7` + `6e2b890` + `20eed47`）**；1b（goal 自动授权）与阶段 2/3 待做。
> 目标：人不在时 `ask_user` 不再无限挂起；
> 超时后由宿主执行**保守兜底**，并可调用 TypeSafe Jev 决策模型给出**建议**。
> Jev 只建议，不代替人授权。
> 关联：`soft-interrupt.md`（Esc 软中断）、`signal-driven-agent.md`（MUI 输入）、
> `docs/design/extension-abi-handover.md`（插件面）。
>
> 来源与状态：本文件是第 2 轮（软中断阶段 1）实现期间产生的设计稿，**不是**该轮开卡的
> 工作项，因此未随 `4902ab7` 进入 main；方案本体同时登记在 walgit 线程
> `decision-fallback-design`。§7 的四条实核已于 2026-10-20 完成，结论见 §7.1。

## 1. 问题（实核）

- `ask_user` 的存在形态：插件在 `call` 里 open → poll 等待用户，宿主 TUI 弹框；
  不回答就一直等（`extensions/kiqi-ask-user/src/lib.rs` 的 poll 循环）。
- 观测到的事实：2026-10-19 19:56 的删除授权问题把整轮 run 阻塞了约 43 分钟，
  会话文件没有任何新条目；人回来后回答，run 才继续。
- 目标：**人不在时流程不能无限挂起**。非 goal 模式不自动授权危险动作；
  goal 模式只在 goal 范围 + 既有权限策略内自动授权，边界外交给 Jev，Jev 判断不了就
  `unanswered` + 保守兜底。

## 2. 目标 / 非目标

| | 项 | 本方案 |
|---|---|---|
| ✅ | `ask_user` 有宿主级 TTL：非 goal **60 秒**；goal active **不等人工** | 阶段 1 |
| ✅ | goal active 在 goal 范围 + 权限策略内自动授权；范围外走 Jev | 阶段 1/2 |
| ✅ | 非 goal 超时、或 goal 边界外 Jev 判断不了 → `unanswered` + 保守兜底 | 阶段 1 |
| ✅ | 超时/提问前可由 Jev 给出建议 + 概率 + confidence | 阶段 2 |
| ✅ | pending decision inbox：问题、Jev 建议、兜底动作持久化，人回来可覆盖 | 阶段 2 |
| ✅ | 所有兜底/建议可审计 | 阶段 1 |
| ❌ | 让 Jev 绕过权限门，或批准超出 goal 范围的动作 | 永不 |
| ❌ | 用 Jev 替代确定性门禁（验签、去重、权限规则） | 永不 |
| ❌ | 在 `ask_user` 之外新增一个“聊天式审批”入口 | 不涉及 |

## 3. Jev 是什么（本方案的事实基础）

TypeSafe Jev（System One）是结构化决策模型，不是聊天模型：

- 端点：`POST https://openrouter.ai/api/v1/systemone`
- 文本模型：`typesafe/jev-1.13`；图片/box：`typesafe/jev-1.14`
- 请求：`state`（上下文）+ `questions`（choice / noul / score / box）
- 响应：每个问题的带类型答案 + 概率分布 + `confidence`；输出免费，按输入 token 计费
- 本机技能：`~/.agents/skills/jev-decision`（`scripts/jev.py`），已验证可用：
  ```json
  {"type":"choice","choice":"A","probabilities":{"A":0.88,"B":0.12},"confidence":0.76}
  ```
- 边界：概率决策器，同一输入可能给不同答案；调用结果要按输入摘要缓存；
  解析失败/缺键时走保守分支。

## 4. 分层设计

### 4.1 宿主 TTL（阶段 1）

`ask_user` 按会话模式走不同路径：

| 模式 | 行为 |
|---|---|
| **非 goal** | 最多等 **60 秒**；超时返回 `unanswered`，执行保守兜底 |
| **goal active** | **不等人工**：先按 goal 范围与既有权限策略自动授权；不在范围内交给 Jev；Jev 判断不了就走 `unanswered` |

- 模型可显式传更短的 `timeout`；宿主上限是 60 秒（非 goal）。
- 超时后宿主关闭对话框，插件返回结构化结果：
  ```json
  {"status":"unanswered","reason":"timeout","fallback":"keep"}
  ```
- 模型看到 `unanswered` 后继续；**不得**把超时当成“用户同意”。
- 默认兜底按问题类型：
  - 删除/合并/关闭/越权类 → `keep`（保留现状）
  - 可逆选择 → 问题自带的 `fallback`，没有就 `keep`
  - 纯信息问题 → 可返回 `unknown`，由模型自行处理

### 4.2 Jev 顾问（阶段 2）

超时或提问前，宿主构造一次 Jev `choice` 请求：

```jsonc
{
  "state": {
    "goal": "目标标题 + 当前进度",
    "task": "当前 run 在做什么",
    "question": "ask_user 的问题",
    "options": { "keep": "全部保留", "delete-all": "全部删除", ... },
    "policy": "本项目允许/禁止的动作摘要",
    "recent": "最近几条关键事件（有界）"
  },
  "questions": {
    "advice": {
      "type": "choice",
      "instructions": "人不在时，最符合目标与安全策略的选择是什么？",
      "criteria": { "keep": "...", "delete-all": "..." }
    },
    "risk": {
      "type": "noul",
      "instructions": "这个动作是不可逆/越权的吗？"
    }
  }
}
```

输出示例：`advice.choice = "keep"`，`probabilities`、`confidence`；`risk.noul` 给出风险概率。

决策规则（按模式）：

**非 goal**

| 条件 | 行为 |
|---|---|
| 等待 60 秒内人工回答 | 用人工答案 |
| 超时，且 `risk.noul` 低、`confidence ≥ 阈值`、选项在白名单 | 应用 Jev 建议，并记录“Jev 建议+置信度” |
| 超时，`risk.noul` 高 / `confidence < 阈值` / Jev 失败 | `unanswered` + 保守兜底，写入 pending decision |
| 同一输入摘要已决策过 | 使用缓存，不重复调用 |

**goal active**

| 条件 | 行为 |
|---|---|
| 问题与选项在 goal 声明的范围/既有权限策略内 | **自动授权并执行**（记录“goal-scope auto”） |
| 超出范围，或涉及删除/合并/强制推送等不可逆动作 | 调 Jev；`risk.noul` 低且 `confidence ≥ 阈值` 时按建议执行；否则 `unanswered` + 保守兜底 |
| Jev 失败/缺键/未知选项 | `unanswered` + 保守兜底，写入 pending decision |
| 同一输入摘要已决策过 | 使用缓存 |

**授权边界**：goal 模式的“自动授权”只覆盖 goal 明确声明、且被权限策略允许的动作；
Jev 的答案只作为决策依据，不能绕过权限门。删除/合并类如果 Jev 也给不出高置信低风险，
就回到 `unanswered`，不自动执行。

### 4.3 Pending decision inbox

- 存储：会话值（`ki.value`），键 `ki.pending_decisions`
- 每条：`id, question, options, jev_advice, confidence, fallback, created_at, status`
- 状态：`pending | answered | superseded`
- 人回来后：
  - TUI 显示待决策项；`/decisions` 查看/回答
  - 或下一条用户输入里直接说“选 X”，由 LLM 调 `ask_user` 的 answer 路径处理
- 不阻塞 goal：兜底执行后 run 继续，pending decision 留到边界展示

### 4.4 审计

每次超时/建议/兜底记录一行：

```
[ki decision] ask_user timeout id=... question=... jev=keep p=0.72 conf=0.61
              fallback=keep applied=false (authorization)
```

## 5. 与现有机制的关系

- **soft-interrupt**：Esc 软中断是“人主动停当前动作”；本方案是“人不在时自动兜底”，
  两者独立，可同时存在。
- **MUI**：pending decision 的提醒可以作为一条 MUI 输入在边界注入；不新增唤醒源。
- **goal**：兜底后 goal 继续；不会因为一个未回答的问题停摆。若兜底是 `keep`，
  下一轮 recon 会把 pending decision 作为输入。
- **权限门**：Jev 不参与工具权限判定；工具调用仍走 `permissions`/宿主策略。

## 6. 配置（草案）

```json
{
  "askUserTimeoutSeconds": 60,
  "decision": {
    "goalAutoAuthorize": true,
    "jev": {
      "enabled": true,
      "endpoint": "https://openrouter.ai/api/v1/systemone",
      "model": "typesafe/jev-1.13",
      "keyFile": "~/.openrouter.key",
      "timeoutSeconds": 30,
      "confidenceThreshold": 0.8,
      "riskThreshold": 0.5
    }
  }
}
```

- key 读取顺序与技能一致：`JEV_KEY_FILE` → `~/openrouter.key` → `sqb-private`；不打印 key。
- `enabled=false` 时只有 TTL + 保守兜底，不调用外部模型。

## 7. 实现落点（方向）

1. `kiqi-cli`：宿主侧对话框 TTL（`interactive_tui` / `UiDialogMailbox` / ask-user 桥）。
2. `kiqi-cli/src/decision.rs`：Jev 客户端（reqwest，复用 `kiqi_ai::http` 的 builder）、
   输入摘要缓存、策略判定、审计。
3. `extensions/kiqi-ask-user`（编译期 + wasm 两侧）：`ask_user` 输入增加可选
   `fallback` / `authorization` 字段；超时返回 `unanswered`，不伪造答案。
4. 会话值：`ki.pending_decisions`；`/decisions` 命令。
5. 测试：假 endpoint 的 TTL/策略单测；Jev 客户端协议单测；live 测试 opt-in。

## 8. 分期

| 阶段 | 内容 | 状态 |
|---|---|---|
| 1a | TTL + 结构化 `unanswered` + 保守兜底 + 审计（不依赖 Jev） | **已落地** `886aac7` + `6e2b890` + `20eed47`（见 §8.1） |
| 1b | goal active 范围内自动授权 | 待做；**被 §9 剩余第 1 条阻塞**（“goal 范围”边界未定，见 §8.3） |
| 2 | Jev 顾问 + confidence 阈值 + 缓存 + pending decision inbox | 待做 |
| 3 | `/decisions`、MUI 提醒、策略细分（按工具/路径） | 待做 |

### 8.0 §7 四条实核结论（2026-10-20）

| 检查 | 结论 | 判据 |
|---|---|---|
| poll 循环现状 | `POLL_INTERVAL=25ms`；`deadline` **仅**来自模型传的 `timeout`，不传则 `None` → 无限等；宿主侧无 TTL；超时返 `Err` 而非结构化结果 | 落地前：`extensions/kiqi-ask-user/src/lib.rs:37` 与 `:519-546`；guest `:22` 与 `:309-316`；`crates/kiqi-cli/src` 内对话框路径搜 `timeout` 无命中 |
| 阈值常量是新增还是调整 | **新增**。`timeout` 是模型可传参数（校验正整数），无上限；60s 是全新宿主上限 | 同上 |
| 编译期与 wasm 两侧是否都改 | **是**，两处超时逻辑逐行平行 | 同上 |
| 外网调用默认开关 | 阶段 1 不涉及（无 Jev）；属阶段 2 的 `decision.jev.enabled` | §6 |

额外实核：**wasm guest 无法实现审计**——`extensions/kiqi-extension.wit` 的 `world extension` import 了
`log`/`fs`/… 但**没有** `wasi:cli/environment`，guest 读不到 `KI_DEBUG_DECISION`；`fs` 又限制在
插件自己的数据目录。故审计只能宿主侧，理由写进了 guest 文件头。

### 8.1 阶段 1a 落地记录

| 位置 | 实现 |
|---|---|
| `extensions/kiqi-ask-user/src/lib.rs` | `DEFAULT_ANSWER_TIMEOUT_SECS = 60`；`effective_timeout_secs(requested) = requested.unwrap_or(60).min(60)`；`PendingQuestion.deadline` 由 `Option<Instant>` 改为 `Instant`——**“无限等”在类型上不再可表示** |
| 同上 | 超时返回结构化 `unanswered`（`status`/`reason`/`fallback`/`question`），`Ok(Some(..))` 让 drive 正常完成；`cancel` 仍为 `Err` |
| 同上 | `conservative_fallback(_) -> "keep"` 单一实现；schema 里没有 per-question `fallback` 字段，加它要动两侧 + `additionalProperties:false`，属破坏性变更，另开 |
| 同上 | `truncate_on_char_boundary`：中文问题 3 字节/字，按字节直接切会 panic；有 `"删".repeat(100)` 的测试 |
| 同上 | `KI_DEBUG_DECISION` → `<temp>/kiqi-decision.log`，一行一个超时 |
| `extensions/wasm/kiqi-ask-user/src/lib.rs` | 同样三处逐一对应；`ask_one` 返回 `AskOne::{Answered, Unanswered}`，超时不进 `Err` |
| `crates/kiqi-host/tests/e2e.rs` | `ask_user_honours_timeout` 从「断言 `is_error` + 文案含 timed out」改为「断言非错误 + `details.status==unanswered` + 无 `answer` 键」 |

两条**不可回退**的判断：

1. **超时必须是结果，不是错误。** 它改变模型的后续行为：`Err` 会让模型以为工具坏了并可能重试，
   结构化 `unanswered` 让它带着 `keep` 继续。载荷里**没有** `answer` 键——超时不是同意，
   省略键是模型唯一无法误读的形式。
2. **`deadline` 不能退回 `Option<Instant>`。** 类型上允许 `None` 就允许“无限等”，而“用户在场”
   是很强的假设；做成 `Instant` 后，任何未来改动想恢复无限等都必须显式改类型，会被评审看见。

### 8.3 阶段 1b 为何未做

§9 剩余第 1 条（“goal 范围”的精确边界：goal notes / permissions / allowlist 三者优先级）未定。
在边界未定的前提下实现“范围内自动授权”，等于现造一套策略再声称符合方案。1b 应先解该问题。

该问题已拆到 `goal-scope.md`，并在那里发现一个比“优先级未定”更根本的事实：**权限门是拦截、
不是询问**（`crates/kiqi-cli/src/session.rs:948-958` 返回 `block: true`；
`crates/kiqi-cli/src/permissions.rs:514-551` 只可能答“放行”或“拦下并给理由”），权限路径上
没有任何 `ask_user` 对话框。所以 1b 的“自动授权”**没有被授权方**：规则放行时无需授权，
规则拒绝时没有对话框可自动回答。另：`goal.notes` 是自由文本且宿主视图
`ActiveGoal`（`crates/kiqi-cli/src/goal_drive.rs:24-31`）根本不解析它，不能作为范围来源。
1b 应作为“新增审批通道”单独立项（`goal-scope.md` 的方案 B），先选定方案。

## 9. 已定默认值 / 剩余问题

**已定（2026-10-19）**

1. 非 goal 模式：`ask_user` 最多等 **60 秒**。
2. goal active：**不等人工**。goal 范围 + 既有权限策略内自动授权；范围外交给 Jev；
   Jev 判断不了（低置信/高风险/失败）→ `unanswered` + 保守兜底。
3. 阈值初值：`riskThreshold = 0.5`、`confidenceThreshold = 0.8`。
4. Jev 调用失败/超时：直接 `unanswered`，不自动重试（避免同一决策反复计费/抖动）。

**剩余**

1. “goal 范围”的精确边界：goal notes、permissions 规则、显式 allowlist 三者的优先级；
   本方案默认三者取交集，未声明则只允许非破坏性动作。
2. pending decision 的查看/回答入口：`/decisions` 命令 vs 下一条聊天消息。
3. goal 自建的 worktree/分支清理是否算“范围内”：倾向于算，但仍要求 Jev 低风险 + 高置信，
   否则 `unanswered`。
