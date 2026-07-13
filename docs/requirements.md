# 产品需求文档

更新日期：2026-07-13

## 1. 产品目标

用一个本地终端工具回答五个问题：

1. 最近有哪些 Codex tasks，它们是运行、完成、中断还是状态不确定？
2. 当前 5 小时和周额度用了多少、何时重置？
3. 当前 5 小时或周重置周期内，哪些 tasks、turns 和模型产生了 token，并占本地活动与估算额度的多少？
4. 每个 task 和 turn 使用了多少 token？
5. 如何一次性输出全部或指定部分信息？

## 2. 已确认口径

- task 与 Codex thread 同义；对话指一个 turn；
- token 使用 Codex 本地累计计数的增量，`totalTokens` 不与 breakdown 重复相加；
- 300/10080 分钟窗口显示为 5 小时/周，其他时长使用通用标签；
- 周窗口严格表示服务端 `resetsAt - 10080 分钟` 到 `resetsAt` 的当前 reset cycle，不是滚动过去 7 天，也不是按星期一等日历边界定义的自然周；
- task/turn 额度贡献是 `estimated`，不是服务端账单；
- 同时显示 exact local token share、observed quota delta、estimated quota、coverage、confidence、unattributed 和 external activity risk；
- 所有任务结束后可以把 `settled` 标为 true并把估算置信度提高到 Medium，但不能标为 exact quota；
- 5 小时和周窗口都按 `resetsAt - windowDurationMins` 计算，不使用简单的 `now - duration`。

## 3. v0.1 功能需求

### FR-1 近期任务状态

TUI Overview 显示近期 tasks：

- 状态使用轻量行背景色及单字符标记表达，Recent tasks 和 Turns 不单独占用状态列；
- Turns 面板底部显示统一状态图例；
- `live`、`exact`、`inferred`、`stale` 等证据和置信度；
- task token 总量；
- 当前窗口 local token share 与 estimated quota；
- project、来源、turn 数和标题预览；
- Recent tasks 顶栏提供 task 标题/项目名搜索和 All、Desktop、Subagent、CLI 互斥来源筛选；标题或 `cwd` basename 使用大小写不敏感的子串匹配，两类条件按 AND 组合，历史 `vscode` 标签归入 Desktop，其他未知来源只属于 All。`F` 与来源按钮的 `A` / `D` / `S` / `C` 必须对应真实快捷键，并按 btop 风格只强调快捷键字符；筛选只属于 TUI 状态，不得修改 `Snapshot` 或一次性输出；
- 默认焦点在 Tasks；`Enter` 将焦点移入所选 task 的 Turns，`Backspace` 返回 Tasks，上下方向键与 `j` / `k` 只移动当前焦点面板的选择。当前焦点使用醒目标记，非焦点面板保留弱上下文标记；Tasks/Turns 标题在对应跳转动作可用时分别以轻量 `↵` / `←` 提示，提示出现或消失不得移动筛选控件；无匹配 task 或无 turn 时不得进入 Turns 或显示旧详情；
- 选中 task 的近期 turns、模型、推理强度、最多 72 字符的用户消息摘要、状态和 token；旧日志缺失强度时显示 unknown，不在 task 层臆造单值；选中 turn 后以响应式详情显示时长、总量与窗口 token breakdown，Overview 使用兼容 5h 分析，Window 使用所选 5h/Week scope，并在空间允许时补充起止时间、占比、置信度和 turn ID，不回读或展示完整消息正文。

独立启动的监控进程不能读取其他 Codex runtime 的精确等待状态。未闭合 turn 只根据事件与文件新鲜度标为 `inferred running`，超过宽限期标为 `stale`；不得把 `notLoaded` 当成 completed。

### FR-2 额度窗口

TUI 和 CLI 显示所有 App Server 返回的额度桶：

- used/remaining percent；
- reset time；
- window duration 与 label；
- limit id；
- server/stale provenance；
- 可用时包含 plan、credits 和 reached 状态的 JSON 字段。

同一 duration 同时存在多个当前桶时，周期边界选择必须确定且保守：优先 Codex 产品桶，再优先 `ServerSnapshot` provenance；若仍有多个候选，选定稳定排序后的一个并输出 warning。未选中的候选仍显示 gauge。由于 `UsageCall` 没有 limit id，多桶歧义下只能计算该 duration 的通用本地 token 构成；必须将该分析标为 partial，并禁用 task/turn/model 的桶级 estimated quota。

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

