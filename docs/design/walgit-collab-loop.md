# 基于 walgit MCP 的全自动协作协议

> 状态：**第 0 轮已闭环**（2026-10-19 前后，5 个线程全部 `closed`，见 §10）。第 1 轮起按本文执行。
> 载体是**本机 walgit** 的 collab 层，**不是 GitHub**。
> 相邻文档：`AGENTS.md`（仓库硬约束）、`docs/design/review-plugin-events-goal-hint.md`（第 0 轮样例）。

## 1. 结论

- 协作记录的唯一载体是 walgit collab：条目落在 `refs/collab/inbox/ki-agent/*`，
  principal = `ki-agent`，ed25519 签名，读取侧逐条验签（当前 12 条全 `verified`）。
- 载体不是 GitHub。`origin = bigfish1913/pi-rust` 是与本项目无关的分叉仓库，
  `fork = gqf2008/pi-rust` 的 issue 已禁用；按 `AGENTS.md` 不得向 `origin` 提 issue/PR/推送/合并。
- 一轮（round）= 一张 board 卡片（issue + naming `owner` 的 status）+ 每个工作项一个 worktree
  + 一串签名条目（patch → review → merge_result → status closed）。
- 卡片 `closed` 的条件是「**提交 SHA + 一条可复制执行的验证命令 + 期望输出片段**」三件套齐。
  只有 `verified` 的 `approve` review 才计入合并规则；正文写「已修复」不构成证据。

## 2. 载体与身份（可复现判据）

| 事实 | 值 | 判据 |
|---|---|---|
| collab 服务器 | `http://127.0.0.1:8081/gqf2008/pi-rust` | `git remote -v`（remote 名 `walgit`） |
| 条目位置 | `refs/collab/inbox/ki-agent/*` | `git for-each-ref refs/collab/inbox/ki-agent` |
| 本 checkout 身份 | 已 `collab join` | `ls .git/walgit/`（worktree 局部身份，不被 git 跟踪） |
| principal | `ki-agent` | `git cat-file -p refs/collab/meta/principals/ki-agent` |
| 签名 | ed25519，逐条 `sig` 字段 | `walgit collab thread <id>` 的 `verified` 标记 |
| MCP 接入 | `.ki/settings.json` 的 `walgit` server：`walgit mcp --remote walgit --allow-write --key .git/walgit/keys/ki-agent.ed25519 --subscribe-interval-ms 5000` | 订阅 `walgit://refs/gqf2008/pi-rust`、`walgit://collab/board/gqf2008/pi-rust` |

常看的四条命令（都只读、都本地聚合，不联网）：

```bash
walgit collab ls                 # 本 checkout 里出现过的线程 id
walgit collab thread <id>        # 单个线程：parent 序 + 逐条验签（JSON）
walgit collab board              # 卡片投影：open / merged / closed / other 四列
walgit collab report --remote walgit   # 摘要 + `unpushed: N`（本地有、远端没有的 collab ref）
```

> 远端一致性只认 `unpushed: 0`。`report --remote` 用 `git ls-remote` 比对，远端不可达是**报错**
> 而不是静默的 0。读到非 0 说明有本 checkout 新写、没推到 `walgit` 的条目（多半是写条目时
> 漏了 `--push`）：补齐推送后再谈闭环，别把「本机 board 好看」当成账实相符。

## 3. 条目种类

一条 entry 就是一个签名 JSON：`{version, kind, id, actor, parent, ts, body, sig}`。
`id` 是**线程 id**（同一线程所有条目共用），`parent` 指向上一条的 entry oid（root 为空串）。

| kind | body 关键字段 | 用途 |
|---|---|---|
| `issue` | `{"title","body"}` | 线程根，卡片标题与正文 |
| `status` | `{"status","owner","worktree?","branch?","work","note?"}` | 认领 / 移动卡片；`status` 可取 `in-progress`、`needs-review`、`blocked`、`needs-human`、`closed` 等 |
| `patch` | `{"title","message"}` + `--base` / `--head` | 实现分支，把改动锚到真实 ref/SHA |
| `review` | `{"decision":"approve\|request_changes\|comment","agent","note"}` | 独立一方的评审（自己的 principal + key；headless sub-agent 也算一方） |
| `merge_result` | `{"merged":true,"oid","result","note"}` | 合入记录，**一条**（board/PR 只看 `merged:true`） |
| `comment` | `{"note"}` | 进度说明，**不移动卡片** |

