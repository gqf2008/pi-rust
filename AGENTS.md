# AGENTS.md — 本仓库给 AI 代理与协作者的约定

## 这个仓库是什么

`ki`（crate 前缀 `kiqi-`）：一个 Rust 编码代理。仓库由 **`gqf2008/pi-rust`** 维护，
开发在 `walgit` 远端的 `main` 分支上推进。

| 远端 | 角色 |
|---|---|
| `walgit` | **主开发远端**（`gqf2008/pi-rust` 的本地镜像，日常推送） |
| `fork` | GitHub 镜像 / 发布目标 |
| `origin` | `bigfish1913/pi-rust`，无关分叉，禁用 |

## 命名（改名已完成，勿回退）

命令行是 **`ki`**，项目名是 **`kiqi`**。2026-10 的改名把 crate / 插件 / wasm guest
目录、package 名、feature（`plugin-kiqi-*`）、Rust 路径（`kiqi_*`）、插件 id、
文件名、运行时目录（`.ki/`）与资源（`.ki/skills/`）统一到了 `kiqi-`。

`git grep -i rpi` 只应命中这三类，**不要**「顺手清干净」：

1. **外部项目名**：`pi-rust/rpi-package`（被收编插件的来源 registry，~30 个文件里
   用作溯源路径）、`@juicesharp/rpiv-todo`（第三方 npm 包）。改名会伪造出处、打断链接。
2. **有意保留的旧数据/旧配置读取**，每处都在原位标注了 `legacy trace` 与失败模式：
   `rpi.value`（`crates/kiqi-harness/src/session/values.rs`）、更新缓存的
   `#[serde(alias = "rpi")]`（`crates/kiqi-cli/src/updates.rs`）、
   `KI_*` 对 `RPI_*` 的回退（`crates/kiqi-cli/src/permissions.rs`）。
   删掉它们会静默丢数据或静默忽略用户已配置的路径。
3. **历史记录**：CHANGELOG 旧条目、设计文档里的旧 worktree 名等。

`website/data/packages.json` 里的 `"author"` 标签、包名、`install` 命令与
`repository` 路径已统一为 `kiqi-`（main 里这一组本来就是全统一的 `rpi-`），
只有仓库名 `pi-rust/rpi-package` 作为外部标识保留。

## 上游已分叉，与本项目无关（**重要，勿误判**）

- 远端 `origin` = `bigfish1913/pi-rust`：**这是一个已分叉、与本项目没有任何关系的仓库**。
  它既不是本项目的上游，也不是下游。
- 因此：**不要**向它提 issue/PR、不要向它推送、不要从它合并/同步、不要把它当作变更来源或
  对齐目标；汇报时也不要说「上游如何」。日常推送主远端 `walgit`
  （`gqf2008/pi-rust` 的本地镜像）；`fork` 只作 GitHub 镜像与发布目标。
- 本项目**没有上游**。引用历史出处时用 commit SHA，不要用 `origin/main` 之类做基线。

## 跟踪与流程

- **`fork` 关掉了 issue**（`gh issue list -R gqf2008/pi-rust` 会报 disabled）。
  批次跟踪落在仓库内的设计文档（`docs/design/*.md` 的分轮记录）与提交信息上，
  不要因为「没有 issue」而跳过立项/验收记录。
- 一批一个 worktree（`.claude/worktrees/wt-<批名>`），批次完成、门禁全绿后 ff-合入 `main`，
  推送 `walgit`（主远端）与 `fork`（镜像），并清理 worktree 与分支。
  **合并、推送与清理是自动收尾，不逐次询问用户**，判据见「硬规矩」。
- 提交信息用 Conventional Commits（中英混排在本仓库历史里都有，前缀必须是规范形式）。

## 本地门禁（= CI 跑的东西）

