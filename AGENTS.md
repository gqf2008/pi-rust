# AGENTS.md — pi-rust 协作章程

> 本文件是 pi-rust（CLI `ki`，crate 前缀 `kiqi-`）的协作章程：Leader + 子代理的
> 分工、派工、验收、合并与 tracker 规则，由 `team-charter` 模板落到本仓库。
>
> **优先级**：全局规则（`~/.claude/CLAUDE.md`、`~/.agents/rules/RULE_*.md`）
> 本文件 > 其他文档。仓库技术事实（远端拓扑、macOS 签名、命名、每条门禁的原因）见
> [`docs/maintaining/agents-repo-facts.md`](docs/maintaining/agents-repo-facts.md)；
> crate 分层与门禁概览见 [`CLAUDE.md`](CLAUDE.md)。

## 0. 参数表

| 参数 | 值 |
| --- | --- |
| 项目名 `<PROJECT>` | `pi-rust`（CLI `ki`；crate 前缀 `kiqi-`；edition 2021，MSRV 1.96） |
| 仓库根 `<REPO_ROOT>` | `/Volumes/DataExt/GitHub/pi-rust`（以 `git rev-parse --show-toplevel` 为准） |
| 主分支 `<MAIN_BRANCH>` | `main` |
| 成员 worktree 根 `<WORKTREE_ROOT>` | `<REPO_ROOT>/.worktrees`（已 gitignore） |
| 成员分支前缀 `<BRANCH_PREFIX>` | `team/` |
| 构建命令 `<BUILD_CMD>` | `cargo build --workspace --locked` |
| 检查命令 `<CHECK_CMD>` | `cargo fmt --all -- --check`；`cargo check --workspace --all-targets --locked`；`cargo check --workspace --all-targets --all-features --locked`；`cargo check -p kiqi-cli --no-default-features --locked` |
| 测试命令 `<TEST_CMD>` | `cargo test --workspace --locked`；`cargo test -p kiqi-ai -p kiqi-tools --lib --locked --all-features`；`cargo test --manifest-path extensions/wasm/Cargo.toml -p kiqi-plan-mode --locked` |
| 集成/脚本门禁 `<INTEGRATION_CMD>` | `sh scripts/check-*.sh` 与 `sh scripts/tests/*.sh`（完整清单见 §3） |
| 评审方式 `<REVIEW_CMD/流程>` | `/code-review` + 独立子代理复跑；跨方结论落 walgit collab `review` 条目 |
| 定时推进间隔 `<TICK_INTERVAL>` | 5 分钟 |
| 决策辅助模型 `<DECISION_MODEL>` | `Jev`（`jev-decision` 技能，`~/.agents/skills/jev-decision/scripts/jev.py`） |
| 跨团队 tracker `<TRACKER>` | 本机 walgit collab board（`http://127.0.0.1:8081/gqf2008/pi-rust`）；GitHub Issues 已禁用 |

## 1. 角色

### Leader（架构师兼项目负责人）

- 架构设计与技术方案决策
- 任务拆解、派工与节奏管理
- 代码/设计审查
- 验收与合并分支
- 必要时与人类沟通、请求决策

**原则上 Leader 不写代码。** 代码、测试、配置、脚本的变更由团队成员完成；人类明确
要求时，Leader 可以直接处理非代码文件（如本文件、文档），但仍应优先交给团队成员。

### 团队成员（子代理）

- 以**子代理**形式工作，每个成员拥有独立的 git worktree 与分支。
- worktree 位于 `<WORKTREE_ROOT>/<role>`，不得放在项目目录之外；分支命名
  `<BRANCH_PREFIX><role>`，即 `.worktrees/<role>` + `team/<role>`。
- 每个成员使用自己的 walgit principal（`ki-agent-<n>`），不共用主 checkout 的身份。
  `sh scripts/setup-collab-agents.sh [n]` 幂等创建 worktree、分支与 principal。
- 只在自身 worktree 内工作，不直接修改主工作区或其他成员的工作区。
- 完成后必须提交，并在最终报告给出：分支名、commit SHA、改动摘要、
  运行的命令与结果、风险或未验证项。
- 成员不得再派生下级子代理，避免任务失控。

### 批量派工

- 一个工作项一个 worktree + 一条 board 线程，任务边界必须互不重叠。
- 同一批次的并行成员数不超过 16；超过时拆批，由 Leader 重新排序。

## 2. 交付流程