写法示例（`--push` 才推到服务器；不带就是只写本地，并在 stderr 说明）：

```bash
walgit collab entry --kind issue --id <thread-id> --actor ki-agent --parent "" \
  --body '{"title":"…","body":"…"}' --push walgit

walgit collab entry --kind status --id <thread-id> --actor ki-agent --parent <issue-oid> \
  --body '{"status":"in-progress","owner":"ki-agent","worktree":".claude/worktrees/wt-<id>","branch":"wt-<id>","work":"…"}' \
  --push walgit

walgit collab entry --kind patch --id <thread-id> --actor ki-agent --parent <status-oid> \
  --base refs/heads/main --head refs/heads/wt-<id> \
  --body '{"title":"fix(cli): …","message":"…"}' --push walgit
```

每条 entry 都会打印出新的 entry oid，下一条的 `--parent` 用它。

认领卡片（D1 §8.4，规则不在这里重述）：退出 0 才继续；2 是别人持有卡片、必须停手
（认 `stand-down:` 行，不要只比对裸数字——2 也是 clap 的用法错误码）；1 是错误或卡片不可认领
（只有 `open` / `in-progress` 可认领）。walgit 的环境变量拼写是 `WALGIT_CLAIM_SESSION`，ki 有意
不设置它，因此总是显式传 `--session`。

```bash
walgit collab claim --id <thread-id> --session "$KI_AGENT_ID" --push walgit
```

### 3.1 瞬态推送失败

`walgit collab entry` 偶发网络/5xx 失败（本会话至少 5 次，重试即过，不是 schema 错误）。
用 `scripts/collab-entry-retry.sh` 包装同一条命令：默认 3 次、指数退避；
`schema` / `validation` / `signature` / `invalid` 等错误第一次就失败，不重试、不掩盖。

```bash
scripts/collab-entry-retry.sh --kind status --id <thread> --actor ki-agent \
  --parent <oid> --body '{...}' --push walgit
```

## 4. board 投影规则（`walgit collab board` 的实际行为）

- 列：`open` / `merged` / `closed` / `other`。
- 线程必须**同时**有 `issue` 和一条 naming `owner` 的 `status`，才投影成**有人认领**的卡片；
  只有 `issue`、无线程级 owner 的，投影成 **unowned open** 卡片（`comment` 里写 owner 不算）。
- 卡片状态取**最新一条**带字符串 `status` 的条目；`merge_result {"merged":true}` 也把它落到 `merged`。
- `owner` / `worktree` / `branch` / `work` 各自取最新一条**写了该字段**的 status；省略即继承，
  显式空串即清除，`work` 缺省回落到该条目的 `note`。
- 投影**不**按签名过滤：验签只体现在 `verified` / `unverified` 计数、合并规则（只算 verified 的
  approve）与 `done` 门禁上。所以「卡片在板上」≠「证据已验签」。

本仓库没有提交 `.walgit/board.toml`（`ls .walgit` 报不存在），board 走内置默认定义；
列名与上面的投影规则都是默认定义下的行为。要改列，先把 `board.toml` 提交进 HEAD 再用
`walgit collab board --board <file>` 预览，**不要手工改状态文件**。

## 5. 一轮（round）的步骤

1. **recon 选项目**：看 board 的 `open` 列和最新 `commit`，确定这一轮要推进的工作项；
   一个工作项一个线程 id，不把两个目标揉进一张卡片。
2. **开 issue + status(owner)**：两次 `collab entry --push walgit`，第二步的 status 必须写
   `owner`（否则卡片是 unowned），把 `worktree` / `branch` / `work` 一并写上，卡片才算被认领。
3. **建 worktree**：`.claude/worktrees/wt-<id>`（见 §6），一个工作项一个 worktree，互不干扰。
4. **实现 + 加断言**：修 bug 必须补一条能证伪的测试；不能被突变打红的断言不算断言
   （第 0 轮 R1/R4 就是靠重跑突变证明的）。
5. **patch 条目**：`--base` / `--head` 写真实 ref 或 SHA（`refs/heads/main`、`refs/heads/wt-<id>`
   或提交 SHA），正文写清楚改了什么、为什么。
6. **review 条目**：由**另一方**（自己的 principal/key，或 headless sub-agent）出
   `decision`；同一条线程里 `request_changes` 未处理完，不进合并。