```bash
cargo fmt --all -- --check
# 本地可选加速：下面每条都可以加 `--offline`（`Cargo.lock` 覆盖全部 feature，
# 离线也能全绿）。CI 在线跑，所以若某个 feature 引入了未锁定的依赖，
# 症状只会是「CI 卡在下载」而你本地无感——偶尔用 --offline 跑一次能提前发现。
cargo check --workspace --all-targets --locked
# 全部 feature 也要能编：否则一个坏掉的公开 feature 会一直无人发现
# （实例：`kiqi-tools` 的 `webp` 调用了 image 0.25 没有的 API，`--features webp` 直接编译失败）
cargo check --workspace --all-targets --all-features --locked
# 测试被非默认 feature 挡住的 crate：`cargo test -p kiqi-ai` 只编译 47 条而不是 206 条
# （整个 anthropic provider 在 `cfg(feature = "providers")` 下），单跑该包会静默跳过。
# 上面的 `--all-features` 那一步只 `check` 整仓，编译不过或跑挂的 feature 门测试
# 在那一步是隐形的。（`kiqi-plugin-sdk` 的同类提示随 cdylib 通道退役删除。）
cargo test  -p kiqi-ai -p kiqi-tools --lib --locked --all-features
cargo test  --workspace --locked
# Cargo.lock 必须覆盖全部 feature 的依赖（离线可解析）；CI 在线跑，
# 缺项的后果只会是「CI 卡下载 / 本地离线报错」，两边都不像根因
sh scripts/check-offline-lock.sh
# `install.sh` 是 `curl | sh` 的入口，离线可测（`--dry-run --version` 不联网）
sh scripts/tests/install-sh.sh
# `release.sh` 必须与它编排的工作流一致（asset 名、push 的远端）
sh scripts/tests/release-sh.sh
# `walgit collab entry` 偶发网络/5xx 失败；包装器只重试瞬态，不重试 schema/验证
sh scripts/tests/collab-entry-retry.sh
# 协作身份：一个 agent 一个 worktree + 一个 walgit principal（dry-run 可测）
sh scripts/tests/setup-collab-agents.sh
# README 的安装指令必须与清单一致（全仓 crate 都是 publish = false）
# 网站 Packages 目录（website/data/packages.json）同样算安装指令：未发布时
# 不许出现 crates.io 链接、裸 `cargo install <crate>`／`cargo add`、非零下载量
sh scripts/tests/docs-install-path.sh
# 文档快照（改了 CHANGELOG.md / README.md 等必须重跑，CI 会比对）
sh scripts/sync-embedded-docs.sh
# `plugin-abi.md` 的 §6.x 编号连续性，以及全仓 `§6.N` 引用是否都能落到真实小节
# （曾经的缺陷：四处引用 §6.34，而它从未被写过）
sh scripts/check-abi-sections.sh
# CI 里每一条 cargo build/check/test 都必须带 --locked（曾经的缺陷：
# `windows-check` 独独没带，于是红叉不再指向某个提交，可能只是上游发了新版本）
sh scripts/check-ci-locked.sh
# 发布矩阵与三处消费清单（updates.rs 的 release_target_triple、install.sh、
# install.ps1）必须给出同一组 target；只改一侧会得到「没人能下载」或「下载 404」
sh scripts/check-release-targets.sh
# 所有「给用户看的示例版本号」（install.sh --help、install.ps1 的 .EXAMPLE 与
# -Version 提示、release-binaries.yml 的 dispatch 输入说明）都必须真的发布过。
# 查 release 而不是查 tag——本地有 v0.3.0/v0.3.19 的 tag 但从未发布，正是漏掉的那类。
# 需要联网；完全无网时设 KI_CHECK_RELEASES_ALLOW_TAG_FALLBACK=1 才退回查 tag（较弱，会告警）
sh scripts/check-install-version-hints.sh
```

注意：goal 测试已用子进程 + tempdir 隔离，跑 `cargo test -p kiqi-cli` 不再改写源码树
（`crates/kiqi-cli/.ki/` 已移出索引并被 `.gitignore` 的 `**/.ki/*` 忽略）。
wasm guest 的**纯逻辑**可带原生单测（`plugins/wasm` 工作区内
`cargo test -p kiqi-<x>-guest`，跑在宿主目标上）；**跨 WIT 边界的行为**一律由
`crates/kiqi-host/tests/e2e.rs` 从宿主侧验证（需 `--features wasm`）。

### 在 worktree 里跑门禁要先隔离 target

