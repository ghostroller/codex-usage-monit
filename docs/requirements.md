# 产品需求文档

更新日期：2026-07-12

## 1. 产品目标

用一个本地终端工具回答五个问题：

1. 最近有哪些 Codex tasks，它们是运行、完成、中断还是状态不确定？
2. 当前 5 小时和周额度用了多少、何时重置？
3. 当前 5 小时窗口内，哪些 tasks、turns 和模型产生了 token，并占本地活动与估算额度的多少？
4. 每个 task 和 turn 使用了多少 token？
5. 如何一次性输出全部或指定部分信息？

## 2. 已确认口径

- task 与 Codex thread 同义；对话指一个 turn；
- token 使用 Codex 本地累计计数的增量，`totalTokens` 不与 breakdown 重复相加；
- 300/10080 分钟窗口显示为 5 小时/周，其他时长使用通用标签；
- task/turn 额度贡献是 `estimated`，不是服务端账单；
- 同时显示 exact local token share、observed quota delta、estimated quota、coverage、confidence、unattributed 和 external activity risk；
- 所有任务结束后可以把 `settled` 标为 true并把估算置信度提高到 Medium，但不能标为 exact quota；
- 当前 5 小时窗口按 `resetsAt - windowDurationMins` 计算，不使用简单的 `now - 5h`。

## 3. v0.1 功能需求

### FR-1 近期任务状态

TUI Overview 显示近期 tasks：

- 状态；
- `live`、`exact`、`inferred`、`stale` 等证据和置信度；
- task token 总量；
- 当前窗口 local token share 与 estimated quota；
- project、来源、turn 数和标题预览；
- 选中 task 的近期 turns、模型、状态和 token。

独立启动的监控进程不能读取其他 Codex runtime 的精确等待状态。未闭合 turn 只根据事件与文件新鲜度标为 `inferred running`，超过宽限期标为 `stale`；不得把 `notLoaded` 当成 completed。

### FR-2 额度窗口

TUI 和 CLI 显示所有 App Server 返回的额度桶：

- used/remaining percent；
- reset time；
- window duration 与 label；
- limit id；
- server/stale provenance；
- 可用时包含 plan、credits 和 reached 状态的 JSON 字段。

App Server 失败时仍显示本地 tasks/tokens，并尝试用 rollout 最近快照提供 stale quota。过期窗口不能进入当前窗口归因。

### FR-3 当前窗口归因

按以下层级聚合：

```text
task/thread -> turn -> model token events
```

每级至少提供：

- `tokenUsage` 与 `windowTokenUsage`；
- local token share；
- estimated quota percent；
- quota confidence。

估算规则：

- 只比较相同 limit id、duration 和 reset epoch 的快照；
- 在线模式优先使用同一 TUI 生命周期积累的 App Server 快照；
- rollout 快照与当前服务端值明显不一致时不混合；
- 百分比回退开启新的观测 epoch；
- 相邻快照超过 5 分钟不分摊额度；
- 扫描截断、坏行或不可读文件时不进行额度分摊；
- 可分摊的正向 delta 按区间本地 token 比例估算；
- 未覆盖部分保留为 unattributed，并显式标记外部活动仍可能存在。

### FR-4 Task 与 Turn Token

- task/turn token 从单调累计计数计算；
- 完全重复的 `token_count` 不重复计数；
- counter 回退只重建 baseline、不重复计算歧义样本，并报告 warning/partial；
- 嵌套 turn 完成后恢复仍在执行的父 turn；
- task 完成后迟到的最终 token 仍归入刚完成的 turn；
- subagent rollout 内嵌的 parent 历史不得重复计入 parent 或 child；
- TUI 显示 total，JSON 同时显示 input、cached input、output、reasoning output 和 total。

### FR-5 一次性输出

默认命令启动 TUI：

```bash
codex-usage-monit
```

一次性输出：

```bash
codex-usage-monit snapshot [--format text|json] [--section ...]
codex-usage-monit limits
codex-usage-monit tasks
codex-usage-monit turns [--thread <id>]
codex-usage-monit models
codex-usage-monit attribution
```

TUI 与 CLI 使用同一 `Snapshot`。JSON 顶层包含 schemaVersion、asOf、partial、所请求 sections，以及 partial 时的来源和错误原因。

退出码：`0` 完整、`1` 失败、`2` partial、`64` 参数错误。局部命令只根据与所请求 section 相关的数据源决定 partial。

## 4. TUI 信息架构

### Overview

- quota gauges；
- task 表；
- 选中 task 的 turns；
- 当前窗口模型 token 表。

### Window

- 当前额度窗口；
- observed/estimated/unattributed/coverage/confidence；
- task 与 turn 当前窗口 token/占比；
- 模型分布。

### Data Health

- 数据源状态；
- scanned/discovered/truncated/unreadable 文件数；
- parsed/skipped 行数与 ambiguous token reset 数；
- active/completed/uncertain task 数；
- partial 与 diagnostics。

宽度小于 100 列时 task/turn 区域改为上下布局。Turns 可分页滚动。

## 5. 非功能需求

### 性能

- 默认最近 7 天、最多 500 文件；
- TUI 维护进程内文件指纹缓存；
- 无变化刷新不得重新读取 JSONL；
- 单文件变化只重读该文件，再用缓存事件重建全局累计状态；
- Running/Stale 必须即使在缓存命中时仍按当前时间重算。

### 隐私与安全

- Codex 数据源只读；
- 不读取 `auth.json`；
- 不保存完整 prompt、assistant、reasoning 或工具内容；
- `--redact-content` 禁用标题预览；
- 未识别字段容忍，坏行、不可读文件与累计 token 回退进入 partial；
- TUI/text 输出清洗终端控制字符，JSON 使用标准转义；
- 不向第三方发送本地数据。

## 6. v0.1 验收

- synthetic parser、app-server、归因、输出和 TUI 测试通过；
- 真实 rollout 中 task token 与 turn token 汇总一致；
- 真实 subagent parent replay 不重复计数；
- 真实 80x24 与 120x40 TUI 不重叠、不崩溃并能恢复终端；
- 多额度桶读取成功；
- 235 文件真实基准 warm refresh 小于 200ms；
- partial 与退出码按 section 生效；
- idle 后额度估算仍不声称 exact。

## 7. 后续增强，不属于 v0.1

- 统一通过同一 App Server 启动 tasks，从而获得精确 waiting approval/input 状态；
- 模型/项目/时间筛选与多种排序；
- TUI token breakdown 切换和 turn 详情选择；
- 跨进程持久化额度快照与索引；
- 多额度桶的交互式归因选择；
- 安装包、Homebrew 与跨平台发布流水线。