工具同时构建当前 5 小时和周 reset cycle 的窗口分析。每个 `windowAnalyses` 项必须独立携带 `partial` 和 `partialReasons`，不能用 Week 的不完整状态污染完整的 5h。周周期的本地 token share 在扫描完整覆盖周期起点且没有截断、坏行、不可读文件或歧义 counter reset 时是本地可观察范围内的精确值；额度百分点归因仍然只能标为 estimated。`--days` 小于覆盖所选 reset cycle 所需的范围时，只将对应窗口分析标为 partial，即使已扫描到的文件本身都可读。

估算规则：

- 只比较相同 limit id、duration 和 reset epoch 的快照；
- primary/secondary 只是当前响应中的槽位，不属于窗口 key；同一 reset epoch 在两槽之间移动后，历史快照仍须连续比较；
- 在线模式优先使用同一 TUI 生命周期积累的 App Server 快照；
- rollout 快照与当前服务端值明显不一致时不混合；
- 百分比回退开启新的观测 epoch；
- 相邻快照超过 5 分钟不分摊额度；
- 扫描截断、坏行或不可读文件时不进行额度分摊；
- 可分摊的正向 delta 按区间本地 token 比例估算；
- 未覆盖部分保留为 unattributed，并显式标记外部活动仍可能存在；
- 服务端不提供当前 300 分钟窗口时，不得用周窗口冒充 5 小时归因；Models 面板必须显示明确的 unavailable 原因，不能只留下空表或把不可用表达成零模型使用量。
- 任何 scope 不可用时不得回退到另一 duration 冒充；旧顶层 window 字段固定兼容首选 5h 结果，周结果只进入多窗口分析结构。

### FR-4 Task 与 Turn Token

- task/turn token 从单调累计计数计算；
- 完全重复的 `token_count` 不重复计数；
- counter 回退只重建 baseline、不重复计算歧义样本，并报告 warning/partial；
- 嵌套 turn 完成后恢复仍在执行的父 turn；
- task 完成后迟到的最终 token 仍归入刚完成的 turn；
- 用户消息有显式 `turn_id` 时按其归属；缺失时只归入当前 active turn，没有 active turn 时不猜测归属；
- subagent rollout 内嵌的 parent 历史不得重复计入 parent 或 child；
- TUI 显示 total，JSON 同时显示 input、cached input、output、reasoning output 和 total。

### FR-5 一次性输出

默认命令启动 TUI：

```bash
codex-usage-monit
codex-usage-monit --theme light
```

TUI 默认使用 dark 主题；`--theme light` 启动浅色主题，`bright` 是 `light` 的别名，运行中按 `t` 可切换。主题只影响 TUI 渲染，不得改变采集结果或一次性 text/JSON 输出。

一次性输出：

```bash
codex-usage-monit snapshot [--format text|json] [--section ...]
codex-usage-monit limits
codex-usage-monit windows
codex-usage-monit tasks
codex-usage-monit turns [--thread <id>]
codex-usage-monit models
codex-usage-monit attribution
```

TUI 与 CLI 使用同一 `Snapshot`。`windows` 输出当前可分析的 5h/Week reset cycles；`snapshot --section windows` 输出 `windowAnalyses`。JSON 顶层包含 schemaVersion、asOf、partial、所请求 sections，以及 partial 时的来源和错误原因。原有 task/turn 顶层 5h `windowTokenUsage`、`localTokenSharePercent`、`estimatedQuotaPercent`、`quotaConfidence` 以及顶层 `models`、`attribution` 保持兼容，始终表示首选 5h 分析，不得因 TUI 选择 Week 而改变语义。Turns 的 text 输出包含消息摘要，JSON 使用 `messagePreview` 字段。

退出码：`0` 完整、`1` 失败、`2` partial、`64` 参数错误。局部命令只根据与所请求 section 相关的数据源决定 partial。

## 4. TUI 信息架构

### Overview

- quota gauges；
- task 表；
- 选中 task 的 turns，包含消息摘要、可点击选中态和 turn 详情；
- 当前窗口模型 token 表；按 token 降序显示，空间不足时标记 `top N/M`，并区分“5 小时窗口不可用”和“窗口内没有本地模型调用”。