**嵌套在本仓库内的 worktree 与主仓共用 `target/`。** 本机 `.cargo/config.toml`（git 忽略，
不在仓库里）设了 `build.target-dir = "target"`，而 cargo 把该相对路径按**`.cargo` 目录的
父目录**解析——所以 `.claude/worktrees/wt-<批名>` 里的构建全部写进主仓 `target/`，
并不是各 worktree 一份：

```bash
$ cd .claude/worktrees/wt-<批名> && cargo metadata --no-deps --format-version 1 \
    | python3 -c "import json,sys; print(json.load(sys.stdin)['target_directory'])"
/Volumes/DataExt/GitHub/pi-rust/target      # 主仓的，不是该 worktree 的
```

两个方向都会给出错结论，且都不像环境问题：

- **worktree 里**复用主仓的陈旧 rlib，报出源码里根本不存在的错误（实例：编出
  “`McpNotice` has no field `authored_sessions`”，而 `grep` 该字段就在源码里）；
- **主仓里**可能跑到分支树的测试二进制：同一命令先报 `699 passed`、再报 `702 passed`
  （702 是分支的条数）。拿任一次当基线都会得出相反的判断。

对策：**worktree 里一律显式给出 target**，主仓基线用独立目录复跑一次：

```bash
CARGO_TARGET_DIR=$PWD/target cargo test --workspace --locked
CARGO_TARGET_DIR=/tmp/ki-base-target cargo test -p kiqi-cli --lib --locked   # 主仓基线
```

判据就看上面那条 `cargo metadata` 打印的路径是否落回主仓。

## 插件通道（改动前先分清）

一方插件**只走 wasm**：`plugins/wasm/kiqi-*-guest`，组件模型 + `plugins/kiqi-plugin.wit`
契约，跑在 wasmtime 沙箱里，按授权拿能力；契约版本见 WIT 的 `package` 行。

**编译期一方插件通道已于 2026-10-21 退役**：`plugins/kiqi-*` crate、`plugin-kiqi-*`
feature、CI 的 opt-in feature 测试与 cdylib 构建都已删除。**cdylib 通道已于 2026-10-26
整体退役**（`kiqi-plugin-sdk`、`ki dev`、`ki install`、`ki package`、`libloading` 装载与
`kiqi_voice`/plugin-stub 的 e2e 随之下线）——wasm 是唯一插件通道。
权限策略由宿主拥有（`crates/kiqi-cli/src/permissions.rs`）。

安装 wasm 版：`ki insmod --build plugins/wasm/kiqi-<name>-guest`。**新增/改插件能力时，
trait 面（`kiqi_plugin`）与 WIT 面必须一起改**，差异要写进 guest README。

## 设计文档在哪

- `docs/design/plugin-abi.md`：wasm 契约与能力模型的**唯一真源**；每轮改动加一节 `### 6.x`，
  §7 是待定项。
- `docs/design/plugin-trait.md`：编译期 trait 契约（**历史存档**；编译期与 cdylib
  通道均已退役）。
- `docs/design/plugin-abi-handover.md`：**接手先读这个**（现状/能力表/上手命令/已知限制/提交轨迹）。
- 未落地的大改走单独方案文件（如 `docs/design/plugin-process-capability.md`）。

## 硬规矩

- 删除文件/目录/远端分支前，先列清单并取得明确授权（用户全局规则 `RULE_未经授权禁止删除文件.md`）。
- **例外（2026-10-20 用户授权，本仓库批次流程）**：批次已完成、门禁全绿，且同时满足
  `git rev-list --count main..<branch>` 为 0、worktree 无未提交改动时，
  **ff 合入 `main`、推送 `walgit` 主远端与 `fork` 镜像、删除该批次的本地 worktree 与
  本地分支是自动收尾，不再逐次询问用户**。`fork` 推送凭据不可用时，推送 `walgit`
  并在汇报中说明 `fork` 未更新，不阻塞本地合入。
  未合入分支、有未提交改动的 worktree、远端分支，以及其它文件/目录，仍按上一条取得授权。
- 上限/超时/额度这类常量上调，单独 commit 并写明新旧值与理由（`RULE_阈值变更.md`）。
- 报告「结论」必须带可复现判据（文件:行号或命令），不得凭命名/同类外推（`RULE_能力与依赖结论必须实核不得外推.md`）。