7. **merge_result**：ff 合入后立刻落一条 `{"merged":true,"oid":"<sha>",…}`，`oid` 是合入后的提交。
8. **status closed**：`note` 里必须给**提交 SHA + 一条可复制命令 + 期望输出片段**（§8 的规格），
   这样关卡片本身就是证据。
9. **文档 / CHANGELOG 与 board 一致**：改了 `CHANGELOG.md` / `README.md` 要重跑
   `sh scripts/sync-embedded-docs.sh`；线程里的结论与仓库内 `docs/design/*.md` 的结论必须同源，
   不允许「板上一套、文档一套」。
10. **收尾**：worktree 与分支在验证通过后清理（删除前先列清单取授权，见 §9）。

## 6. 命名与提交信息

| 对象 | 形态 | 说明 |
|---|---|---|
| 线程 id | `<主题-slug>`（如 `ask-user-guest-call-panic`）；审查类 `review-<base-sha>-<head-sha>-<主题>` | 第 0 轮既有形态，沿用 |
| worktree | `.claude/worktrees/wt-<id>` | 与 `AGENTS.md` 的「一批一个 worktree `wt-<批名>`」同形，`<id>` = 线程 id 或工作项 slug |
| 分支 | `wt-<id>` | 与 worktree 同名，patch 的 `--head` 指它 |
| 提交信息 | Conventional Commits（`fix(cli): …` / `feat(goal): …` / `chore(docs): …`） | `AGENTS.md` 硬约束；一批一个 worktree，完成后 ff 合入 `main` |

上限/超时/额度这类常量上调，**单独一个 commit** 并写明新旧值与理由（`RULE_阈值变更.md`）。

## 6b. 一个 agent 一个 worktree/principal

同一 checkout 里开多个 agent 会让所有条目共用 `ki-agent` 这个 principal：claim 的 session
作用域能收敛，但 board 上的 owner/审计无法区分，通知里的 actor 也无法用于自回声抑制。
`scripts/setup-collab-agents.sh [n]` 会为每个 agent 建一个 worktree 并执行
`walgit collab join --principal ki-agent-<n> --push walgit`；各自 `cd` 进去启动 `ki -a`。
脚本幂等、只创建不删除；`--dry-run` 只打印命令。

身份是持久的 principal，session 是运行上下文：一个身份可以有多个 session。协作键是
`(identity, session)`——每条 entry 自动带 `KI_AGENT_ID` 写入的 `session`，通知里的
`_meta.sessions/self_session` 让“全是本 session 写的”批次只记录不唤醒，同伴 session 的
更新照常唤醒。不需要 per-session key，也不需要 worktree。

### session id 必须真的到得了服务端

`self_session` 由 **服务端**（`walgit mcp`）填，它读自己的 `KI_AGENT_ID`；而 stdio MCP 子进程
是 ki 用 `env_clear()` + 白名单拉起来的。少了这一条转发，`_meta.self_session` 永远缺失，
当时的客户端判定函数（`self_authored_from_meta`，现已改为读 `_meta.sessions` 的
`authored_sessions_from_meta` + `McpNotice::is_own_echo`）恒为 false——抑制逻辑在两端都写好了，
却一次都不会触发，表现就是“自己写一条、自己醒一次”。

现在 `spawn_stdio_client` 显式转发 `AGENT_ID_ENV`（经 SDK 常量，不写死名字），顺序上排在
白名单之后、server 自己的 `env` 之前（配置仍可覆盖）；provider key 与 `KI_SESSION_ID` 照旧
不外泄，`crates/kiqi-mcp/tests/e2e.rs` 的那条断言同时钉住这两半。HTTP transport 没有环境变量
这回事，HTTP 版协作服务端需要在自己配置的 `headers` 里带上 session。

判据：把转发删掉 → 该 e2e 报 `left: "<unset>"`；把 `ANTHROPIC_API_KEY` 塞进白名单 →
同一条报 `left: "sk-should-never-leak"`。

### 比对发生在 ki 侧，且用“此刻”的 session

转发只解决一半。`self_session` 是**服务端进程**的快照：子进程拿到的是 spawn 时的
`KI_AGENT_ID`，而 ki 在 `/session` 切换或 fork 之后会 republish 自己的 id
（`publish_session_identity`）。拿批次去比服务端快照，切换一次之后两边就永久不一致，
抑制再次静默失效——症状与没转发一模一样（“自己写一条、自己醒一次”）。

因此在客户端定夺：`_meta.sessions` 原样带到 `McpNotice.authored_sessions`，由
`McpNotice::is_own_echo(ki 当前 session)` 在决策时比对（`SignalHost` 每次决策调
`session::current_session_id()`）。三条不可回退的规则：