Overview 保持首选 5h 兼容视图；Week 的交互分析只在 Window 页切换，不能改变 Overview 或旧 JSON 字段的 5h 语义。

### Window

- `[5h]` / `[Week]` scope 按钮；`5` / `W` 是真实快捷键，整块按钮支持鼠标左键点击，并遵循 btop 风格快捷键字符强调规则；
- 所选当前 reset cycle 的 duration、实际起止时间和额度；
- 随 scope 同步切换的 observed/estimated/unattributed/coverage/confidence；
- 随 scope 同步切换的 task 与 turn 窗口 token/占比；
- 随 scope 同步切换的模型分布。

### Data Health

- 数据源状态；
- scanned/discovered/truncated/unreadable 文件数；
- parsed/skipped 行数与 ambiguous token reset 数；
- active/completed/uncertain task 数；
- partial 与 diagnostics。

宽度小于 100 列时 task/turn 区域改为上下布局。Recent tasks 的名称搜索和来源按钮嵌入面板顶边，不额外占用窄终端数据行。显示、点击、滚动和键盘导航均把过滤后的位置映射回 `snapshot.tasks` 绝对索引；刷新时按 `thread_id` / `turn_id` 保留仍符合筛选的选择。

Turns 可分页滚动；Recent tasks 和 Turns 以轻量背景色及单字符标记区分状态，Turns 底部提供统一图例。Overview、Window、Data Health 顶层 tab 均可用鼠标左键切换；Window scope 切换不得清空 task/turn 搜索、来源筛选、焦点和仍存在于新 scope 的 ID 选择。Overview 和 Window 中可点击 Tasks/Turns 数据行并切换键盘焦点。除显式视图 tab、顶栏筛选控件、scope 按钮和滚动条外，标题、边框、表头和空白区不得触发选择。`Enter` / `Backspace` 在 Tasks 与 Turns 之间移动焦点，上下键只改变当前焦点面板的选择。参考 btop 的面板路由语义，滚轮只滚动鼠标所在的 Tasks 或 Turns viewport，每格 3 行，不改变选择或键盘焦点；内容超出 viewport 时在右边框显示比例 thumb，点击轨道可跳转、按住左键可拖动，释放后停止拖动，且均不得改变当前数据行选择。dark/light 主题均须保持状态、选中项、额度和 diagnostics 可辨识，按 `t` 即时切换。

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
- `--redact-content` 禁用 task 标题预览和 turn 消息摘要；
- 未识别字段容忍，坏行、不可读文件与累计 token 回退进入 partial；
- TUI/text 输出清洗终端控制字符，JSON 使用标准转义；
- 不向第三方发送本地数据。

## 6. v0.1 验收

- synthetic parser、app-server、归因、输出和 TUI 测试通过；
- 真实 rollout 中 task token 与 turn token 汇总一致；
- 真实 subagent parent replay 不重复计数；
- 真实 80x24 与 120x40 TUI 中筛选控件、Tasks/Turns 焦点与键鼠交互不重叠、不崩溃，并能恢复终端；
- 多额度桶读取成功；
- 235 文件真实基准 warm refresh 小于 200ms；
- partial 与退出码按 section 生效；
- 5h/Week 使用服务端 reset cycle 边界；`--days` 覆盖不足时对应 `windowAnalyses` 为 partial；
- Window 的 `5` / `W` 和鼠标按钮会同步切换 Tasks、Turns、Models 与 Attribution；
- 相同 duration 多桶选择 Codex/Server 优先、输出 warning，未选桶保持 gauge-only；
- `windows` 与 `snapshot --section windows` 输出多窗口分析，旧 5h 字段保持兼容；
- idle 后额度估算仍不声称 exact。

## 7. 后续增强，不属于 v0.1

- 统一通过同一 App Server 启动 tasks，从而获得精确 waiting approval/input 状态；
- 模型、项目、时间等更多筛选与多种排序（task 名称/source 筛选已实现）；
- TUI token breakdown 显示模式切换；
- 跨进程持久化额度快照与索引；
- 非 Codex 产品桶或同 duration 歧义桶的显式高级归因选择；
- 安装包、Homebrew 与跨平台发布流水线。
