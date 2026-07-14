# 现有开源工具调研

更新日期：2026-07-14

## 结论

目前没有单一开源工具同时满足：近期 task 状态、实时 TUI、当前 5 小时/周服务端额度、task/turn/model token、5 小时额度按 task/turn 估算归因，以及完整/局部一次性输出。

[abtop](https://github.com/graykode/abtop) 最接近任务监控 TUI；[CodexBar](https://github.com/steipete/CodexBar) 的 App Server 额度采集最成熟；[codex-ops](https://github.com/ChenLuoi/codex-ops) 最接近窗口内 session/model/event 分析。这三个项目仍然都没有把当前普通 `codex` gauge、本地 reset-cycle token share 与 task/turn/model 级 Low-confidence EST 放进同一个实时界面和一次性快照。

## 对比

`yes` 表示已实现；`partial` 表示粒度、时效或数据源不完全符合；`no` 表示未实现。

| 工具 | 实时终端界面 | 运行 task 状态 | Codex 5h/周 | task/turn/model token | 一次性输出 | 判断 |
| --- | --- | --- | --- | --- | --- | --- |
| [abtop](https://github.com/graykode/abtop) | yes，Ratatui | yes，进程、`lsof` 与 rollout 推断 | partial，当前会话最新本地快照 | session/model/累计 token；turn history 无稳定 turn id | `--once` / `--json` | 最接近目标 TUI 外壳 |
| [AgentHUD](https://github.com/neochoon/agenthud) | yes，Ink | yes，working/waiting、活动流、会话树 | no | model/context，非完整 token 账本 | once/report/follow | task 状态设计最强 |
| [codexusage](https://github.com/Szpadel/codexusage) | yes，`watch` | no | yes，usage API 定时刷新 | day/month/session/model，无 turn 行 | report JSON | 额度与 burn-rate 看板较完整 |
| [codex-ops](https://github.com/ChenLuoi/codex-ops) | no | no | partial，本地观测快照 | session/model/event；支持真实 5h/7d 窗口归组 | table/json/csv/markdown | 需求 3 的最佳分析参考；见其 [5h window 文档](https://github.com/ChenLuoi/codex-ops/blob/master/README.md) |
| [ContextBar](https://github.com/htahaozlu/context-bar) | yes，`live` | no | partial，transcript 最新快照 | session/model 聚合 | JSON | [collector 源码](https://github.com/htahaozlu/context-bar/blob/main/crates/context-bar-core/src/collect.rs) 尚未提供完整 App Server 实时 probe |
| [ccusage](https://github.com/ccusage/ccusage) | no Codex TUI | no | no；其 5h block 是 Claude Code 口径 | Codex session/model 聚合 | JSON | 成熟历史报表；见 [Codex 支持](https://ccusage.com/guide/codex/) 与 [blocks 说明](https://ccusage.com/guide/blocks-reports) |
| [CodexBar](https://github.com/steipete/CodexBar) | no，主界面为 macOS 菜单栏 | no | yes，App Server 或 Web/OAuth | session/project/model/day，无 turn 明细 | [CLI text/json](https://github.com/steipete/CodexBar/blob/main/docs/cli.md) | 服务端额度采集参考最好；见 [provider 文档](https://github.com/steipete/CodexBar/blob/main/docs/providers.md) |
| [CodeBurn](https://github.com/getagentseal/codeburn) | yes，分析 TUI | no | partial | project/model/session/任务类别；任务类别不是 Codex turn | JSON/status/export | 历史分析 UI 参考 |
| [codex-ratelimit](https://github.com/xiangz19/codex-ratelimit) | partial，进度条 | no | partial，最新 JSONL | no task/turn 分解 | JSON | 小型额度查看器，不覆盖综合需求 |

## 排除项

- [weft](https://github.com/michaelfromyeg/weft) 是跨 agent harness/plugin 编译框架，不是监控工具。
- [agentwatch](https://github.com/ankit-aglawe/agentwatch) 的 [Codex adapter](https://github.com/ankit-aglawe/agentwatch/blob/main/crates/agentwatch-adapters/src/codex_cli.rs) 仍是未完成实现，once/report/export 不能作为现成方案。

## 自研理由

现有项目可以提供三个成熟参考方向：

1. abtop/AgentHUD 的 task 发现与终端信息架构；
2. CodexBar 的只读 App Server 额度读取；
3. codex-ops/ccusage 的 rollout token 重建。

本项目增加的核心能力是把三条链路放进同一个 snapshot model，并把 exact/partial local token、账户级 `codex` gauge、Low-confidence estimated quota share、confidence 和 external risk 分开表达。EST 明确使用 `codex usedPercent × 本地非 Spark token share`，不会把 token 占比冒充官方额度账单。

## Idle 后是否精确

所有对话结束后，可以精确结算完整落盘的 thread/turn/model token。服务端仍只暴露账户级整数 `usedPercent`，没有逐 task/turn 扣费账本，所以 5 小时额度归因只能提高稳定性，不能变为严格精确值。