- **没有来源就不算回声。** 空 `authored_sessions`（老服务端、或写入早于 session 记录）
  → 唤醒，与旧行为一致。
- **混合批次不算回声。** 同伴的条目是新闻；只有全部来自本 session 才跳过。
- **`ephemeral` 不是身份。** `--no-session` 每次写同一个字面量，当成身份会让一个临时
  session 吞掉另一个的更新；`session_id_from` 把它和空白值都归为“无 session”。

判据：`suppression_follows_a_session_switch_not_the_servers_snapshot` 用两个不同 id
证明“同一批次对新 session 是回声、对旧 session 是新闻”；
`an_ephemeral_session_id_is_not_an_identity` 钉住 `ephemeral`/空白/真实 id 三种输入
（纯函数，不写进程级环境变量——本仓库有过 env 测试互相竞争的实例）。

## 7. 门禁（= CI 跑的东西，一轮结束前必须全绿）

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo test  --workspace --locked

# 改了 CHANGELOG.md / README.md 等嵌入式快照文档时必须重跑，CI 会比对：
sh scripts/sync-embedded-docs.sh
```

注意：goal 断言已用子进程 + tempdir 隔离，`cargo test -p kiqi-cli` 不再改写源码树
（`crates/kiqi-cli/.ki/` 已移出索引并被 `.gitignore` 的 `**/.ki/*` 忽略）。
wasm guest 只在 `wasm32-wasip2` 链接，
guest crate 内不写单测，行为由 `crates/kiqi-plugin-host/tests/e2e.rs` 从宿主侧验证。

## 8. 什么样的条目算闭环证据

**规格**：一条 `status closed`（或 comment）里必须能挑出三样东西——

1. **提交 SHA**：7 位以上的真实提交，能 `git show <sha>`；
2. **一条可复制执行的验证命令**：单行、无占位符、不依赖会话上下文；
3. **期望输出片段**：命令跑完后应看到的关键字或计数（如 `1 passed`、`FAILED`、`104 passed / 0 failed`）。

第 0 轮的两个正例（原文见 `walgit collab thread ask-user-guest-call-panic`）：

```
提交 c42f95e；判据：
cargo test -p kiqi-cli --lib --locked plugins::tests::plugin_calls_run_outside_the_tokio_context
→ 1 passed
```

```
R1 已修 70fbfff；把 guest 的 present 改成 |_name| !tools.is_empty() 后
cargo test -p kiqi-plugin-host --test e2e --locked goal_injection_carries
→ FAILED（e2e.rs:3292）；还原后 1 passed
```

反例（一律不算闭环，退回 `open`）：

- 只写「已修复」「已处理」「应该没问题」而不给 SHA 或命令；
- 命令带占位符（`<sha>`、`…`）或依赖某个会话里才有的环境；
- 用命名/同类外推当结论（例如「这个常量应该也有上限」而没读到代码或跑到测试）；
  结论必须落到 `文件:行号` 或命令输出（`RULE_能力与依赖结论必须实核不得外推.md`）；
- 复验基准漂移：note 里的 SHA 不是当前 `HEAD` 的祖先，或命令在当前 `HEAD` 上已跑不过。

## 9. 硬约束

- **删除前授权**：删除任何文件/目录/远端分支（含 worktree、collab 线程、分支）前，
  先列清单并取得明确授权（`RULE_未经授权禁止删除文件.md`）。collab 条目本身是 append-only
  证据，**不删**：卡片用 `status closed` 退役，条目永久留在 `refs/collab/inbox` 里。
- **阈值单独提交**：上限/超时/额度上调单独一个 commit，写明新旧值与理由（`RULE_阈值变更.md`）。
- **结论可复现**：报告「结论」必须带判据（`文件:行号` 或命令），不得凭命名/同类外推
  （`RULE_能力与依赖结论必须实核不得外推.md`）。
- **不碰 origin**：`origin` 是与本项目无关的分叉，不提 issue/PR、不推送、不合并、不作为变更来源；
  要推就推 `fork`（`gqf2008/pi-rust`）或 collab 的 `walgit` 远端。
- **不手改状态**：卡片移动一律走签名 `status` 条目，不手写 `refs/collab/*`、不手改 board 状态文件。

## 10. 轮次记录

线程 id 以 `walgit collab ls` 为准（下表与 refs 一一对应）。

| 轮次 | 线程 id | 工作项 | 状态 | 提交 SHA |
|---|---|---|---|---|
| 0 | `mcp-probe-145027` | 订阅链路探针（board / refs 通知到达会话） | closed | —（探针，无提交） |
| 0 | `mui-probe-155733` | MUI 通知测试（重启并重新订阅后输入是否到达） | closed | —（探针，无提交） |
| 0 | `mcp-subscription-probe-20261019` | MCP 订阅通知测试（2026-10-19） | closed | —（探针，无提交） |
| 0 | `ask-user-guest-call-panic` | 修 `ask_user` guest 在 call 路径 panic（tokio 上下文内嵌套运行时） | closed | `c42f95e` |
| 0 | `review-4ad87ef-acf14ed` | 审查 `4ad87ef` / `acf14ed`，R1–R8 收口 | closed | `70fbfff` / `78dec42` / `453f976` / `63b1e63` |
| 1 | `r1-goal-state-isolation` | goal 测试把运行时状态写回源码树（`.ki/goals.json` 被 git 跟踪） | closed | `6204aac` / `2c892f0` |
| 1 | `r1-ansi-slice-styles` | `slice_by_column` 的 `preserve_styles` 是空参数，裁剪丢色丢光标标记 | closed | `1f90026` / `ce191b2` |
| 1 | `r1-hung-post-flaky` | `a_hung_post_does_not_wedge_the_worker_for_later_calls` 间歇失败（线缆超时文案不说超时） | closed | `5b741b3` / `b899a05` |
| 2 | `r2-soft-interrupt-phase1` | 软中断阶段 1：单 Esc 只取消当前工具、run 继续（双 Esc 仍硬取消） | closed | `f7013cb` / `4902ab7` |

第 1 轮三条卡片各走完「issue → status(owner) → worktree → patch → review → 修复 → 再审 →
merge_result → status closed」。两条实质工作项的第一轮评审都是 `request_changes`
（P1：三份文档仍在教用户跑已失效的 `git checkout --`；P2：修复本身引入窗口外零宽转义泄漏与
OSC-8 不闭合），修复后第二轮 `approve`。`r1-hung-post-flaky` 是第一轮 `approve` 但带守卫缺口
建议，补齐后合入。第 1 轮合入后 `main` = `b899a05`。

第 0 轮的三条探针卡片在关闭时把 `status` 写成 `closed`，note 说明探针已验证、原始条目永久留在
`refs/collab/inbox` 作为证据；两条实质工作项把提交 SHA 与复验命令写进 `status` 的 `note` 与
同线程 `comment`（`docs/design/review-plugin-events-goal-hint.md` 是同一份结论的仓库内副本）。

复验第 0 轮的现状：

```bash
walgit collab board          # → closed 5 / open 0 / merged 0 / other 0
walgit collab report --remote walgit | head -1   # → unpushed: 0 (vs walgit)
```

复验第 1 轮合入后的现状：

```bash
walgit collab board                              # → closed 8（第 0 轮 5 + 第 1 轮 3）
walgit collab report --remote walgit | head -1    # → unpushed: 0 (vs walgit)，38/38 entries verified
git log --oneline -1                             # → b899a05
```

第 2 轮只有一张卡片（`r2-soft-interrupt-phase1`），走完了同样的流程，且是第一次
**实现阶段**：`InterruptControl` 落进 `kiqi-agent`、`AgentLane::interrupt` 落进
`kiqi-harness`、Esc 分流落进 `kiqi-cli`。首轮评审 `request_changes`，问题是
UI 提示与标志语义相反（提示说“没有东西可中断”，实际已上膛会取消下一个动作）；
修在 `4902ab7` 后 `approve`。四条突变各自打红对应断言，写在卡片 note 里。

复验第 2 轮合入后的现状：

```bash
walgit collab board                              # → closed 9
walgit collab report --remote walgit | head -1    # → unpushed: 0 (vs walgit)
git log --oneline -1                             # → 4902ab7
```

第 2 轮结束后 board 上仍留三条 `open`：两条早期探针（`insert-probe-175642`、
`restart-probe-175422`）与软中断方案的阶段 2/3（尚未开卡）。

第 1 轮结束后 board 上仍留三条 `open`：软中断方案（`soft-interrupt-design`，已转 `needs-review`）
与两条早期探针（`insert-probe-175642`、`restart-probe-175422`）。它们是下一轮 recon 的输入，
不是遗留缺陷。
