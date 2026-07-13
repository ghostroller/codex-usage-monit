# Codex Usage Monitor

本地优先的 Codex 终端监控工具。默认启动实时 TUI，也能一次性输出 text/JSON 快照。

当前实现已在 `codex-cli 0.144.1` 与真实 `~/.codex` 数据上验证，提供：

- 近期 task/thread 状态，以轻量行背景色和图例表达状态，选中项保留证据来源与置信度；
- 选中 task 下的 turns、模型、推理强度、消息摘要和 token；
- 所有可用 5 小时、周及非标准额度桶；
- 当前 5 小时或周重置周期内 task、turn、model 的本地 token 占比；
- 明确标注为 estimated 的额度百分点归因、evidence coverage、confidence、unverified gap 和 Low cycle proxy；
- 完整或按 section 输出的一次性 text/JSON 快照；
- 文件指纹缓存，TUI 刷新只重读变化的 rollout。

## 精度边界

Task、turn 和模型的 token 来自 Codex 累计计数的单调增量；在扫描完整、日志未缺失且累计计数没有回退时，这是本地可观察范围内的精确值。累计计数回退时，工具不会把新的较小基线重复算作消费，而会跳过该歧义样本并将 `ambiguousTokenResets`、数据源和快照标为 partial。

5 小时和周额度的账户总百分比来自 `codex app-server`。周窗口指服务端当前 reset cycle：从 `resetsAt - 10080 分钟` 到 `resetsAt`，不是滚动的过去 7 天，也不一定是自然周。Codex 不提供 task/turn 级官方配额账单，因此额度归因始终是估算。严格证据层只分配相邻快照间隔不超过 5 分钟时观测到的正向百分点变化；服务端整数平台期仍会保留与当前快照一致的细粒度 rollout 样本。完整、当前且可按模型归桶的周期还会把证据缺口按桶内本地 token share 投影为 Low-confidence cycle proxy，让实时 EST 覆盖当前 gauge。

`estimatedAssignedPercent` 和 coverage 只描述严格证据；`unattributedPercent` 是尚未验证的证据缺口。新增的 `proxyProjectedPercent` 表示其中被周期代理投影的部分，它仍包含在 `unattributedPercent` 内，两者不能相加。长间隔、窗口校正和外部活动仍可能构成这个缺口，所以输出同时标记 `externalActivityPossible`；扫描不完整、stale、模型缺失或目标桶缺失时禁用代理并标为 partial。

TUI 的 `EST.Q5H` / `EST.QWK` 分别表示归因给该 task/turn 的估算 5 小时/周额度百分点，不是 token 占比；`LOCAL5H` / `LOCALWK` 才是本地 token share。Low-confidence 值用 `~` 前缀显示；数据质量不足以生成严格证据或周期代理时显示 `-`，不将未知值冒充 `0.0%`。

5 小时和周周期的本地 token share 在整个周期日志扫描完整时可以精确结算。`--days` 控制本地 rollout 的扫描范围；若它没有覆盖所选周期起点，或受 `--max-files`、坏行、不可读文件、counter 回退影响，相应窗口分析必须标为 partial，不能把不完整分母称为精确占比。无论扫描是否完整，task/turn 级 quota share 都仍是 estimated。

即使所有任务都已结束，也只能得到精确 token 与更稳定的最终估算，不能变成 OpenAI 服务端意义上的精确任务额度账单。`settled=true` 可让严格观测分配最高达到 `Medium`；任何包含 cycle proxy 的结果仍保持 `Low`。

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

`--offline` 不启动 App Server，额度改用 rollout 中每个 `limit_id` 最近的本地快照并标记为 stale/partial，避免离线时丢失 `codex` 或 `codex_bengalfox` 其中一个桶。`--redact-content` 不保留 task 标题预览或 turn 消息摘要。TUI 默认使用 `dark` 主题；`--theme light` 启动浅色主题，`bright` 是 `light` 的别名。主题仅影响 TUI 渲染，不改变数据采集或一次性 text/JSON 输出。

## 一次性输出

```bash
codex-usage-monit snapshot
codex-usage-monit snapshot --format json --compact
codex-usage-monit snapshot --section limits,windows,tasks,turns,models,attribution,health
codex-usage-monit limits --format json
codex-usage-monit windows --format json
codex-usage-monit tasks --format text
codex-usage-monit turns --thread <thread-id> --format json
codex-usage-monit models --format json
codex-usage-monit attribution --format text
```