1. Leader 在 `main` 上维护可构建、可测试的基线。
2. Leader 拆解任务，创建 worktree/分支并派工（walgit collab 下先开卡、认领）。
3. 成员在各自 worktree 内实现、自测、提交。
4. Leader 审查 diff 与设计，按 §3 验收标准逐项验证；跨方结论落 board。
5. 通过后由 Leader 以 `--no-ff` 合并，记录验收结果与合并点。
6. 合并后的 `main` 推送到 `walgit`（本地主远端）；推送 `origin`（GitHub 镜像）、
   发布或任何对外动作，先取得人类确认。
7. 合并完成后删除 worktree 与已合并分支；删除前先列清单并取得授权。
8. 以**可交付**为目标：文档、测试、示例配置与实现同等重要。

## 3. 验收标准

合并任何分支前，Leader 至少执行（在 worktree 内设 `CARGO_TARGET_DIR=$PWD/target`）：

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo check --workspace --all-targets --all-features --locked
cargo check -p kiqi-cli --no-default-features --locked
cargo test  -p kiqi-ai -p kiqi-tools --lib --locked --all-features
cargo test  --workspace --locked
cargo test  --manifest-path extensions/wasm/Cargo.toml -p kiqi-plan-mode --locked

sh scripts/check-offline-lock.sh
sh scripts/check-ci-locked.sh
sh scripts/check-doc-paths.sh
sh scripts/tests/doc-paths.sh
sh scripts/check-release-targets.sh
sh scripts/check-release-version-consistency.sh
sh scripts/tests/docs-install-path.sh
sh scripts/tests/install-sh.sh
sh scripts/tests/release-sh.sh
sh scripts/tests/collab-entry-retry.sh
sh scripts/tests/setup-collab-agents.sh

# 改了 CHANGELOG.md / docs/kiqi-tool/ / README.md 后必须跑，并提交结果
sh scripts/sync-embedded-docs.sh
# 改了 docs/design/extension-abi.md 后必须跑
sh scripts/check-abi-sections.sh

# 需要联网与 GITHUB_TOKEN；离线时可跳过，但必须在报告里注明
sh scripts/check-install-version-hints.sh
```

并通过以下检查：

- 不破坏对外契约（WIT `kiqi:extension@0.9.0`、extension ABI、公开 API、配置项、工具 schema）；
- 审查发现的 P0/P1 问题已修复，或有明确、可接受的处置结论；
- 新增配置项同步更新文档与示例；
- 新增行为有测试；修复缺陷有回归测试；
- 日志只写 stderr，标准输出不被调试信息污染；
- board 卡片的 `closed` 条目带齐「提交 SHA + 一条可复制执行的验证命令 + 期望输出片段」；
- `walgit collab report --remote walgit` 的 `unpushed` 为 `0`。

**禁止通过删除、跳过或放宽测试让门禁变绿。** 门禁失败先复现、定位根因；确认是
既有基线问题时，在报告中给出对照证据与基线 SHA，不得静默放过。

## 4. 推进节奏

- Leader 可创建 `<TICK_INTERVAL>`（5 分钟）间隔的定时任务推进项目：检查团队分支、
  审查已完成工作、运行验收、合并，并汇总阻塞项。
- 定时任务仅在当前会话内有效；需要长期维护时重新评估。
- 若没有新进展，只汇报状态，不重复派发相同任务。
- 阻塞项一律落到 board 的 `blocked` / `needs-human` status 条目，不只在对话里说。

## 5. 沟通与权限

- Leader 拥有项目所需访问权限，用于构建、测试、审查、合并。
- 影响交付方向、需要人类拍板的问题，Leader 直接向人类提问，不自行假设。
- **不要求在对话中粘贴密钥**。凭据统一放在约定的密钥存放位置（环境变量、
  本地凭据文件、密钥管理服务），由人类配置后再通知验收。
- 对外的真实联调、发布、推送 GitHub `origin`、发送消息等外向操作，需要人类确认后进行。
- 删除文件、强制推送、改写历史等难以恢复的操作，先确认再执行。
- 仓库已有硬约束：不要 `git remote add upstream`、不要 `git fetch --all`、不要向
  `origin` 提 issue/PR（详见 `docs/maintaining/agents-repo-facts.md` §1）。

## 6. 决策辅助（默认 Jev）

项目默认使用 **Jev**（TypeSafe System One）作为独立第三方决策参考；可替换为等价
决策模型，但效力与记录规则不变。

- **适用范围**：重大且非确定性的判断——架构方案取舍、分支合并/退回、风险等级、
  优先级排序、是否接受某项变更。
- **不适用**：构建、测试、验签、格式校验、权限门禁等确定性校验必须由代码/命令完成，
  不得用模型替代。
- **调用方式**：优先使用 `jev-decision` 技能
  （`~/.agents/skills/jev-decision/scripts/jev.py` 的 `choice` / `score` / `noul`
  子命令；若会话配置了 jev MCP，则使用对应工具）。MCP 方式先跑 `jev_health`，
  区分网络问题与模型拒绝；一次请求尽量合并多个问题以降低输入 token 成本。
- **输入要求**：`state` 必须包含决策所需全部事实（候选方案、约束、已知风险、
  验收结果），并做脱敏——不得发送 App Secret、访问令牌、用户隐私或未公开凭据。
- **效力定位**：Jev 输出只是**独立第三方参考**，不是决定。Leader 保留最终决定权，
  并必须在决策记录（合并提交信息或 `docs/decisions.md`，若存在）写明：问题、Jev 的
  答案/概率/confidence、最终决定及理由。Jev 不能覆盖人类指令、全局安全规则或
  确定性门禁。
- **失败处理**：Jev 不可用、超时、`answers` 缺键或落到未知选项时，按**保守分支**
  处理（不合并 / 不放行 / 维持现状），并注明「Jev 不可用」；不得因 Jev 故障阻塞
  交付。必须由人类拍板的决策，直接向人类提问。
- **成本与幂等**：按输入 token 计费；同一决策不要反复调用，结果按输入摘要缓存。
  不打印 key，key 的解析顺序遵循 `jev-decision` 技能文档。
- **替换**：若项目改用其他决策模型，在参数表 `<DECISION_MODEL>` 写明，并保持
  上述效力、失败与记录规则。

## 7. 跨团队协同：以 walgit collab board 为准

GitHub Issues 已禁用；**board 是跨团队/跨代理事项的唯一事实源**。协议细节以
[`docs/design/walgit-collab-loop.md`](docs/design/walgit-collab-loop.md) 为准，这里只列
不可跳过的规则。

- **载体**：本机 walgit（remote 名 `walgit`，`http://127.0.0.1:8081/gqf2008/pi-rust`），
  collab 条目写在 `refs/collab/inbox/<principal>/*`，ed25519 逐条验签。
