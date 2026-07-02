<p align="center"><strong>Codex-Link</strong></p>

<p align="center">
  An independent Apache-2.0 community fork of OpenAI Codex CLI, focused on
  local runtime reliability, compact-survivable context, and event-driven
  background work.
</p>

<p align="center">
  <a href="#english">English</a> · <a href="#中文">中文</a>
</p>

> Codex-Link is not an official OpenAI product. It is derived from
> [OpenAI Codex CLI](https://github.com/openai/codex) and preserves the
> upstream Apache-2.0 license, NOTICE attribution, and repository history.

## English

Codex-Link tracks upstream OpenAI Codex CLI while experimenting with a more
local-first agent runtime. The fork keeps upstream history intact so changes can
be audited and merged forward, but it develops under its own public workflow and
uses its own distribution names.

The default local binary name used by this fork is `codel`. Do not publish
Codex-Link binaries or npm packages under upstream `@openai/*` names.

### Current Focus

- **Link Context Engine**: a structured context capsule that survives compact
  summaries and keeps active goals, progress, evidence, files, blockers, and
  jobs model-visible with bounded size.
- **Link Job Store**: local tracking for long-running commands, completed jobs,
  logs, and trigger events.
- **Event-driven background work**: background commands wake the model only on
  declared triggers such as `on_exit`, regex matches, metric thresholds, or
  plateau conditions. Long jobs should not be supervised by repeated model
  polling.
- **Local observation tools**: `job_observe` and `job_cancel` expose job state
  without forcing a reasoning loop.
- **Typed, triggerable rules**: Link rules can be loaded from disk each turn and
  selected by scope instead of being permanently stuffed into prompt context.
- **Compact survival**: compact summaries point back to full transcript and
  sidecar evidence rather than forcing the model to reconstruct state from a
  natural-language summary alone.

### Status

This fork is experimental and self-use first. It is suitable for local
development and architecture experiments. Treat public releases as unstable
until release artifacts, signatures, and compatibility notes are published.

### Build From Source

Requirements follow the upstream Rust workspace. A typical local build is:

```shell
git clone https://github.com/JinchengGao-Infty/Codex-Link.git
cd Codex-Link/codex-rs
cargo build -p codex-cli --bin codex
install -m 755 target/debug/codex ~/.local/bin/codel
```

Then start the forked CLI with:

```shell
codel
```

For upstream build details, see [docs/install.md](./docs/install.md).

### Documentation

- [Fork notes and roadmap](./docs/link-fork.md)
- [Local development workflow](./docs/link-development.md)
- [Upstream Codex documentation](https://developers.openai.com/codex)
- [Upstream contributing notes](./docs/contributing.md)

### License And Attribution

Codex-Link is distributed under the [Apache License 2.0](./LICENSE), the same
license used by upstream OpenAI Codex CLI.

When redistributing source or binaries:

- keep the `LICENSE` file;
- keep the upstream and Codex-Link entries in `NOTICE`;
- preserve OpenAI Codex CLI attribution;
- clearly mark Codex-Link as an independent fork;
- do not imply OpenAI endorsement;
- do not publish fork packages under upstream `@openai/*` names.

Upstream baseline at fork creation:

```text
openai/codex@db887d03e1 fix(core) Remove full text websocket trace (#30757)
```

## 中文

Codex-Link 是 OpenAI Codex CLI 的独立 Apache-2.0 社区分支，目标不是简单改
prompt，而是把 Codex CLI 改造成更可靠的本地 agent runtime：上下文可以跨
compact 存活，后台任务由本地事件驱动，长输出和证据进入 sidecar，而不是靠模型
反复轮询或从自然语言摘要里猜状态。

本分支默认使用本地二进制名 `codel`。不要以 OpenAI 上游的 `@openai/*`
包名发布 Codex-Link。

### 当前重点

- **Link Context Engine**：结构化上下文胶囊，保存 active goal、进度、证据、
  文件、阻塞项和后台任务，并以有限 token 注入模型上下文。
- **Link Job Store**：本地记录长任务、已完成任务、日志和 trigger 事件。
- **事件驱动后台任务**：后台命令只在 `on_exit`、regex、metric threshold、
  plateau 等 trigger 命中时唤醒模型，不靠模型反复轮询。
- **本地观察工具**：通过 `job_observe` / `job_cancel` 查看或取消任务，避免把
  状态检查变成推理循环。
- **类型化规则系统**：Link rules 每轮可从磁盘重载，并按 scope/trigger 选择，
  不再把所有规则永久塞进 prompt。
- **compact survival**：compact 摘要保留对完整 transcript 和 sidecar 证据的
  引用，避免模型只靠一段自然语言 summary 恢复任务状态。

### 状态

本项目目前是实验性、自用优先的 fork。可以用于本地开发和架构验证；在正式
release、签名和兼容性说明完善前，请把公开构建视为不稳定版本。

### 从源码构建

Rust workspace 的依赖基本沿用上游。一个常见本地构建流程是：

```shell
git clone https://github.com/JinchengGao-Infty/Codex-Link.git
cd Codex-Link/codex-rs
cargo build -p codex-cli --bin codex
install -m 755 target/debug/codex ~/.local/bin/codel
```

然后运行：

```shell
codel
```

上游构建说明见 [docs/install.md](./docs/install.md)。

### 文档

- [Fork 说明和路线图](./docs/link-fork.md)
- [本地开发流程](./docs/link-development.md)
- [OpenAI Codex 上游文档](https://developers.openai.com/codex)
- [上游贡献说明](./docs/contributing.md)

### 许可和归属

Codex-Link 继续使用上游 OpenAI Codex CLI 的
[Apache License 2.0](./LICENSE)。

分发源码或二进制时请遵守：

- 保留 `LICENSE`;
- 保留 `NOTICE` 中的上游和 Codex-Link 归属说明;
- 明确说明本项目派生自 OpenAI Codex CLI;
- 明确说明 Codex-Link 是独立社区 fork，不是 OpenAI 官方产品;
- 不暗示 OpenAI 背书;
- 不使用上游 `@openai/*` 包名发布本分支。

Fork 创建时的上游基线：

```text
openai/codex@db887d03e1 fix(core) Remove full text websocket trace (#30757)
```
