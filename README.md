# Codex Usage Monitor

本地优先的 Codex 终端监控工具。默认启动实时 TUI，也能一次性输出 text/JSON 快照。

当前实现已在 `codex-cli 0.144.1` 与真实 `~/.codex` 数据上验证，提供：

- 近期 task/thread 状态，以轻量行背景色和图例表达状态，选中项保留证据来源与置信度；
- 选中 task 下的 turns、模型、推理强度、消息摘要和 token；
- 所有可用 5 小时、周及非标准额度桶；
- 当前 5 小时窗口内 task、turn、model 的本地 token 占比；
- 明确标注为 estimated 的额度百分点归因、coverage、confidence 和 unattributed；
- 完整或按 section 输出的一次性 text/JSON 快照；
- 文件指纹缓存，TUI 刷新只重读变化的 rollout。

## 精度边界

Task、turn 和模型的 token 来自 Codex 累计计数的单调增量；在扫描完整、日志未缺失且累计计数没有回退时，这是本地可观察范围内的精确值。累计计数回退时，工具不会把新的较小基线重复算作消费，而会跳过该歧义样本并将 `ambiguousTokenResets`、数据源和快照标为 partial。

5 小时和周额度的账户总百分比来自 `codex app-server`。Codex 不提供 task/turn 级官方配额账单，因此额度归因始终是估算：相邻快照间隔不超过 5 分钟时，工具按该区间本地 token 比例分配观测到的正向百分点变化。长间隔、缺失扫描、窗口校正和无本地调用的变化保留为 `unattributed`；其他设备或云任务仍可能贡献已观测变化，所以输出同时标记 `externalActivityPossible`。

即使所有任务都已结束，也只能得到精确 token 与更稳定的最终估算，不能变成 OpenAI 服务端意义上的精确任务额度账单。`settled=true` 的估算置信度最高为 `Medium`。

## 构建运行

```bash
cargo build --release
./target/release/codex-usage-monit
```

也可以安装到 Cargo bin 目录：

```bash
cargo install --path .
codex-usage-monit
```

默认扫描最近 7 天、最多 500 个 rollout。常用全局选项：

```bash
codex-usage-monit --days 2 --max-files 200
codex-usage-monit --offline
codex-usage-monit --redact-content
codex-usage-monit --codex-home /path/to/.codex
codex-usage-monit --theme light
```

`--offline` 不启动 App Server，额度改用 rollout 中最近的本地快照并标记为 stale/partial。`--redact-content` 不保留 task 标题预览或 turn 消息摘要。TUI 默认使用 `dark` 主题；`--theme light` 启动浅色主题，`bright` 是 `light` 的别名。主题仅影响 TUI 渲染，不改变数据采集或一次性 text/JSON 输出。

## 一次性输出

```bash
codex-usage-monit snapshot
codex-usage-monit snapshot --format json --compact
codex-usage-monit snapshot --section limits,tasks,turns,models,attribution,health
codex-usage-monit limits --format json
codex-usage-monit tasks --format text
codex-usage-monit turns --thread <thread-id> --format json
codex-usage-monit models --format json
codex-usage-monit attribution --format text
```

`limits` 优先走轻量 App Server 查询，不扫描 rollout；仅在额度读取失败时扫描本地日志降级。`turns` 的 text 输出包含消息摘要，JSON 使用 `messagePreview` 字段。JSON schema 当前为 v1，字段统一使用 camelCase。

退出码：

- `0`：所请求 sections 完整；
- `1`：无法生成有效结果；
- `2`：所请求 sections 有可用但不完整的数据；
- `64`：参数错误。

## TUI 操作

- `1` / `2` / `3`：Overview、Window、Data Health；
- `Tab`、左右方向键：切换视图；
- `j` / `k`、上下方向键：选择 task；
- 鼠标左键：点击 Tasks 或 Turns 数据行选择；
- 鼠标滚轮：在所在的 Tasks 或 Turns 表内每格滚动 3 行，不改变当前选择；
- `PageUp` / `PageDown`：滚动选中 task 的 turns；
- `t`：在 dark 与 light 主题间切换；
- `q`、`Esc`、`Ctrl-C`：退出。

本地数据每 2 秒检查一次，账户额度每 45 秒刷新一次。真实 235 文件、约 23.2 万行的 debug 基准中，冷扫约 5.7 秒，无文件变化的缓存刷新约 55ms。
TUI 中的绝对时间使用系统本地时区；text 输出中带 `UTC` 后缀的时间保持 UTC。
Recent tasks 和 Turns 使用轻量背景色及单字符标记表示状态，Turns 面板底部显示统一状态图例，以减少状态列占用并兼容无色终端。选中的 turn 会在表格下方显示详情：紧凑终端优先保留状态、时长、模型、推理强度和 token breakdown，空间允许时再显示起止时间、占比、置信度、turn ID 与本地保存的最多 72 字消息摘要。

## 数据与隐私

- 只读 `CODEX_HOME/sessions`、`archived_sessions` 和 Codex App Server；
- 不读取 `auth.json`，认证完全由 App Server 管理；
- 不修改、恢复或终止 Codex task；
- 缓存仅存在于 TUI 进程内，不写入索引数据库；
- 默认最多保留 96 字符的首条用户消息作为 task 标题、每个 turn 最多 72 字符的用户消息摘要，不保存完整消息、reasoning 或工具内容；缺少显式 `turn_id` 时只归入当前 active turn，没有 active turn 则不猜测归属。
- 部分 subagent turn 没有明文 `user_message`，消息摘要会显示 `-`，不会从注入上下文反推。
- TUI 与 text 输出会剥离动态文本中的终端控制字符；JSON 使用标准转义。
- task 来源优先按 `originator` 归一化为 `desktop`/`cli`，子代理保持 `subagent`；缺失时才回退到底层 rollout `source`。

## 文档

- [现有开源工具调研](docs/existing-tools.md)
- [Codex 数据能力与边界](docs/codex-data-capabilities.md)
- [产品需求](docs/requirements.md)
- [实现路径](docs/implementation-plan.md)