`limits` 优先走轻量 App Server 查询，不扫描 rollout；仅在额度读取失败时扫描本地日志降级。`windows` 输出所有可分析的当前 reset cycle；`snapshot --section windows` 在 JSON 中使用 `windowAnalyses` 字段，每项 attribution 通过 `proxyProjectedPercent` 单独暴露 Low cycle proxy。为兼容既有消费者，task/turn 上原有的 5h `windowTokenUsage`、`localTokenSharePercent`、`estimatedQuotaPercent`、`quotaConfidence`，以及顶层 `models` 和 `attribution`，继续表示首选 5h 分析，不改成周数据。`turns` 的 text 输出包含消息摘要，JSON 使用 `messagePreview` 字段；已知时 `serviceTier` 给出 turn 激活时的服务等级，其中 `priority` 对应 TUI 的 Fast。subagent task 已知直接父会话时，JSON 额外输出可选的 `parentThreadId`。JSON schema 当前为 v1，字段统一使用 camelCase。

退出码：

- `0`：所请求 sections 完整；
- `1`：无法生成有效结果；
- `2`：所请求 sections 有可用但不完整的数据；
- `64`：参数错误。

## TUI 操作

- `1` / `2`：Overview、Data Health；
- Overview 顶栏使用 `5` / `W` 在 `[5h]` 与 `[Week]` 间切换；两个按钮也可用鼠标左键点击，Tasks、Turns、Models 及其中的归因摘要同步使用所选 reset cycle；
- `Tab`、左右方向键：切换视图；
- 默认键盘焦点在 Recent tasks；`j` / `k`、上下方向键选择当前焦点面板的数据行，`Home` / `End` 跳到首尾；
- `Enter`：从 Tasks 进入所选 task 的 Turns；`Backspace`：从 Turns 返回 Tasks；标题中的 `↵` / `←` 也可用鼠标点击；
- `V`：切换 Turns 的默认显隐。默认隐藏时，`Enter` / `↵` 会临时展开 Turns，`Backspace` / `←` 返回 Tasks 时自动收起；
- `M`：切换 Models 面板显隐；最上方的 `[M]Models` 也可用鼠标点击；
- `R`：在 Recent tasks 的 Flat 与 Tree 视图间切换；标题中的 `[R]Tree` 也可用鼠标点击。Tree 中大写 `E` 或 `[E]Collapse` 收起当前过滤树的全部父会话；选中拥有子节点的 task 后，`-` 收起、`+` 展开，行内固定宽度的 `[-]` / `[+]` 也可直接点击；
- `/` / `F`：编辑当前焦点面板自己的 Filter；编辑确认后可用 `Delete` 或标题右侧的 `[Del]` 清空当前面板查询；
- Filter 输入时可用左右方向键、`Home` / `End` 移动光标，`Backspace` / `Delete` 编辑，`Enter` / `Tab` 确认，`Esc` 取消本次编辑；
- `A` / `D` / `S` / `C`：直接切换 All、Desktop、Subagent、CLI 来源，`[` / `]` 循环切换；
- 鼠标左键：点击最上方视图 tab、顶栏筛选控件，或选择 Tasks / Turns 数据行并把键盘焦点切到该面板；右侧滚动条支持点击轨道和按住拖动；
- 鼠标滚轮：只滚动所在的 Tasks 或 Turns viewport，每格 3 行，不改变当前选择或键盘焦点；
- Recent tasks 位于顶部时会随刷新保持顶部，让新建或刚更新的 task/subagent 立即可见；向下滚动后则固定当前阅读位置，直到再次滚回顶部；
- `PageUp` / `PageDown`：滚动当前焦点所在的 Tasks 或 Turns viewport；
- `t`：在 dark 与 light 主题间切换；
- `q`、`Ctrl-C`：直接退出；非搜索状态的 `Esc` 打开退出确认，弹窗内 `Enter` 确认、`Esc` 取消；搜索输入状态中的 `Esc` 仍只取消本次编辑。

