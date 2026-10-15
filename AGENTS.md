# AGENTS.md — 本仓库给 AI 代理与协作者的约定

## 这个仓库是什么

`ki`（crate 前缀 `rpi-`）：一个 Rust 编码代理。仓库由 **`gqf2008/pi-rust`** 维护，
开发在 `fork` 远端、`main` 分支上推进。

## 上游已分叉，与本项目无关（**重要，勿误判**）

- 远端 `origin` = `bigfish1913/pi-rust`：**这是一个已分叉、与本项目没有任何关系的仓库**。
  它既不是本项目的上游，也不是下游。
- 因此：**不要**向它提 issue/PR、不要向它推送、不要从它合并/同步、不要把它当作变更来源或
  对齐目标；汇报时也不要说「上游如何」。要推送就推 `fork`（`gqf2008/pi-rust`）。
- 本项目**没有上游**。引用历史出处时用 commit SHA，不要用 `origin/main` 之类做基线。

## 跟踪与流程

- **`fork` 关掉了 issue**（`gh issue list -R gqf2008/pi-rust` 会报 disabled）。
  批次跟踪落在仓库内的设计文档（`docs/design/*.md` 的分轮记录）与提交信息上，
  不要因为「没有 issue」而跳过立项/验收记录。
- 一批一个 worktree（`.claude/worktrees/wt-<批名>`），批次完成、门禁全绿后 ff-合入 `main`，
  再清理 worktree 与分支。
- 提交信息用 Conventional Commits（中英混排在本仓库历史里都有，前缀必须是规范形式）。

## 本地门禁（= CI 跑的东西）

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo test  --workspace --locked
# 可选 feature（有 wasm 版的插件都不在默认构建里）必须单独跑，否则它们会腐烂：
cargo test -p rpi-cli --lib --locked --features plugin-rpi-todo,plugin-rpi-lens,plugin-rpi-webfetch,plugin-rpi-websearch,plugin-rpi-subagents,plugin-rpi-goal,plugin-rpi-plan-mode,plugin-rpi-ask-user,plugin-rpi-langfuse
# 文档快照（改了 CHANGELOG.md / README.md 等必须重跑，CI 会比对）
sh scripts/sync-embedded-docs.sh
```

注意：`cargo test -p rpi-cli` 会改写 `crates/rpi-cli/.ki/goals.json`（运行时状态被提交进仓库），
跑完记得 `git checkout --`。wasm guest 只对 `wasm32-wasip2` 链接，**guest crate 里不写单测**，
行为一律由 `crates/kiqi-plugin-host/tests/e2e.rs` 从宿主侧验证。

## 两条插件通道（改动前先分清）

| 通道 | 形态 | 适用 |
|---|---|---|
| **编译期** `plugins/rpi-*` | `kiqi_plugin::Plugin` trait（进程内、无沙箱），另带 cdylib 面供 `ki dev` 热载 | 一方插件 |
| **wasm** `plugins/wasm/kiqi-*-guest` | 组件模型 + `plugins/kiqi-plugin.wit` 契约，跑在 wasmtime 沙箱里，按授权拿能力 | 第三方；契约版本见 WIT 的 `package` 行 |

**默认构建只编入没有 wasm 版的插件**（当前：`rpi-orchestrator`、`rpi-permissions`、`rpi-codegraph`）。
有 guest 的那些都是 `--features plugin-rpi-<name>`，或用 `ki insmod --build plugins/wasm/kiqi-<name>-guest`
装上 wasm 版。**新增/改插件能力时，两侧（trait 面与 WIT 面）必须一起改**，
差异要写进 guest README，不许藏。

## 设计文档在哪

- `docs/design/plugin-abi.md`：wasm 契约与能力模型的**唯一真源**；每轮改动加一节 `### 6.x`，
  §7 是待定项。
- `docs/design/plugin-trait.md`：编译期 trait 契约。
- `docs/design/plugin-abi-handover.md`：**接手先读这个**（现状/能力表/上手命令/已知限制/提交轨迹）。
- 未落地的大改走单独方案文件（如 `docs/design/plugin-process-capability.md`）。

## 硬规矩

- 删除任何文件/目录/远端分支前，先列清单并取得明确授权（用户全局规则 `RULE_未经授权禁止删除文件.md`）。
- 上限/超时/额度这类常量上调，单独 commit 并写明新旧值与理由（`RULE_阈值变更.md`）。
- 报告「结论」必须带可复现判据（文件:行号或命令），不得凭命名/同类外推（`RULE_能力与依赖结论必须实核不得外推.md`）。