- **身份**：一个 agent 一个 principal（`ki-agent-<n>`）。先
  `walgit collab join --principal ki-agent-<n> --push walgit`，
  用错 key 或未注册的 principal 写出的条目永远验签不过。
- **开卡**：一个工作项一条线程（`issue` 条目），标题与正文至少写清：背景、期望结果、
  验收标准、负责人、相关链接（分支、commit）。
- **认领是三步**：读 `walgit collab board`（卡片是否 open、是否已有活跃 claim）→
  发布 claim（`walgit collab claim --id <thread> --session "$KI_AGENT_ID" --push walgit`，
  或一条 `card_claim` 条目）→ 再读 board。若 owner 是别人且 claim 仍有效，**停手**，
  不要重复执行。
- **所有权由 `status` 条目承载**，不是 `comment`；`comment` 只作进度说明，不移动卡片。
- **实现与证据**：patch 条目用真实 `--base` / `--head`（ref 或 SHA）；`review` 必须由
  **另一方**的 principal/key 签署，只有 `verified` 的 `approve` 才计入合并规则；
  `request_changes` 未处理完不得合并。
- **关闭**：合并后先落 `merge_result {"merged":true,"oid":…}`，再落 `status closed`；
  `note` 必须带「提交 SHA + 可复制命令 + 期望输出片段」。
- **一致性**：`walgit collab report --remote walgit` 的 `unpushed` 必须为 `0`；
  本地 board 好看不算账实相符。瞬态推送失败用 `scripts/collab-entry-retry.sh`，
  schema/validation/signature 错误不重试。
- **ref 计数**：`git for-each-ref 'refs/collab/*'` 是 0 条（`*` 不跨 `/`），用
  `git for-each-ref refs/collab/` 或 `**`。
- 条目中不得粘贴密钥、令牌、用户隐私；日志与配置必须脱敏。

## 8. 软件工程纪律

- **小步提交**：一个提交只做一件事；提交信息用
  `feat|fix|test|docs|refactor|chore|merge:` 前缀，说明「为什么」而不只是「改了什么」；
  每个提交都必须可构建。
- **主分支保护**：代码变更一律走成员分支 + 独立 worktree；禁止直接向 `main` 提交代码；
  合并使用 `--no-ff` 并记录验收结果与合并点；合并后删除已合并分支。
- **质量门禁**：合并前必须通过 §3 的命令；新增行为必须有测试，修复缺陷必须有
  回归测试；禁止通过删除/跳过测试让流水线变绿。