本地数据每 2 秒检查一次，账户额度每 45 秒刷新一次。真实 235 文件、约 23.2 万行的 debug 基准中，冷扫约 5.7 秒，无文件变化的缓存刷新约 55ms。
TUI 中的绝对时间使用系统本地时区；text 输出中带 `UTC` 后缀的时间保持 UTC。
Recent tasks 和 Turns 使用轻量背景色及单字符标记表示状态，Turns 面板底部显示统一状态图例，以减少状态列占用并兼容无色终端。Recent tasks 默认保持 Flat 排序；Tree 模式把已知直接父会话且父节点也在当前过滤结果中的 subagent 递归缩进到父节点下，缺失或被过滤的父节点不会被强行补回。拥有子节点的行显示可点击的 `[-]` / `[+]`，并可在 Tasks 焦点下用 `-` / `+` 收起或展开；大写 `E` 和可点击的 `[E]Collapse` 会一次收起当前过滤树的所有父节点，包括已藏在折叠祖先下的嵌套父节点。折叠状态跨数据刷新保留。节点收起时，父行会汇总当前过滤树中所有被隐藏后代的 token，并在同一额度桶内汇总 local share 与 estimated quota；若该分支跨越两个独立额度桶，local share 会按合并后的本地 token 重新计算，estimated quota 显示 `-`，而不会相加两个不同分母的百分点。展开后各行恢复独立显示。挂在父节点下的 child 省略重复项目名，作为 orphan root 显示时仍保留项目名。树根和同层分支由最近活动的后代带动排序，因此新活动的 subagent 仍会把整组带到顶部。Recent tasks 最后一列优先显示 Codex 当前会话标题；标题来自 `session_index.jsonl` 中该 thread 最新的重命名记录，索引缺失时才回退到 rollout 首条消息摘要。顶栏按该标题或项目名（`cwd` basename）执行大小写不敏感的子串筛选，并可与 All、Desktop、Subagent、CLI 单选来源筛选组合使用；历史 `vscode` 标签归入 Desktop，其他未知来源只出现在 All。Turns 的独立 Filter 可匹配 turn ID、模型、推理强度、消息、状态以及 `fast`；非空筛选右侧的 `[Del]` 整块可点击，非输入状态下也可按 `Delete` 清空当前焦点面板的查询。焦点从 Turns 返回 Tasks 时会自动清空 Turns 查询并恢复完整 turn 列表，但 Tasks 查询保持不变。Codex 标记为 Fast 的 turn 会在模型名称后额外显示醒目的 `FAST`，普通 turn 的显示保持不变。醒目的 `▌` 表示当前键盘焦点，较弱的 `▏` 保留另一面板的上下文选择；筛选无结果时 Turns 不显示旧 task 数据。选中的 turn 会在表格下方显示详情：紧凑终端优先保留状态、时长、模型、推理强度和 token breakdown，空间允许时再显示起止时间、占比、置信度、turn ID 与本地保存的最多 72 字消息摘要。Overview 的 Models 面板把所选 5h/Week scope 的 observed evidence、完整 estimated（含 proxy 时带 `~`）、unverified gap、evidence coverage 与 confidence 摘要和按 token 降序的模型表放在一起；proxy 与 gap 会同时显示但不相加。容量不足时标出 `top N/M`。当所选 scope 不可分析时，面板明确显示 unavailable，不会用另一时长的数据冒充；面板隐藏后仍可从最上方的 `[M]Models` 恢复。

工具对已知 `codex` / `codex_bengalfox` 桶使用固定模型映射：仅 `gpt-5.3-codex-spark` 进入 `codex_bengalfox`，其他明确且非空的模型进入 `codex`。两个桶同时存在时，分别使用自己的 reset 边界、服务端观测和额度分母生成 `windowAnalyses`，Models 面板逐桶显示归因摘要；临时只返回其中一个桶时仍执行映射，不把另一个桶的调用塞进现有桶，并将缺失目标桶标为 partial。单桶 task/turn 继续显示 `EST.Q5H` / `EST.QWK`，跨桶 task/turn 则显示 `-`，不把不同额度池的百分点相加。缺失模型的调用不猜桶；与这类调用重叠的额度增量保持 unattributed。任何其他未知的同 duration 多桶组合仍采用保守策略：只选择稳定的周期边界、保留 gauge 和本地 token 构成，并禁用桶级 estimated quota。每项 `windowAnalyses` 独立给出 `partial` 和 `partialReasons`，所以 Week 扫描不完整不会污染完整的 5h 标记。

## 数据与隐私

- 只读 `CODEX_HOME/sessions`、`CODEX_HOME/archived_sessions`、`CODEX_HOME/session_index.jsonl` 和 Codex App Server；
- 不读取 `auth.json`，认证完全由 App Server 管理；
- 不修改、恢复或终止 Codex task；
- 缓存仅存在于 TUI 进程内，不写入索引数据库；
- 进程内保留 `session_index.jsonl` 的当前会话标题；索引没有对应标题时，才回退到最多 96 字符的首条用户消息。每个 turn 最多保留 72 字符的用户消息摘要，不保存完整消息、reasoning 或工具内容；缺少显式 `turn_id` 时只归入当前 active turn，没有 active turn 则不猜测归属。
- 部分 subagent turn 没有明文 `user_message`，消息摘要会显示 `-`，不会从注入上下文反推。
- TUI 与 text 输出会剥离动态文本中的终端控制字符；JSON 使用标准转义。
- task 来源优先按 `originator` 归一化为 `desktop`/`cli`，子代理保持 `subagent`；缺失时才回退到底层 rollout `source`。

## 文档

- [现有开源工具调研](docs/existing-tools.md)
- [Codex 数据能力与边界](docs/codex-data-capabilities.md)
- [产品需求](docs/requirements.md)
- [实现路径](docs/implementation-plan.md)