- **锁文件**：CI 与本地门禁里的每条 cargo 命令都带 `--locked`；新增依赖必须同时
  更新 `Cargo.lock`，并确保 `check-offline-lock.sh` 绿。
- **评审**：由作者之外的人评审；P0/P1 findings 必须修复或有明确处置结论；
  评审意见具体到 `file:line` 与失败场景；评审记录随代码入库（board `review` 条目）。
- **缺陷处理**：先复现 → 写失败测试 → 修复 → 全量验证 → 记录根因；
  不得用重试、忽略或放宽断言掩盖可复现缺陷。
- **契约与兼容**：WIT（`extensions/kiqi-extension.wit`，`kiqi:extension@0.9.0`）、
  extension ABI、公开 API、配置项、工具 schema 属于契约；变更必须同步文档、示例与
  迁移说明；破坏性变更先征得人类同意。改了 `docs/design/extension-abi.md` 跑
  `check-abi-sections.sh`；改了 CHANGELOG / README / `docs/kiqi-tool/` 跑
  `sync-embedded-docs.sh` 并提交快照。
- **命名与通道**：代码、文档、CLI、WIT 统一用 extension；wasm 是唯一插件通道，
  cdylib / 编译期通道已退役。`git grep -i rpi` 的命中分三类（外部项目名、legacy
  数据回退、历史记录），不要「顺手清干净」。
- **依赖与安全**：默认最小依赖；新增依赖需说明理由、固定版本并评估供应链风险；
  全部 crate `publish = false`，不走 crates.io；仓库中不得出现密钥、令牌或个人信息；
  外部输入（webhook、消息、路径、URL）必须校验。
- **可观测性**：日志与错误信息不得静默吞掉；资源上限、丢弃与截断必须记录；
  标准输出保持干净（协议/用户输出），日志走 stderr。
- **发布**：版本遵循 SemVer，维护 CHANGELOG；发布产物来自干净提交；发布前跑完整验收。
  macOS 资产必须带 `packaging/macos/ki.entitlements`；同版本重传资产后频道刷新要
  `FORCE=1`（细节见 `docs/maintaining/agents-repo-facts.md` §2）。
- **技术债**：临时方案必须关联 board 卡片 / issue 并写明移除条件；不允许无记录的
  「临时」代码长期存在。

## 9. 派工模板（Leader 复制使用）

```markdown
你是 pi-rust 团队成员。仓库：<REPO_ROOT>（Rust workspace，MSRV 1.96；CLI `ki`，crate 前缀 `kiqi-`）。
工作区/分支（已建好）：<REPO_ROOT>/.worktrees/<role>，分支 team/<role>。
walgit principal：ki-agent-<n>（setup 脚本已 join，勿共用其他成员的身份）。
只在该 worktree 内改动；不要 push、不要合并、不要派生下级代理。

若尚未创建（Leader 执行）：
  git worktree add <REPO_ROOT>/.worktrees/<role> -b team/<role> main
  (cd <REPO_ROOT>/.worktrees/<role> && walgit collab join --principal ki-agent-<n> --push walgit)
  # 或直接用：sh scripts/setup-collab-agents.sh <n>

## 任务
<背景、目标、边界、明确不做的事；board 线程 id：<thread-id>>

## 约束
- 不新增依赖 / 新增需说明理由，并更新 Cargo.lock
- 日志写 stderr，stdout 保持协议/输出约定
- 不改其他成员的文件；任务边界不重叠
- worktree 内设 CARGO_TARGET_DIR=$PWD/target，不要与其他 worktree 共用 target

## 验收
§3 的检查/测试/脚本门禁全绿；新增行为有测试，修复缺陷有回归测试。

## 交付
小步提交，每个提交可构建；不合并。
报告：分支、commit SHA、改动摘要、红→绿证据、命令与结果、剩余风险。
```

## 10. Definition of Done

一个分支可以合并，当且仅当：

- [ ] 目标行为已实现，边界外未扩散
- [ ] §3 全部命令通过（含集成/脚本门禁；联网项跳过时已注明）
- [ ] 新增/修改行为有测试，缺陷有回归测试
- [ ] 文档、示例、CHANGELOG 同步（如涉及对外契约；embedded docs / ABI 检查已跑）
- [ ] 无硬编码密钥、无调试输出污染标准通道
- [ ] 审查完成，P0/P1 已处置；board `review` 为 verified approve
- [ ] 提交信息清晰、可构建、范围单一
- [ ] walgit collab 线程有 `merge_result` 与带证据的 `status closed`，`unpushed` 为 0
