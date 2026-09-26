# 项目瘦身与重构：修改方案及本地 Agent 执行说明

日期：2026-09-26  
仓库：`ghostroller/codex-usage-monit`  
依据：[`refactoring-review-2026-09-26.zh-CN.md`](refactoring-review-2026-09-26.zh-CN.md)  
审核修订提案：[`refactoring-proposal-2026-09-26.zh-CN.md`](refactoring-proposal-2026-09-26.zh-CN.md)

> **本文是可执行的任务拆解，不是全部路线的实施授权。**
>
> 用户下达“按本文件默认范围实施”时，执行 M0—M6：修订提案、明确的小范围机制替换、保持外部语义的应用层解耦，以及收尾验证。A 方案的产品切换/删除与 SQLite 原型、全面迁移均不在默认范围。不要因遇到这些未批准事项而停止其他独立任务。

## 1. 开工约定与范围

### 1.1 先读实际仓库，不照旧行号机械修改

依次读取当前适用的 `AGENTS.md` 和已配置的本机环境指令、配套审核意见、原提案，再检查实际 HEAD 的相关代码和测试。不要假设必须存在名为 `environment.local` 的文件，也不要为了本任务额外建设环境配置系统。

审查读取提交为 `abef359eac136a2b004c7e4b98890cee27bd37fe`；原提案代码基线为 `1c153229d5c167a0b02eca12fbc6ce4c4770e26f`。这些是比较基线，不是要求 checkout、reset 或还原到该版本。

记录开工 HEAD、工作区状态和实际工具链。保留用户未提交修改；不得使用破坏性清理、覆盖已有工作或重写历史来获得“干净基线”。若相关实现已更新，按实际代码调整任务，并记录已被解决、仍成立和不再适用的部分。

### 1.2 默认范围与决策门

| 工作 | 默认范围 | 限制 |
| --- | --- | --- |
| M0：复核、修订原提案、建立契约/调用清单 | 是 | 不把建议改成“产品已批准” |
| M1：用 `semver` 替换版本 parser | 是 | 升级身份、降级及发布策略保持 |
| M2：标准库锁替换生产 `fs2` 使用 | 是 | 保持原生锁与私有文件安全契约 |
| M3：应用查询与 CLI/TUI 编排解耦 | 是 | 保留现有 A/C 状态、`All`、JSON、退出码及可见行为 |
| M4：小范围 XML 解析替换 | 是 | 不默认修改生成输出与服务指纹 |
| M5：同契约私有临时文件封装 | 是，先分类后实施 | 不改变恢复命名、发布顺序、权限与持久性 |
| M6：回归、复审与净收益记录 | 是 | 不以未执行的检查冒充通过 |
| R2：已经落地的 Windows 三项迁移 | 只保留回归 | 不重复迁移 |
| A0—A5：来源隔离产品路线 | 否 | 必须先明确选择 A，并批准兼容/迁移规则 |
| P1/P2：SQLite 有界原型 | 否 | 被明确委托后，才在隔离数据上实施 |
| SQLite 生产存储切换 | 否 | 原型通过之后仍需独立批准 |
| R7/R8 其余库替换、daemon/IPC 重构 | 否 | 不因“顺手优化”纳入 |

批准 M3 不代表批准新的集合报告 schema；批准 A 不代表批准 SQLite；批准 SQLite 原型不代表批准操作真实历史数据。

### 1.3 本次任务不默认授权的外部操作

不自动 push、发 PR、创建 tag/release、部署、安装/卸载真实服务或触发 GitHub Actions。只有用户明确要求时才执行相应操作；尤其不要通过 push 间接触发未授权 CI。

本机验证遵循本机环境指令，不把某台机器的限制固化到项目级 `AGENTS.md`。不要为补齐不存在的原生平台而自行启动 Docker 测试、远端测试或 GitHub CI。测试优先使用隔离临时目录、合成样本与仓库夹具，不修改真实 Codex 历史、账户配置或现有服务。

## 2. 执行顺序、并行与提交边界

建议依赖顺序：

```text
M0：基线/契约/任务所有权 + 原提案修订
  ├─ M1：semver
  ├─ M2：标准库文件锁
  ├─ M3：报告/应用层解耦
  └─ M4：XML 解析
       （以上可按文件冲突情况并行）

M2 完成相关私有文件改动后 → M5：临时文件同契约归并
各工作包完成 → M6：组合回归、复审、交付记录

A0—A5、P1/P2：仅在各自决策门打开后追加，不阻塞默认范围
```

支持多 agent 的环境中，主 agent 负责集成、共享接口与最终验收；已就绪且文件所有权不冲突的实现任务可以并行。不能让所有 worker 等待同一个顺序研究任务，也不能让多个 worker 无协调地同时修改 `Cargo.toml`、`Cargo.lock`、相同测试模块或私有文件底层。

可行分配是版本更新一组、锁及相关测试一组、应用层一组、XML 一组。主 agent 统一依赖变更；M5 避开 M2 对底层文件的并发编辑。没有多 agent 能力时按依赖执行，不为本任务新建调度系统。

每个工作包保持独立可审查的提交边界；按仓库既有约定处理本地提交，不自动 push。不要把 semver、文件锁、产品语义切换和数据库迁移混入同一提交。各 worker 做针对性验证，组合完整回归由主 agent 统一执行，避免重复跑多套全量测试。

## 3. M0：复核和原提案修订

### 目标

把审核意见转化为当前仓库可落实的任务，消除提案中的隐式授权和调用边界歧义。

### 必做修改

按审核文件 D1—D8 修订原提案。重点调整：R4 可独立前移，R7 默认暂缓，R2 已完成；C1 不循环完整报告加载器；C4 加同机旧进程；文件锁加混合版本验证；SQLite 门槛具体化。

保留原提案的审查日期、历史快照与证据说明。在开头增加简短修订记录和本次实际 HEAD；不要把旧统计值标成新测量，也不要把 A/C 或 SQLite 的待决定项改成已批准。

建立下面两个紧凑清单即可，无需另写大量架构文档：

**查询入口/契约清单。** 检查 `collect_and_load_report_history*`、`load_v2_history_since_inner`、`load_remote_overview_history`、`AllIncluded` 及其他实际调用者。记录入口是否采集、stage、flush、恢复、读取 quota、执行同步；记录隐私/所有权约束、失败处理和输出消费者。覆盖 CLI summary/trends/health、后台刷新和非当前显示页面，具体入口以 HEAD 为准。

**A 路线候选删除清单。** 只做源码盘点，不删除功能。记录 facts/归并相关符号的生产消费者，区分中心专用、旧 v5 服务端和聚合同步共享职责。该清单是未来 A 决策依据，不是已经批准的删除列表。

### 退出条件

原提案修订完成；默认范围、产品门和原型门一目了然；相关实现与测试位置已找到；不存在用“后面实现时再决定”掩盖外部 schema、退出码或安全边界的情况。

## 4. M1：`semver` 替换

### 主要落点

`src/update.rs` 中 `compare_versions` 及其升级判断/测试，`Cargo.toml`、`Cargo.lock`。精确函数与依赖版本先按实际 HEAD 核对。

### 实现要求

使用 `semver::Version::parse` 验证完整输入，再使用 `cmp_precedence` 比较升级优先级；不要先去掉 `+` 后缀再解析，也不要直接用默认 `Ord`。[E1]

原入口已有的输入约定、错误上下文应先确认；非法版本不能被当作相等、更旧或默认版本。源码 build 身份、同版本冲突、禁止降级、安装 receipt 和更新事务保持原逻辑。仅声明必要的直接依赖，不连带升级无关包或重写发布脚本。

### 最低验收

| 场景 | 要求 |
| --- | --- |
| 主/次/补丁与预发布顺序 | 既有正常升级顺序不变 |
| `01.0.0`、`1.0.0-01` | 按合法版本契约拒绝 |
| `1.0.0+`、非法 metadata | 拒绝，而非截断后接受 |
| 长数字预发布标识、合法边界 | 无自制窄整数溢出或错误排序 |
| 只有 metadata 不同 | 升级优先级相同 |
| 同版本不同源码 build | 仍由原有身份冲突规则处理 |
| 降级及相同版本策略 | 既有安全策略不变 |

删去不再使用的手写 parser 和专属于旧实现的冗余代码。对错误接受的非法版本收紧校验，可在变更记录中明确说明，无需模拟旧错误。

## 5. M2：标准库文件锁迁移

### 主要落点

先全仓检索 `fs2`、`FileExt`、`lock_contended_error`、`lock_is_contended`，再覆盖 `file_lock.rs`、`private_state_store.rs`、ownership/profile lease、诊断和实际发现的调用点。不得仅替换一个包装器便宣称完成。

### 实现要求

保留现有 guard 和显式解锁责任；将通用锁调用迁移到标准库，按 `TryLockError::WouldBlock` 与真实错误区分竞争和故障。核对工具链支持，不为迁移无故升级项目 MSRV。[E2]

锁文件的位置、创建/打开参数、共享策略、no-follow/文件身份检查、锁后重验证、错误传播和释放顺序保持不变。不要混淆独立打开的句柄与 `try_clone`/继承句柄的所有权，避免重复获取同一底层已持有锁的对象。

文件锁保证互斥，不自动替代历史 ownership 或 profile lease；后两者不能在换 API 时被删掉。

### 最低验收

覆盖同进程和跨进程竞争、共享读者共存、独占排斥、try-lock 非阻塞、真实 I/O 错误、提前失败后的释放、复制/继承句柄存活时的 guard 释放，以及独立共享读者不被另一个 guard 析构误释放。

增加**旧 `fs2` helper ↔ 新 std helper 的双向跨进程竞争**：独占锁彼此排斥，共享锁行为与现有契约一致，释放后可以获得锁。用显式 ready/continue 握手和有界超时，不依赖随意 sleep 判断竞争。

Windows 原生覆盖实际 sharing 与句柄行为；Unix 语义在有相应原生环境时验证。无条件的“跨平台都通过”必须有对应执行证据。

生产使用清零后再移除生产 `fs2` 依赖。若为互操作验证保留测试依赖，应明确记录，不要把“生产退役”写成“整个锁定图完全删除”。不要为了短期测试额外维护永久的旧锁生产实现。

## 6. M3：先做保持外部语义的应用层解耦

### 6.1 工作目标

从 CLI/TUI 中提取真正可复用的应用编排与查询结果边界，使展示层不再重复建立历史环境、控制写入与拼接查询。**这一阶段保留当前完整归并能力和对外输出，不把 `AllIncluded` 改成多来源求和或新集合。**

主要落点为 `src/cli.rs`、`src/history_query.rs`、`src/tui.rs` 及实际历史 runtime 接口。新增模块、函数和类型按仓库习惯命名，不要求照搬下文示意，也不建设通用服务框架。

### 6.2 最小内部边界

拆分“接收/准备一次 observation 并按需提交”与“使用已准备环境查询”。不能在每个来源循环里重新调用 `collect_and_load_report_history_selected`。

内部结果应能表达来源用量、账户 quota、各自状态、警告、revision 和只读限制。可以沿用已有结构并增设小型适配层，不要求一次改动全部报告 DTO。

每次一致性查询尝试共享查询时刻、范围、来源策略、项目映射、隐私上下文和额度结果；维持现有读取预算与资源上限。来源策略、权限或 revision 改变时按现有规则有界重试，重新建立有效上下文。不要通过无界缓存或放宽快照检查减少读取次数。

明确的读取阶段不承担新采集、`stage_local_collection`、`flush_staged`、主动同步或隐式格式迁移。必要的历史恢复与所有权协调放回明确的准备/恢复阶段，不是简单删除。

只读模式可以保留原有内存 staged overlay，但不得持久化本应受写权限约束的历史。诊断是否可写遵循原有独立规则，不把“历史只读”误解为禁止一切日志，也不借日志路径写历史。

### 6.3 quota 与错误结构

quota 为账户观测，不是某来源 token 总数的子字段。内部应避免每查一个来源就重复遍历全部 quota 来源，并保留同账户确认、来源纳入、redaction、reset/时间槽和 provenance 规则。

内部可区分整体环境失败、单源用量失败和 quota 失败。**默认 M3 通过兼容适配层维持既有可见错误、警告、partial 和退出码。** 新的“部分来源成功仍输出集合”或“quota 故障不阻断 token 展示”如果改变现有输出行为，必须单列为行为变更，在获得对应范围授权后再发布；不能用重构提交夹带。

`AllIncluded` 的现有归并仍走旧语义；精确 local/remote 路径继续保留来源选择。显式 remote 不可用时不能自动回退 local。

### 6.4 消费者迁移方式

先迁移共用报告入口和一个代表性 TUI 路径，验证新边界确实复用，再迁移其他相同职责消费者。不得只移动代码、保留所有旧入口及重复实现，却宣称应用层完成。

不必一次改掉全部渲染函数；要确保剩余兼容包装薄且没有再次承担 stage/flush/同步。若暂留包装函数，应注明消费者和收敛条件，不长期维护两套独立业务引擎。

### 6.5 最低验收

| 编号 | 验收 |
| --- | --- |
| Q1 | 一次应用请求中的本地准备/提交不会因为来源数量成倍执行；用调用计数或现有 spy 验证 |
| Q2 | 同一次有效查询尝试共享账户 quota 读取/合并；必要的有界重试单独计数 |
| Q3 | 真正读取阶段不新增采集、flush、主动同步或隐式迁移 |
| Q4 | 只读和 profile/ownership 改变时不越权写入；写前重验证仍有效 |
| Q5 | local、显式 remote、AllIncluded 的既有 token/费用/partial/告警及输出结构保持 |
| Q6 | quota 错误内部可区分；对外仍符合本阶段冻结的兼容行为 |
| Q7 | 具体来源不可用不回落本地；无数据、离线、过期不变成完整的零值 |
| Q8 | CLI summary/trends/health、后台刷新、缓存失效及非当前页消费路径均有覆盖 |
| Q9 | 源策略、revision、redaction 或项目映射改变，不跨边界复用旧上下文 |
| Q10 | 本阶段不建立“所有远端同一时刻快照”的新承诺，不扩大读取预算 |

测试应尽量复用现有 fixture 和故障注入工具，不为调用计数构造一套庞大的 mocking 框架。

## 7. M4/M5：小范围机制整理

### 7.1 M4：XML 解析

先定位 `service.rs` 中 launchd plist 的字符串搜索 parser，以及已有 `quick-xml` 用法和锁定版本。复用现有依赖，限定读取项目需要的字段，验证目标字典层级、键值对应、值类型和重复键。

先建立正常服务定义及指纹的黄金样本，再替换解析。覆盖转义、换行/回车、空元素、错误嵌套、截断或畸形输入、输入大小/深度边界以及不支持的声明/实体策略。不要把拒绝无效结构做成静默丢字段。

标准 plist 声明如何处理应基于现有合法样本确定；不应为了限制复杂输入而误拒绝项目自己或系统正常产生的服务定义。解析不得引入外部资源解析或任意文件/网络读取。

**默认不改生成格式、转义策略或服务指纹，不改 `launchctl print` 的非 XML 文本路径。** 生成端确需变化时另交兼容证明；原生 launchd 运行检查只在相应环境与授权下执行，纯解析测试通过不能冒充原生服务验证。

### 7.2 M5：私有临时文件和发布

先按以下契约分类实际调用点，不按文件名或表面语句相似度统一：

| 类别 | 本轮允许做什么 | 不允许顺带改变什么 |
| --- | --- | --- |
| 普通可丢弃临时文件 | 复用分配/清理，必要时评估 `tempfile` | 输入边界、目录和权限策略 |
| 可恢复历史发布 | 只归并确实相同的安全/发布原语 | 恢复命名、commit 顺序、redo/manifest 语义 |
| 安装升级候选文件 | 只整理经证明相同的底层操作 | 二进制身份、receipt、rollback、原生替换语义 |

保持安全创建、写入、文件同步、关闭/替换策略、发布后验证和目录同步等既有边界。Windows 打开读者下替换、ACL、no-follow 和实际对象身份验证不得降级。

`NamedTempFile::persist` 不承担全部持久化同步；RAII 也无法在进程被终止之后执行清理。参与 `.target.pid.sequence.tmp` 恢复识别的路径，默认不改成随机文件名。[E3]

优先复用已有私有文件组件。只有同契约调用点确实减少重复责任时才提取公共函数；如果适配代码反而更多，就记录保留结论，不为满足“瘦身任务”强行引库。`tempfile` 是否由开发依赖变成生产依赖须记录净成本。

### 7.3 退出条件

XML 正常生成/指纹不变，错误结构处理有测试；临时文件抽象没有放宽权限/恢复/持久性，故障时保留旧数据或按既有明确状态恢复。未修改的高风险发布路径清楚列出，不宣称已经完成全仓替换。

## 8. M6：验证、复审与交付

### 8.1 验证次序

按仓库现有脚本和实际支持的 features 执行：目标测试 → 受影响模块测试 → 本平台构建/Clippy/格式检查 → 适当的组合回归。不要凭空添加 `--all-features` 或更换 CI 策略。

下列只是命令形态示例，具体过滤词和检查参数必须从当前仓库确定：

```text
git status --short
git rev-parse HEAD
cargo fmt --all -- --check
cargo test --locked <已经确认实际存在的测试过滤词>
```

执行过滤测试后要确认确实运行了预期测试，不能把“0 tests”当作验证成功。新增依赖应按仓库方式更新锁定文件，更新完成后用锁定依赖验证；不要为通过 `--locked` 随意删除锁定约束或重置依赖。

锁、文件身份与平台服务相关场景以原生平台证据为准；其他平台暂不可执行时，标记“未执行/待原生验证”，不要用静态阅读、交叉编译或容器测试伪装成原生运行。进程强杀测试证明的是对应进程中断路径，不能自动当作断电持久性证明。

### 8.2 合并前独立复审

主 agent 或独立 reviewer 对组合 diff 检查：有没有扩大任务范围、改变 All/JSON、放宽隐私或 ownership、删掉仍被 v5 使用的 handler、误删共享摘要/subagent/quota、吞掉错误、留下旧生产引擎，以及通过删测试减少失败。

源码、测试、文档和实际验证应指向同一最终代码状态。测试后继续修改代码时，应按影响范围补跑，不把先前 SHA 的结果无条件沿用为最终证据。

### 8.3 净收益记录

区分生产实现、测试、注释/空行及纯移动。记录实际删除的重复职责、剩余适配代码、新增依赖/feature、生产与开发依赖差异，以及经实测的性能/资源数据。没有测量的项目填“未测”，不承诺百分比收益。

不以减少文件数、减少测试或把实现搬到另一文件为目标。普通机制替换允许总行数暂时因边界测试增加；价值应解释为减少手写机制或降低修改风险。

### 8.4 完成报告格式

最终提供以下内容，并将精简结果补入第 11 节：

```text
开工 HEAD / 最终 HEAD（有未提交改动则说明）：
完成的 M 编号与对应提交/文件：
重要实现选择与兼容边界：
实际执行的测试、平台、结果与日志位置：
未执行的验证及具体原因：
生产/测试代码和依赖变化；已测/未测收益：
复审发现及处理：
仍关闭的决策门：A / SQLite 原型 / SQLite 生产迁移。
```

遇到一个工作包无法安全完成时，保留其事实和阻碍，继续其他独立工作。不要反复请求用户确认已定义的技术细节；只有确实涉及产品能力、破坏性数据迁移或外部操作的决定才停在对应决策门。

## 9. 条件路线 A：仅在产品明确选择 A 后执行

以下是未来的实施约束，**不是默认工作包**。M0 可以完成源码清单和场景草案，但不得据此关闭 facts 或改变输出。

**N4 决策材料状态（2026-09-26）：**以下草案已按 `62f6a3ab2a787759867379071ffd5fa1d00b4ba6` 增量复核；复用第 11.1 节清单，没有重做全仓盘点。手动/自动 facts follow-up、AllIncluded reconciliation、v5 handler 及共享本地物化消费者仍存在。M3 已提供准备一次、共享 quota/预算及逐次 fence 的内部边界，尚未提供这里拟议的来源集合产品。**材料完成不等于选择 A；下列行为、schema、兼容与删除策略全部待批。**

推荐决定：若目标明确收窄为“各来源记录的日常用量”，选择 A，并同时接受下表能力损失及 A0/A4 契约；若复制、迁移、分叉后的全局唯一事件用量仍属核心需求，保留 C，结束 A 删除路线。批准前继续 C。真实用户中复制/分叉的频率、旧 JSON 消费者数量、各能力使用率及愿意接受的损失均**未知**；未读取真实历史或增加遥测，不以代码体量代替产品证据。来源表示观察/导出节点，不证明用量实际在哪台机器产生。

| 合成场景 / 决策输入 | 保留 C 的含义 | 选择 A 的唯一草案预期 |
| --- | --- | --- |
| 独立会话：来源甲 20、乙 30，覆盖完整 | 唯一事件投影为 50 | 分列甲 20、乙 30；不展示全局唯一总量，即使此例相加恰好为 50 |
| 完整复制：两源各有相同的 100，完整摘要证明相同 | 折叠为 100，不必额外补 facts | 两行各 100，并注明来源记录可能重叠 |
| 公共 100，甲新增 20、乙新增 30；facts 完整有效 | 公共部分一次、独有部分均保留，合计 150 | 甲 120、乙 130；首个 A 版本不提供记录合计，绝不把旧 All 的 150 同名改成 250 |
| 上述分叉 facts 缺失/过期，但每源聚合完整 | 按现有证据排序选权威副本并标限制；仅这些数字不能确定权威或精确总量，不能取最大值 130 | 仍分列 120、130；不为跨源 facts 缺失发起补齐，也不因此降级每源已完整的覆盖 |
| 相同事件 ID 的 facts 冲突，但每源聚合完整 | 按现有强证据参与者选该事件、保留其他独有事件并告警；未给出事件内容/证据排序时数值未知 | 展示各源已有记录，跨源冲突不改变各源值；不得宣称哪一份是唯一真实消费 |
| 父会话与 subagent 有不同 thread ID | 保留 lineage 和子代理独有用量 | 保留每源 lineage；源 ID 与 thread ID 共同定位，不能把子代理当副本删掉 |
| 同账户两源在同一 reset 窗口观察到 40% / 45%，后者更新且证据兼容 | 按账户 quota 规则选用 45% | 同样单列账户 GLOBAL 45%，不算 85%；账户身份/窗口不能确认时分开标未知，不强行合并 |

### A0：冻结语义、依赖与兼容策略

选择来源记录语义，确认不再提供跨来源同会话唯一事件总量。冻结 CLI 默认行为、新集合 JSON/schema、旧 `--source all` 处理、UI 保存状态、逻辑会话 ID、项目/任务 ID、partial 与退出码映射。

**待批行为契约草案 v1：**推荐一次有版本说明的产品切换，不长期维持 A/C 两套引擎。下面是确定的建议值，不能当成当前 CLI 已支持的选项。

| 接口 / 状态 | 推荐草案与验收结果 |
| --- | --- |
| CLI 默认与显式选择 | `summary` / `trends` 不给来源时返回已纳入来源集合；新增 `--all-sources` 显式拼写。`--source local` / `--source <NodeId>` 保留精确来源计算，返回单行集合；与 `--all-sources` 同用拒绝。显式旧 `--source all` 在读取/写入前报迁移提示并退出 64；不静默替换含义。`health` 继续检查所有纳入来源，不顺手增加来源过滤参数 |
| 集合 JSON | 新 envelope 固定 `schema: "source-reports"`、`schemaVersion: 1`、`reportKind: summary/trends/health`、`asOf`、`status`、`sources`、`accountQuota`、`diagnostics`、`error`；不复用旧顶层 `schemaVersion: 1` 的意义。每行带 `sourceId`、`generation`、`revision`、`freshness`、`status`、`report`、`error`；取不到的身份版本字段为 `null`，不能编造 0。`report` 保留相应精确来源报告的指标、覆盖度、价格不确定性和精确数值编码，失败时为 `null`；不加入顶层总 tokens/费用 |
| quota 与 Health | Summary/Trends 的 `accountQuota` 单独返回 `status: available/notObserved/failed`、已按现有账户规则投影的 `report` 或 `null`、结构化 `error`；同一请求只准备一次，来源行不复制 GLOBAL quota。Health 只给 quota 状态/诊断，`accountQuota.report` 固定 `null`，每源 `report` 只含现有 Health 允许的健康信息；recorder/service 放在 `diagnostics` 一次，不借新 envelope 泄露 token/account 明细 |
| TUI / 图表 | 原 All 入口更名为 Sources，Overview/Models/Summary/Trends 均按源展示；Health 展示逐源诊断。图表分母与来源、时段一致；项目映射保留为标签分组，首个 A 版本不提供跨源记录合计。若未来增加，必须另行命名“来源记录合计（未去重）”，不能还原为 All |
| 保存状态 / 标识 | 一次版本化迁移将保存的 All 选择转为 Sources，并显示一次语义变更提示；仍存在的精确 source ID 保留。已删除/排除来源仍显示不可用选中态，不回落 local。旧跨源 logical session ID 失效并提示重选；来源会话/项目/任务 ID 使用带类型、来源维度的键，不把旧 logical ID 猜成 thread ID；项目映射和本源 lineage 保留 |
| 出错输出 | 能完成请求初始化时，即使全部来源失败也输出上述 envelope；逐行 `error` 使用稳定 code 加诊断文本。参数错误输出 stderr 且退出 64；无法建立可信请求/安全状态时 stderr 退出 1，不尝试绕过 fence 读取。沿用现有 stdout 管道提前关闭的成功退出约定，不由本路线另改 |

**身份版本字段的推荐类型：**`generation` 为正 `u64` 的十进制字符串，本地来自 `SourceIdentity::generation`，SSH 来自已验证 binding 的 `SourceGeneration.generation`；不能填中心 `ingest-gen-*`。`revision` 是带类型的诊断对象或 `null`：本地用 `{kind: "localReservationHighWater", value: "42"}` 表示相同 profile/redaction 下实际读到的保留高水位，可能含未提交编号空洞，绝不称已提交次数；SSH 用 `{kind: "remoteActiveGeneration", value: "ingest-gen-…"}` 表示实际读取的中心 active generation 字符串。两者都只是已观察到的版本信息，不是完整报告的 ETag、增量游标或写入前置条件；它们不能替代完整 active binding、协议/catalog revisions、ownership/profile、quota/live、映射和策略的内部 fence，也不代表跨源同一时刻快照。无法可信读取对应值时为 `null`，不把路径、私有指纹或合成数字当版本。初版 A 不提供基于这两个字段的增量/条件查询接口。

两条成功查询的身份片段示例（其余 report/status 字段省略，非完整响应；所有数字均为合成）：

```json
[
  {"sourceId": "node-11111111111111111111111111111111", "generation": "1", "revision": {"kind": "localReservationHighWater", "value": "42"}},
  {"sourceId": "node-22222222222222222222222222222222", "generation": "3", "revision": {"kind": "remoteActiveGeneration", "value": "ingest-gen-33333333333333333333333333333333"}}
]
```

A0 冻结时须把成功本地/SSH、保留后未提交空洞及取不到版本的完整响应加入黄金样例，并检查 `u64` 超过 JavaScript 精确整数范围时字符串仍无损；不把当前内部 Rust DTO 的数字编码直接暴露为上述新 schema。

`status` 的判定次序同样待批，按下列优先级形成唯一结果。来源的 `freshness` 独立取 `fresh/stale/offline/unknown`；来源 `status` 取 `complete/partial/noData/failed`，无数据与有证据的零值不同。没有纳入来源时返回 `empty`/1。仅用于展示的诊断不自动变成 partial；覆盖缺口、现有报告的 partial/价格不确定性判定、离线/过期和查询失败均须保留。

| 场景（前面的规则优先） | Summary / Trends 集合 status、退出码及输出 |
| --- | --- |
| 全部选中来源查询失败 | `failed`，1；每源保留错误，quota 能读取则仍保留，不返回本地替代值 |
| 无可用用量观察，至少一源查询成功 | `empty`，1；无数据行 `noData`，同时保留其他失败/离线及 quota 诊断，不填完整零值 |
| 至少一源有可用观察，但另一已纳入源失败/无数据，或任何行 partial/离线/过期/新鲜度未知，或 quota 读取失败 | `partial`，2；成功行和可用旧数据保留，失败行不隐去。quota 失败不抹掉可用 token 值；这是 A 待实施的新行为 |
| 所有选中来源覆盖完整、状态新鲜，quota 可用或正常无观察 | `complete`，0；`notObserved` 表示正常缺少 quota 观察，不等同读取失败。完整窗口有证据的零值可属于本行 |
| 被配置排除的来源 | 不纳入默认集合，也不因此造成 partial；显式请求该来源则单行 `failed`、1，原因 `source_excluded`，不改变配置 |

Health 使用诊断成功标准：可读取且没有健康降级为 `complete`/0；能输出诊断但存在来源、quota、recorder 或 service 错误/降级为 `partial`/2；无法建立可信请求为失败/1。没有 token 观察本身不使健康检查失败。此差异保留现有 Health 的职责，不能套用 Summary/Trends 的 `empty` 退出规则。

示例（合成且仅演示 CLI/状态，尚未实现）：`summary --all-sources --format json` 遇公共 100 + 分叉 20/30 时返回 `sources` 两行 120、130，`status=complete`、退出 0，不出现 150 或 250 的顶层总量；甲可用而乙读取失败时保留甲 120、乙 `report=null/error.code=source_read_failed`，`status=partial`、退出 2；相同输入若显式写 `--source all` 则在查询前退出 64。实际行 payload 由精确来源报告 DTO 的 schema 夹具冻结，不能用以上简写替代完整 JSON 黄金样例。

最小失败 envelope 草案示例（该合成 NodeId 未配置，退出 1；`error` 固定 `{code,message}` 或 `null`，`asOf` 使用 RFC 3339；这不是真实程序输出）：

```json
{
  "schema": "source-reports",
  "schemaVersion": 1,
  "reportKind": "summary",
  "asOf": "2026-09-26T00:00:00Z",
  "status": "failed",
  "sources": [{
    "sourceId": "node-11111111111111111111111111111111",
    "generation": null,
    "revision": null,
    "freshness": "unknown",
    "status": "failed",
    "report": null,
    "error": {"code": "source_not_found", "message": "Requested source is not configured"}
  }],
  "accountQuota": {"status": "notObserved", "report": null, "error": null},
  "diagnostics": [],
  "error": {"code": "all_sources_failed", "message": "No selected source could be read"}
}
```

**最后消费者删除矩阵（本次只核对，未删除）：**

| 符号/模块 | 当前生产消费者 | 中心/agent/共享 | 保留责任 | 最后消费者退役阶段 | 验证 |
| --- | --- | --- | --- | --- | --- |
| `logical_replica::detect_replica_candidates`、`history_query/reconciliation::plan_replica_resolution` | AllIncluded 投影、`remote_fact_sync` planner | 中心 | 精确来源投影、摘要和 lineage | A1 迁移全部前后台消费者且 A2 停 planner 后，A3 删除 | 复制/分叉场景；后台 Overview/Models 无旧归并调用 |
| `replica_fact_followup::{prepare,execute_prepared}_replica_fact_followup` | `cli` 手动同步、`automatic_remote_sync` | 中心 | 同步收尾与其 fence/预算/health/quota | A2 同时停两入口，A3 删除专用实现 | 两入口 transport spy 的 `SessionFacts` 计数为 0，收尾仍执行 |
| `remote_fact_sync::{plan_next_replica_fact_sync,sync_remote_thread_facts_bounded}` | 上述 follow-up | 中心 | 聚合页、游标、身份、预算和错误处理不得连删 | follow-up 无消费者后 A3；共享字段先拆分 | 聚合页失败不推进、重试幂等、generation 改变仍受 fence |
| `remote_agent` 的 `SessionFacts`、`remote_fact_exporter::prepare_remote_fact_page`、facts wire DTO | v5 服务端；本地物化函数另被中心 follow-up 使用 | agent / 中心共享 | `materialize_complete_session_facts_from_normalized_observation` 在 A2 后仍被 exporter 使用 | A4 协调协议边界后才删除最后一侧 | 新旧二进制配对、明确拒绝、无半成功/坏游标 |
| `source_history/session_evidence`、`source_export`、`remote_export_state` | facts ingest/export、reconciliation、摘要/聚合导出 | 共享 | 源身份、摘要、revision、quota、generation、游标和 fence | A3 仅中心专用部分，v5 部分等 A4；共享部分保留 | 按字段追踪剩余消费者，分页/恢复/ownership 回归 |
| `local_observation`、`remote_generation`、`finalize_remote_sync_attempt` | 本地 observation、聚合发布、手动/自动收尾 | 共享 | redo、COW/manifest、revision 不复用及安全提交 | 无 A 删除阶段；不计入 A 净删除量 | 原有观察提交/恢复、两类同步收尾继续通过 |

至少检查 facts planner、follow-up、同步摄取、exporter、reconciliation，以及 `source_history`、`source_export`、`remote_export_state`、`session_evidence` 中混合职责。同步编排和存储共享代码不能因名字包含 facts 就整块删除。

### A1：来源报告与消费者迁移

基于 M3 的内部边界实现来源报告集合。复用精确来源的计算，不按源重复 stage/flush 或 quota 合并。保留源 ID、revision、新鲜度、partial、价格不确定性和错误。

接入点是 [`prepare_report_history` / `PreparedReportHistory`](../src/history_application.rs) 与 [`HistoryQueryContext`](../src/history_query.rs)，不是恢复已删除的 monolithic 加载器。当前 `PreparedReportHistory::query` 和 `HistoryQueryResult::into_snapshot` 保留旧错误策略；当前 quota 失败也使 usage 返回错误。A 获批后才扩展可分别消费 quota/usage 的应用接口及回归，不可把循环调用旧适配器描述成已经支持部分成功。保留每次查询的 ownership/profile、policy/mapping/revision 校验、共享读预算和四次一致性尝试上限；多源 revision 是分别观察到的值，不是分布式同一时刻快照。

多来源默认按来源展示；首版按 A0 不提供合计。账户 GLOBAL quota 单独展示，不对百分比求和。比例与图表分母必须对应同一来源/时段。

更新 Overview、Models、Summary、Trends、Health、后台刷新、CLI/JSON 和所有实际消费者；旧 All 不能同名静默改为求和。显式来源不可用不回落本地，旧逻辑会话 ID 不能被误作新的来源会话 ID。

### A2：停止本中心 facts 补齐，但保留同步收尾

同时处理手动和自动同步后续、本中心本地 facts 物化。用 transport spy/调用计数证明本中心不再发起 `SessionFacts` 或额外事实扫描。

保留 `finalize_remote_sync_attempt` 等共享收尾的 config/host fence、预算结算、metadata、健康和 quota。不要通过提前 return、永久报错 transport 或关闭所有同步来模拟关闭 facts。

旧 v5 服务端 handler 及物化依赖暂保留，直到 A4 的协调协议切换。

### A3：删除已无生产消费者的中心逻辑

按清单退役中心归并、facts 规划/摄取及专用状态。先拆共享摘要/指标、类型和存储职责。移除对应生产路径及专属测试；保留支撑剩余契约的测试，不用测试占位器隐藏生产调用。

允许有结束时间的离线影子对照，不保留长期 A/C 双生产引擎来宣称瘦身。

### A4：协议、磁盘状态及旧进程协调迁移

**推荐协调切换，不支持跨产品语义的混合版本写入。** 以下均是待批准、待用旧二进制夹具验证的拒绝/恢复策略，不是已经实现的兼容保证。A1/A2 可分工准备，但不能在旧 All 消费者仍依赖 facts 时先启用 A2；A4 前旧 v5 服务端保持原契约，不能靠删 capability 猜测客户端不会请求。

| 场景 | 唯一推荐草案 / 拒绝点 | 所需验收证据 |
| --- | --- | --- |
| 新中心 + 旧 agent | A4 激活后在协议协商处拒绝该源，同步标版本不匹配；不请求 facts、不推进页游标、不覆盖已有源数据；A1 集合保留该源失败/旧数据诊断 | 固定旧 agent 二进制与 transport spy；失败前后游标/manifest 相等 |
| 旧中心 + 新 agent | 新边界在请求解码/版本协商时明确拒绝旧协议，先于任何导出状态写入；不返回看似成功的空页 | 旧中心夹具确认明确失败且无游标推进；新 agent 无新导出快照残留 |
| 同机旧 recorder/TUI/CLI 仍运行 | 拒绝激活。经明确授权停止旧写者，取得旧版本也会参与的原共同锁并重新验证进程/文件身份后才可准备迁移；任一旧写路径未受同一互斥约束或无法证明停机就不迁移。新标志/新锁/只查一次 PID 均不足 | 已支持旧二进制实际持锁/竞争夹具，迁移期间尝试旧写入；逐项覆盖 recorder/TUI/CLI，不实际停止用户服务来做本次材料 |
| 迁移中断/重启 | 先在隔离新目录准备并验证，旧目录保留一致快照；发布激活记录前失败则旧状态仍为权威，恢复时丢弃或幂等重建未发布副本；激活后只按已验证记录恢复新状态，不混读两个目录 | 准备、复制、校验、激活发布前后逐点中断；来源身份/桶/quota/游标/提交状态成组相等 |
| 新格式写入后启动旧二进制 | 不允许把旧二进制指向新目录；必须先证明既有旧格式校验会在写前拒绝，不能依赖旧程序看不懂的新 marker。证明不足则禁止原地切换，只允许隔离新目录且已验证部署入口不会误指；仍无法排除误写就拒绝激活 | 旧二进制显式指定新目录和按默认入口重启的夹具；失败后新目录字节不变。仅有迁移锁通过不算此项通过 |
| 旧 UI / 过滤器 / logical ID | 按 A0 一次迁移；All 显示 Sources 语义提示，精确来源保留，旧 logical ID 明确失效，无静默本地替代 | 保存状态黄金夹具、未知/排除来源、同名不同类型 ID；键盘/鼠标/compact 路径 |
| 备份与回滚 | 切换前停止写者并在旧共同锁内制作一致备份，包含历史、源身份、账户 quota、游标/manifest、ownership/profile、配置/映射/UI 版本；恢复到独立旧目录并验证后才重启旧版本。激活后新写入不自动反向转换；需回退时明确接受回到备份时点，新目录另行保留 | 备份完整性清单与 hash、旧二进制恢复演练；不得把可重建 facts 备份当成全部权威数据备份 |

实际协议版本号由仓库约定和变更情况决定，不在本文件预先猜定。仅在该边界统一退役旧 facts handler/exporter/wire DTO。

旧磁盘格式用独立读取 DTO 或明确迁移处理，不能直接删 `deny_unknown_fields` 状态中的字段便声称兼容。在对应锁及所有权机制下退役可重建派生数据，保留源身份、聚合桶、quota、游标和提交状态。真实用户历史的迁移执行需另有授权和可用备份。

### A5：退出条件

覆盖以下场景：独立会话、完整复制、公共 100 + 分叉 20/30、冲突/缺失/过期 facts、排除/离线/失败来源、父子会话与 subagent、多来源同账户 quota，以及升级、分页失败、重试和重启。

成功来源可保留，但集合不完整必须可见；全部来源失败、partial、无数据和 quota 失败按 A0 固定的规则处理。证明不再承诺唯一总量、事实补齐已退役、共享同步仍正确、兼容窗口有结束点，再记录实际净删除量与性能变化。

**不能通过过滤掉 delta journal 的旧变更、却继续推进游标实现所谓优化。** 请求时间范围不改变已承诺的增量消费与提交语义。

## 10. 条件路线 P：SQLite 有界原型

此路线只有收到明确的原型任务才启动；生产切换另行审批。R3 尚未确定时，只研究不依赖 facts 去留的本地观察契约，不迁移准备退役的数据。

**N4 任务书状态（2026-09-26）：**本节材料已补齐，原型尚未委托或运行。推荐先独立委托 P1；若希望有条件延伸到 P2，委托须明确写入 P2 范围和下述通过门。是否选择 A 不影响 P1 启动，P1 也不是 A 的前置条件。当前未新增数据库依赖、读取真实历史或选定版本；所有收益与原型平台结果均待测。

### P1：单次本地 observation

隔离原型覆盖一次 observation 的多数据族提交、同 revision 可见性、重启恢复和多进程一致读取。使用合成或明确授权的脱敏副本，既有 JSON 生产路径不受影响，不自动导入真实历史。

**可独立委托的任务范围：**从 [`source_history/local_observation.rs`](../src/source_history/local_observation.rs) 的既有契约与 failpoint 开始，写一套隔离的 local-observation SQLite 适配和对照驱动；不接入生产选择器、不新增长期可选后端。比较同一批 account quota、bucket、weekly、session digest 和 metadata 的提交/读取，不导入事件 facts、远端快照或生产 history-root。M3 的准备/查询边界可作为未来接入位置，本原型不改 CLI/TUI 行为，也不把替换 `PreparedReportHistory` 计作存储收益。

推荐原型方案以 `journal_mode=WAL`、`synchronous=FULL` 为起点，短事务发布全部数据族、独立持久化保留 revision 高水位，再用业务 fence 校验提交；这些配置本身不是满足旧持久性契约的证明。保留 revision 与数据发布是两个不同承诺：一次崩溃可以留下编号空洞，不能使已保留编号因数据事务回滚而重新发放。初始金额候选用 16 字节大端 BLOB 保存 `u128`，由 Rust 做受检精确聚合，排序/索引仍需证明；不把 SQLite INTEGER/REAL 或内置 `SUM` 当成 `u128` 的等价物。该方案只是原型比较起点，不是已证明可用的替代。

| 顺序 / 工作包 | 固定输入、工作与产物 | 可并行性与边界 |
| --- | --- | --- |
| P1.0 冻结对照 | 记录实际 HEAD、工具链、平台/文件系统、旧 JSON 配置；确定原型隔离目录及有界忙等待/取消阈值。用固定 seed `20260926` 合成 0、1、1,000、10,000 条记录档位，清单记各数据族数量、时间范围、hash 和磁盘尺寸；正常/空/partial observation、tombstone、重复和 profile/ownership 改变另列小夹具 | 不重复盘点全仓；不含 facts，A/C 未定也可独立完成。开跑前冻结资源上限与评价阈值，不跑到满意再选样本 |
| P1.1 依赖与文件安全 | 明确委托后才选择候选 `rusqlite` / `libsqlite3-sys`、features、链接方式，重新核对适用公告及实际 SQLite 版本；验证 DB/WAL/SHM/目录权限、对象替换和只读策略 | 可与合成夹具准备并行；依赖文件统一由主 agent 管理。缺少当前版本证据不得进入性能比较或声称安全 |
| P1.2 提交与恢复 | 实现 revision 保留和单 observation 原子发布；对保留后、事务中、提交前后、重启重试及所有权切换设置确定性中断；至少两个独立读者与一个写者进程用就绪握手竞争 | 等 P1.0/P1.1 通过；不把网络/长解析放进写事务，不通过禁用并发验证降低目标 |
| P1.3 数值/资源对照 | 验证 0、`2^63-1`、`2^63`、`2^64`、`u128::MAX` 及溢出；比较同一数据集/持久性配置下的新进程首次打开与已初始化重复打开、提交、查询、文件增长/写放大和构建成本 | 安全/原子性/精确性通过后再测。每个固定条件各 10 次，记录全部样本、中位数/最大值和失败数；未控制 OS 缓存不称真正冷缓存，不以小样本保证 p95 |
| P1.4 评价与移交 | 交付命令、输入/输出 hash、失败点记录、平台缺口、下表逐项结论及维护责任差额；给出采用候选/限定补证/不采用的一个结论 | 主 agent 统一复审；没有授权 P2 则在此结束，不自动扩到远端或真实数据 |

输出的不只是演示代码，还应逐项提供证据。以下硬门不能用性能收益抵销；“通过”指原型范围，不代表生产迁移通过。

| 项目 | 通过证据 | 停止 / 限定补证条件 |
| --- | --- | --- |
| 依赖版本 | 固定 Rust 包/features/链接来源；记录运行 `sqlite_version()`、编译选项、实际底层版本与核查日期，核对届时适用修复 | 只有包装库版本、旧网页结论或未知动态链接版本则停止；离线缺公告证据标未验证，不自动触发 CI |
| 文件安全 | DB/WAL/SHM、父目录、实际打开对象及恢复/只读路径满足既有威胁模型；路径替换、权限扩大可被拒绝 | 任一旁路可写或对象绑定无法证明则停止；未执行的平台列缺口，不能拿 Windows 结果冒充 Unix/macOS |
| revision / 原子性 | 已保留编号不复用；读者只能见同一已提交 revision 的全部数据族；空/partial observation 不能抹掉待恢复旧批次；profile/ownership 改变拒绝旧提交 | 任一混合 revision、旧持有者成功提交、重复编号或重试丢数据即停止，不能用自增行号替代契约 |
| 事务边界 | 解析在事务外，原子提交为短事务；忙等待和取消在 P1.0 固定上限内，竞争不破坏读取一致性 | 依赖无限重试、网络等待占写锁或永久串行化才能通过则停止 |
| 精确金额 | 所有边界值读写、索引排序、分组聚合及溢出拒绝一致；编码迁移无精度损失 | 使用 REAL、整数截断/饱和或只验证 round-trip 则不通过 |
| 持久性 | 记录 journal/synchronous/checkpoint 配置及 SQLite 对照的耐久目标；进程中断后按契约恢复，明确 fsync/平台差异 | 降低持久性才更快则不计收益；进程终止测试不能写成断电验证，断电实验不自动纳入 |
| WAL / 资源 | 长读者与 checkpoint 并发、增长预算、磁盘不足、中断及非默认 history-root 的拒绝/支持策略均有记录 | 超过预设增长/等待上限而无可执行收敛策略则停止扩围；不默认为网络文件系统安全 |
| 性能 / 构建 | 固定条件原始样本、峰值内存、磁盘/写放大、二进制与依赖构建增量；量不到的写未知；与相同持久性 JSON 基线比较 | 超过 P1.0 冻结的预算则不建议采用；若阈值或代表性未定，只能限定补证，不能事后挑指标宣布胜出 |
| 迁移草案 | 仅用合成旧格式说明导入校验、schema 版本、备份一致性、恢复到旧副本及不可反向转换边界 | 需要读取真实历史才能继续则停在数据授权门；不试迁移用户目录 |
| 净维护收益 | 列明数据库可替代的本地 redo/多族发布责任、仍保留的 revision/fence/权限/业务规则及新增依赖/迁移/运维测试成本；纯移动不算删除 | 若完整旧 redo/COW 仍需长期复制进数据库且新增责任不减，结论不采用；P1 不据此删除远端 generation/COW |

原审核曾记录 WAL-reset 修复边界 3.51.3（2026-03-13）及部分旧分支回补，见 [E4]；这是原审核引用的历史材料，**本次 N4 未联网核实，也不是任何候选版本的当前安全证书**。明确委托原型后才核对官方最新资料、实际链接版本及其他适用公告，不凭“高于该版本”宣布安全。

不要用降低持久性的配置制造性能优势。现有 revision 持久化保留规则不能简单由普通回滚事务替代；数据库文件的打开前路径检查也不能未经证明就等同于验证实际操作对象。

### P2：一个远端增量页

只有 P1 结果足以支持继续投入、**且明确委托范围包含 P2**，才验证一个远端页的 expected-active、页身份/指纹、数据族与游标共同发布、重复执行、来源 generation 改变及中断恢复。默认只扩展合成聚合页，不加入待 A/C 决定的 facts 存储；不启动真实 SSH 同步。验收固定为旧 expected-active 拒绝、重复页幂等、数据族与游标同成同败、generation 切换使旧提交失效，失败页不能过滤后继续推进游标。

不能把远端网络事务扩展成一个长期 SQLite 写事务；不能由本地事务成功直接删除所有 COW/generation。应说明数据库具体接管了哪些原子发布和恢复责任。

### 原型退出与停止条件

| 结论 | 必须满足 / 下一步 |
| --- | --- |
| 采用候选 | 硬门全部有证据、已测平台与预算达标、维护责任净减少；只建议另拟生产迁移方案，不改生产后端。P2 仍检查委托范围 |
| 继续限定验证 | 未出现硬门失败，但明确的一项证据/平台/预算缺口影响判断；列唯一补证范围、次数和停止上限，经范围授权后继续，不泛化扩围 |
| 不采用 | 安全/精确数值/revision/并发契约失败且无有界修复，或达到预设补证上限、平台/构建成本不可接受、无净维护收益；停止扩大，保留结论和失败证据 |

以上结论不依赖先选 A 或 C。A/C 决策、P1 委托、P2 扩围、生产切换及真实数据操作是分别记录的门；前一项通过不会自动打开后一项。R7 的 `tui-input` / `gix-url` 原型和 R8 的日志/通知/进程/下载库替换仍不纳入这份任务书。

原型通过后，另提交生产迁移方案及真实数据操作授权请求。**不保留一个无人维护的永久可选 SQLite 后端，也不在 SQLite 上长期原样复制全部旧 redo/COW 引擎。**

## 11. 本地实施记录

### 11.1 开工复核与清单

2026-09-26 开工 HEAD 为 `abef359eac136a2b004c7e4b98890cee27bd37fe`，与审查读取提交相同。原提案已经有用户未提交修订，两份配套文档未跟踪；保留这些内容，未回退至更早基线。D1—D8 已合入，M0 只补本地事实和清单。机器为 Windows 11 AMD64，Rust/Cargo 1.97.0，目标 `x86_64-pc-windows-msvc`；遵照 `.agent/environment.local.md` 及本次委托使用本机原生验证，不使用 Docker、UTM、远端或 GitHub CI。开工文档哈希及工具链证据保存于 `target/verification/refactoring-20260926/baseline.json`。

开工源码复核：手写 `compare_versions`、生产 `fs2` 与 launchd 字符串解析仍存在；`Zeroizing`、`windows-service` 和 `winreg` 已完成的迁移仍在，不重复实施。以下清单描述开工行为；重构后的边界与验证在后续记录中对应。

| 查询入口 / 消费者 | 准备及副作用 | 读取、错误和约束 |
| --- | --- | --- |
| `cli::run_summary/run_trends/run_health` → `collect_and_load_report_history_selected` | 采集在上游；建立 runtime、profile lease、恢复/迁移，重验证后 stage/flush；Summary backfill 是另一个显式写入操作 | local/remote/AllIncluded，保留报告 DTO、warning、partial 和退出码；明确 remote 不回退 local |
| `history_runtime::load_unified_history_since{,_selected,_with_staged_selected}` | 本身已是查询包装，初始化及恢复在 runtime 构造/准备入口；staged overlay 只在内存应用，不重复实施已存在的分离 | `history_query` 统一读取，ownership/profile/redaction 与源 revision 限制、项目映射、一致性重试及预算均须保留 |
| `history_query::load_v2_history_since_inner` | 不采集、不主动同步；读取 account quota 及同账户远端 quota，随后读取 token 与证据 | quota 不依赖 token selector；原 quota 错误会传播至整个查询；`AllIncluded` 保留 reconciliation；策略改变触发有界重试 |
| `TuiHistoryStore::load_since_with_staged_selected/reload_since_with_staged_selected` | deferred runtime 准备、显式 stage/flush 与读取交织；写前验证 profile lease | Summary/Trends/Health 与历史刷新；projection revision、staged revision 和过期时间控制缓存 |
| `TuiHistoryStore::load_remote_overview_history` | 读取源 metadata、active revision；接受已读 unified seed 或执行 AllIncluded 查询 | Overview/Models、后台完成及非当前页刷新均消费；失败保留既有文本诊断，缓存不得跨策略/项目映射复用 |
| `automatic_remote_sync` / CLI remote sync | 独立执行 transport、分页提交、facts follow-up 和 finalize | 不属于纯查询；本轮不截断同步或 facts，不变更预算及 v5 能力 |

A 候选清单仅供未来产品决策，以下符号本轮均保留：

| 符号 / 模块 | 实际生产消费者 | 属性与保留责任 | 最后消费者退役门 / 验证 |
| --- | --- | --- | --- |
| `logical_replica::detect_replica_candidates`、`history_query/reconciliation::plan_replica_resolution` | AllIncluded 查询投影；`remote_fact_sync` planner | 中心归并；共享摘要与精确来源投影不能一起删 | A1 消费者迁移后 A3；复制/分叉/partial 投影回归 |
| `replica_fact_followup::{prepare,execute_prepared}_replica_fact_followup` | `cli` 手动同步、`automatic_remote_sync` | 中心主动补齐；`finalize_remote_sync_attempt` 的 host/config fence、预算、health、quota 独立保留 | A2 停调用、A3 退役；transport spy 和收尾回归 |
| `remote_fact_sync::{plan_next_replica_fact_sync,sync_remote_thread_facts_bounded}` | `replica_fact_followup` | 中心 planner/摄取；分页和游标责任按字段核对 | A3；失败页不推进、重试幂等 |
| `remote_agent` 的 `SessionFacts` 分支、`remote_fact_exporter::prepare_remote_fact_page`、`remote_protocol` facts DTO | v5 服务端请求 handler；中心 follow-up 也使用本地物化函数 | agent / 中心共享，`materialize_complete_session_facts_from_normalized_observation` 尚有双侧消费者 | A4 协调协议切换；新旧中心/agent 明确拒绝或兼容 |
| `source_history/session_evidence`、`source_export`、`remote_export_state` | exporter、facts ingest、reconciliation、聚合导出和 revision fence | 混合职责；摘要、来源身份、quota、generation、游标与 fence 不能按文件删除 | A3 先拆职责，v5 部分等 A4；分页/恢复/ownership 回归 |
| `source_history/remote_generation`、`local_observation`、`remote_sync_attempt::finalize_remote_sync_attempt` | 聚合页发布、本地观察提交、手动/自动同步 | 共享保留；COW/manifest、redo、revision 不复用及 quota 非 A 专用 | 不在 A 默认删除量内；须独立替代证据 |

同机旧 recorder/TUI/CLI、迁移中断、新格式后旧二进制、备份回滚的场景草案继续采用第 9 节 A4 矩阵；本轮不选择迁移机制，也不声称旧进程可被新标志阻断。

### 11.2 实施与验证结果

M5 契约分类与处置：

| 类别 / 调用点 | 本次处置 | 仍由调用者保留的责任 |
| --- | --- | --- |
| `PrivateStoreLayout::write_atomically`、`project_mapping::write_mappings_atomically`、`remotes_config::write_private_atomically`、`source_identity::write_private_atomically`、`history_ownership::write_private_atomically`、`history_profile_lease::write_active_profile` | 共用 `private_state_store::publish_private_replacement`；统一 write → sync_all → close → replace → 发布后校验 → 目录同步及失败清理 | 各自的输入上限、目录验证、安全分配、ACL/no-follow/实际对象验证、临时文件名和发布后校验；发布后校验失败仍返回错误，不虚构已回滚 |
| `cache` 的可丢弃缓存/写探针 | 保留；缓存写入仅 flush，写探针 sync_all 后删除，与持久状态发布契约不同 | 缓存目录/权限和清理；本轮不强化或降低原持久性策略 |
| `history` / `source_history` 的流式 JSON、`.target.pid.sequence.tmp` 恢复文件、local observation redo、remote generation/manifest | 保留 | 尺寸预算、恢复命名、revision、提交顺序、分页游标、故障恢复 |
| ownership 的 hard-link create-once、project mapping 的 prepared 发布 | 保留 | 不覆盖已有目标、prepared 生命周期和协调提交 |
| `update` / 安装升级候选 | 保留 | 二进制身份、receipt、rollback 及原生替换契约 |

不增加 `tempfile` 生产依赖。抽出的目录同步沿用旧实现：Unix 同步目录句柄，Windows 由既有 `atomic_file::replace_file` 的原生替换负责；没有新增断电持久性承诺。

| 项目 | 实施状态 | 结果/证据 |
| --- | --- | --- |
| 开工 HEAD / 工具链 / 工作区 | 已核对 | M0—M6 收尾验证时 HEAD 为 `abef359eac136a2b004c7e4b98890cee27bd37fe`，当时改动未提交；后续本地提交见第 11.5 节。保留用户原文档修改；无 push/tag/部署/CI |
| M0：复核与提案修订 | 完成 | 第 11.1 节清单、原提案复核记录；已有 D1—D8 和 Windows 迁移不重复实施 |
| M1：semver | 完成 | `update.rs` 及测试；完整解析 + precedence，保留 build 冲突/降级保护；4 个目标测试通过；CHANGELOG 记录非法版本拒绝 |
| M2：标准库锁 | 完成实现及 Windows 目标验证 | 19 个锁模块迁移，删除 11 个 contention helper；保留 guard/unlock/identity；新旧跨进程双向共享/独占、释放和真实 IO 错误回归通过；Unix 原生待验证 |
| M3：应用边界 | 完成实现及目标验证 | 新 `history_application.rs`，CLI、TUI、history runtime/query 接入；8 个新增目标测试通过；保留 AllIncluded、v5/facts、报告 schema/退出码及 quota 可见错误 |
| M4：XML 解析 | 完成实现及纯解析验证 | 新 `service/launchd_plist.rs` 及黄金 fixture；两个生产入口各解析一次；旧 parser 黄金 1 项、最终解析 10 项通过；未执行原生 launchd |
| M5：临时文件归并 | 完成限定范围 | 6 处共享持久化替换；与 M2 合并目标验证 11 项通过（其中 1 项为进程 helper 的空入口，父用例实际执行双向握手）；未改高风险保留路径 |
| M6：组合验证与复审 | 完成 | 独立交叉复审、失败诊断和最终影响范围复验完成；组合证据覆盖 1,779 项库测试与 128 项集成测试，保留 1 项真实历史手动基准未执行；结果、哈希、命令和限制见下文 |
| A 产品路线 | 决策门关闭，未执行 | 只盘点候选消费者，不改 All，不删 facts/v5，不操作真实状态 |
| SQLite 原型 | 决策门关闭，未执行 | 未新增 rusqlite/SQLite 或实验后端 |
| SQLite 生产迁移 | 决策门关闭，未执行 | 无 schema/存储迁移、真实数据导入或破坏性清理 |

M3 的具体边界与验收：

- `prepare_report_history` 独立执行 runtime 准备、stage/flush，写前再次验证 profile lease；`PreparedReportHistory::query` 不再提交。旧 CLI monolithic 生产入口删除，仅保留测试便捷适配。
- `stage_runtime_collection` 收敛 CLI、TUI 和 backfill 的三处 digest/stage 降级规则；各自启动 cutover 与可见警告策略仍保留。`query_runtime_history` 和 `history_projection_revision` 被 CLI/TUI 实际消费，不是无人使用的备用 API。
- request context 共享 quota 与 `SourceHistoryReadBudget`，每次读取仍验证 ownership、源策略、映射和相关 revision，最多 4 次一致性尝试。quota 原始错误和 usage 错误内部区分；兼容适配保留原 quota 失败传播及具体来源不回退。revision 探测失败只禁缓存，不新增远端查询失败。
- `prepared_report_` 两项覆盖准备一次、无重复 revision 提交、共享 quota、远端不可用与本地 revision 损坏；`request_context_` 四项覆盖源策略/映射失效、quota/usage 错误、跨重试预算和四次上限；`application_refresh_` 一项覆盖真实 TUI 精确来源加后台 Overview 的 quota 共享、源策略/本地 revision 缓存失效及 panic 后清理。
- 冷启动复验另增加 `remote_overview_seed_without_remote_sources_preserves_data_and_invalidates_old_cache`：无纳入的 SSH 来源且已有统一查询 seed 时，直接 `from_unified` 并清空 Overview 缓存，保留统一桶、warning/partial，不再读取仅服务于缓存的 revision；无 seed 或有远端仍走原查询和 fence。回归覆盖完整结果相等、旧缓存清理、无 seed 重新查询及新增远端失效。
- 既有只读/profile lease、redaction、pending observation、replica/facts、Summary/Trends/Health JSON、鼠标/键盘/compact 回归由最终本平台组合套件统一执行。没有新建多来源集合 schema，也不承诺远端同物理时刻快照。

### 11.3 验证证据与复审

所有本地日志位于 `target/verification/refactoring-20260926/`（Git 忽略）。原生平台为 Windows 11 AMD64 / `x86_64-pc-windows-msvc`，Rust 1.97.0；当时验证对象是未提交的工作区。组合 runner 的 JSON 保存完整文件哈希、命令、时间、退出码及测试前后是否变化；文档修改不计入 Rust/测试/脚本输入快照。后续提交与该快照的对应关系见第 11.5 节，原始日志不改写为提交后重跑的证据。

| 实际命令 / 检查 | 结果 | 日志 |
| --- | --- | --- |
| `cargo test --locked --offline --lib update::tests::semantic_` | 4 passed | `m1-focused.log` |
| `cargo test --locked --lib service::tests::launchd_plist_generation_matches_golden_bytes_and_fingerprint -- --exact` | 旧 parser 上 1 passed | `m4-golden-before-parser.log` |
| `cargo test --locked --lib launchd_plist` | 最终 10 passed | `m4-launchd-plist-final.log`、`m4-source-hashes.txt` |
| `cargo check --locked --offline` | 通过；后续以最终 Clippy/全套证据为准 | agent 编译输出 |
| `cargo test --locked --offline --lib request_context_ -- --test-threads=1` | 4 passed | agent 工具会话 36757；最终全套另含这些测试 |
| `cargo test --locked --offline --lib prepared_report_ -- --test-threads=1` | 2 passed | `m3-prepared-report.log` |
| `cargo test --locked --offline --lib application_refresh_ -- --test-threads=1` | 1 passed | `m3-tui-refresh.log` |
| `cargo test --locked --offline --lib -- file_lock::tests private_state_store::tests --test-threads=1` | 11 passed；源码前后未变 | `m2-m5-focused.log`、`m2-m5-focused.json` |
| `cargo fmt --all -- --check` | 通过 | `format-preflight.log` |
| `cargo tree --locked --offline --target x86_64-pc-windows-msvc -e normal -i fs2` | 无生产依赖路径 | `dependencies-fs2-production.log` |
| `pwsh -NoProfile -File scripts/windows/verify.ps1 -CargoTargetDir D:\Workspace\codex-usage-monit\target\refactoring-final -CargoBuildDir D:\Workspace\codex-usage-monit\target\refactoring-final-build` | fmt、Clippy `--all-targets -- -D warnings`、25 项 Python 契约、60 项 verify-smoke 和 17 项 dev-smoke 通过；沙箱权限契约处停止，未到 Rust 全套 | `windows-final.log/json` |
| `pwsh -NoProfile -File scripts/windows/tests/repair-state-permissions.ps1`；`powershell -NoProfile -File scripts/windows/tests/repair-state-permissions.ps1` | 正常宿主账户分别 10 passed，PowerShell 7.4.1 / 5.1.26100.9444；源码前后未变 | `permission-repair-host-pwsh7.log/json`、`permission-repair-host-powershell51.log/json` |
| 同一 `verify.ps1` 及两个目录参数，加 `-SkipFormat -SkipClippy -SkipSmoke` | 首次库全套 1,769 passed / 9 failed / 0 ignored；完整命令保存在 JSON | `windows-rust-final.log/json` |
| `cargo test --locked --offline --lib --` 后跟上述 9 项失败过滤词及 `--test-threads=1`（完整参数见 JSON） | 安全仓库外 TEMP、正常宿主账户下 9 passed；未重跑其余 1,769 项 | `windows-failed-focused-host.log/json` |
| `cargo test --locked --offline --bin codex-usage-monit` 及全部 10 个显式 `--test` 目标，`--test-threads=4` | bin 自身无测试；agent_management 首次 1 passed / 1 failed，后续目标尚未启动；失败项换短 TEMP 定向补验 1 passed | `windows-integration-final.log/json`、`windows-agent-short-temp.log/json` |
| `cargo test --locked --offline --no-fail-fast` 及剩余 9 个 `--test` 目标，`--test-threads=4` | 首次 124 passed / 1 failed / 2 ignored；ConPTY 初始数据等待超时，后续修正及复验见下文；全部目标名与参数保存在 JSON | `windows-integration-remaining.log/json` |
| 同一 `verify.ps1` 及两个目录参数，加 `-SkipFormat -SkipClippy -SkipTests` | 原生 CLI build、version、offline JSON/partial/任务非空冒烟通过 | `windows-cli-smoke.log/json` |
| `cargo test --locked --offline --lib -- tui::tests::remote_overview_seed_without_remote_sources_preserves_data_and_invalidates_old_cache tui::tests::application_refresh_shares_quota_with_background_overview_and_invalidates_quota_policy --exact --test-threads=1` | 最终窄修正的两项回归 passed；首轮短名配 `--exact` 的 0 tests 不作为证据 | `m3-overview-empty-seed-final.log` |
| `cargo test --locked --offline --test tui_pty --test tui_data_integration -- --test-threads=1` | 最终快照 4 passed，含两个真实 ConPTY 用例；正式 8 秒阶段期限未改 | `windows-tui-integration-final.log/json` |
| `cargo test --locked --offline --lib tui::tests -- --test-threads=4` | 最终 TUI 模块 339 passed | `windows-tui-unit-final.log/json` |
| `cargo clippy --locked --offline --all-targets -- -D warnings`；`cargo fmt --all -- --check` | 最终快照均通过 | `windows-clippy-final.log/json`；格式检查工具输出为空且退出 0 |
| `cargo test --locked --offline --test update_cli different_build_running_portable_launcher_keeps_proxy_protocol -- --exact --ignored --test-threads=1` | 最终 1 passed；使用独立 HEAD 构建作为 `CODEX_USAGE_MONIT_PREVIOUS_TEST_BINARY`，测试确认两个实际 buildId 不同 | `windows-previous-build-compat.log/json` |

测试环境修正与失败分类：首次未限定 `--lib` 的构建遇到旧 `target/debug/deps/codex_usage_monit.exe` 占用而 LNK1104，未终止用户进程，库目标验证及最终独立 target/build 目录避开该冲突。首次 `lock` 筛选的 97 项中，多项因默认 `%TEMP%` 继承另一沙箱 SID 而触发私有目录拒绝；停止该次运行，未放宽产品安全规则。后续 `TEMP` / `TMP` 均指向工作区本次证据目录下的 `temp`，其 DACL 仅允许当前测试账户、SYSTEM 和 Administrators。新增 NUL 句柄测试的错误假设也已纠正为 attributes-only 句柄，真实 IO 错误回归已通过。以上失败保留在原日志，不计为通过证据。

组合入口中的 ACL 修复契约在受限账户 `CodexSandboxOffline` 缺少 `SeSecurityPrivilege` 时失败；两份脚本的 Git blob 与 HEAD 一致，PowerShell 7/5.1 均独立复现，确认与本轮源码改动无关。随后仅将该独立契约放到正常宿主账户执行，使用新的合成临时目录，两种 shell 均 10 项通过；没有授予特权、修改产品 ACL 规则或操作真实 monitor 状态。5.1 子进程另指定原生 `PSModulePath=C:\WINDOWS\System32\WindowsPowerShell\v1.0\Modules`，避开本机混入 PS7 模块的问题。保留全部失败及补验日志，不将首次组合入口写成全绿。

库全套的 9 项失败已逐项分类：1 项非仓库识别受 TEMP 位于 Git 工作区影响；4 项安装注册表测试在创建随机隔离 HKCU fixture 键时被沙箱拒绝（OS 5）；1 项 trace 测试创建符号链接 fixture 时缺少特权（OS 1314）；2 项真实 Git 证据测试与 1 项 combined snapshot 测试在高并发运行中分别出现证据未取得和 5 秒握手超时。相同源码使用仓库外安全 TEMP、正常宿主账户及单线程定向补跑 9 项全部通过。后三项仍记为负载敏感风险，未调高时限、删除测试或认定性能无退化。

集成安装用例的首次失败发生在 PowerShell 5.1 执行过长的合成安装路径，直接执行此前已成功；换到短私有 TEMP `C:\Users\Ghost\AppData\Local\Temp\cm-refactor-20260926` 后同一测试通过。未修改真实 PATH、ARP 或服务。首次集成套件保留两项原有忽略门槛；随后利用独立 HEAD 对照产物，补跑 `different_build_running_portable_launcher_keeps_proxy_protocol` 并通过。最终仅 `benchmark_real_codex_cache` 因需要真实历史而未执行。

ConPTY 冷启动失败没有按环境问题直接略过：正式测试在串行复验中仍超时。隔离 `git archive HEAD` 的原测试通过（harness 7.38 秒）；当前实现的临时诊断副本在 30 秒诊断窗口内通过，trace 显示 source-aware 数据就绪约 7.5 秒。该副本不是正式通过证据，也未修改正式 8 秒期限。另一次相对 `--codex-home` 的 headless 诊断触发既有只读 fallback，其约 101 毫秒结果已排除；绝对路径复现 V2 冷启动约 7.43 秒。发现 Overview seed 路径为未使用的缓存增加探测后，实施第 11.2 节的窄修正并独立复审；最终正式 ConPTY 两项及数据集成两项均通过。对照日志为 `head-baseline-conpty.*`，诊断日志为 `windows-conpty-diagnostic.*`、`conpty-diagnostic-*`、`startup-absolute-diagnostic-*`。单样本、路径和冷加载差异限制了性能推断，不声称一般性能增益；Windows 冷启动期限余量仍是风险。

完整库/早期集成验证对应源码快照 `6aa474156a3307ab2eb45654f4a623a4ea7cba94c9ce31a2cb7549c3f451aa98`。最终快照为 `24453cdcc122ebbba85ed4c484ee687ff5b9e3d2a47970845b280636f00c7544`，只有 `src/tui.rs` 和 `src/tui/tests.rs` 改变；`source-delta-final.json` 已核对这两个文件的差集。按影响范围补跑 TUI 模块、ConPTY/数据集成、格式和 Clippy，其余模块沿用未改变文件的证据，不把早期全套称为最终源码的一次全绿运行。

独立复审与处理：版本/XML、锁/发布、应用层分别交叉复审；主 agent 统一依赖和组合验证。发现的本地 revision 探测扩大远端失败范围已改为禁缓存，并用损坏本地 revision 的真实 fixture 验证；错误复制会丢失 typed cause 的风险已改为兼容适配直接返回原始 quota 错误；Overview 的 ownership/映射/local/quota revision 失效与 panic 后请求上下文清理均补入回归。没有删除仍有生产消费者的 v5/facts、共享摘要、subagent、quota 或测试来减少失败。

### 11.4 代码、依赖及剩余边界

`semver = "1.0.28"` 成为直接生产依赖（锁定版本原已存在，默认仅 `std` feature，MSRV 1.68，MIT OR Apache-2.0）；`fs2 0.4.3` 移至开发依赖，仅供混合版本测试。`quick-xml 0.41.0` 不变，`tempfile` 保持开发依赖，工具链仍为 1.97.0。锁定文件只新增根包的 semver 依赖边，无包版本/checksum 升级或新包。已核对 [semver 锁定版本清单](https://docs.rs/crate/semver/1.0.28/source/Cargo.toml) 和 [precedence API](https://docs.rs/semver/1.0.28/semver/struct.Version.html#method.cmp_precedence)；本机无 `cargo-audit`，未执行完整最新公告审计，不将网页检索当作审计通过。

源码行数采用本次 `code_stats.py` / `code-stats.json` 的相同静态口径：`tests.rs`、tests 子目录及尾部 `#[cfg(test)] mod tests` 算测试区，其余算生产区（因此零散 cfg(test) helper 可能计入生产区）；单独统计空行及整行 `//` 注释，不声称编译器级分类。当前净变化为生产区 **+479** 物理行（非空/非整行注释 +419、注释 +41、空行 +19），测试区 **+1,143**（对应 +1,083/+12/+48）。新增应用模块中的约 600 行来自原 CLI 编排与 TUI revision 逻辑迁入，这部分纯移动不计维护收益。具体净收益是删除手写版本解析、11 套锁竞争分类、重复的 stage 降级和 6 处发布顺序实现；XML 增加结构检查与边界测试，未取得净减行收益。

除上述用于定位超时的单一 fixture 冷启动记录外，运行延迟、I/O、内存、二进制尺寸、构建成本和性能收益均**未做受控基准对照**；新 revision 探测及 account DTO clone 也有成本。未执行 Linux/Unix 与 macOS 原生锁/发布验证、真实 launchd/SCM 服务安装、断电持久性验证或真实用户历史基准；本次仅执行 Windows 本机验证，按委托不使用 Docker、不自动触发 CI/发布部署。进程测试不能替代断电证明；macOS XML 纯解析不能替代原生 launchd。以上跨平台及测量缺口保留，不作为已通过声明。

实施记录简洁保留实际状态、剩余风险和测试证据即可，不因每个子任务再创建一套重复文档。源代码事实与技术依据见配套审核文件的固定提交链接。

### 11.5 本地提交与验证快照对应

2026-09-26，收到“先提交，然后按文档规划下一阶段任务”的委托后，先在原 `main` 分支完成以下 6 个本地提交，再开始第 12 节规划：

| 提交 | 范围 |
| --- | --- |
| `90c38c7` | M1：完整 semver 解析、precedence 与升级身份回归；直接依赖边 |
| `546421f` | M3：应用准备/查询上下文、CLI/TUI 接入、缓存失效及冷启动窄修正 |
| `887db9d` | M4：有界 launchd XML 解析及黄金样本 |
| `23d3527` | M2：生产标准库文件锁、混合版本互操作、fs2 移至开发依赖 |
| `ee84f31` | M5：6 处同契约私有文件发布归并及失败回归 |
| `0656a20` | M0/M6 文档、审核原文、提案修订、实施证据和 CHANGELOG |

规划基线为 `0656a201edbb5517a9f3ecac78549cbc6d8a15c0`，该提交后工作区干净。M2/M5 的重叠文件仅拆分暂存内容，未改动已验证的最终工作区源码；中间提交边界做静态及格式复核，没有逐提交编译或重复执行全量测试。最终源码快照仍为第 11.3 节的 `24453cdcc122ebbba85ed4c484ee687ff5b9e3d2a47970845b280636f00c7544`。审核原文件字节保持不变；其顶部双空格 Markdown 换行保留，不作为代码空白缺陷删除。完整 Git SHA、快照及提交核对记录保存在同一证据目录的 `commit-planning-verification.json`。

本轮提交及后续规划只做源码一致性、文档链接、指令与 diff 检查，沿用第 11.3 节的组合测试证据，不宣称另有一次完整套件全绿。所有提交仅在本机，未 push、创建 tag、发布、部署或触发 GitHub CI。

## 12. 下一阶段任务规划（实施结果见第 13 节）

### 12.1 建议顺序与范围

本节是在 `62f6a3a` 提交的实施前规划；收到“按规划的继续”后，实际执行情况另记第 13 节。M0—M6 已完成，不再重做。下一阶段先推进 **N1—N3：Windows 验证稳定性、受控测量和依赖审计**；N4 的产品/架构决策材料独立准备。N5 是有条件的平台补证，当前 Windows 机器缺少对应原生环境时保持未执行，不阻塞前四项，也不改变第 9、10 节的决策门。以下保留原定范围和验收标准。

```text
0656a20 已提交基线 + 已有验证证据
  ├─ N1：Windows 夹具/时序失败诊断与定向修复
  ├─ N2：冷启动、查询与资源受控测量 → 有证据再做窄优化
  ├─ N3：锁定依赖的本地公告审计
  └─ N4：A/C 决策材料；SQLite 原型委托草案（可并行）
N1 的环境修正稳定后 → N2 正式采样
N1—N3 稳定 → 主 agent 一次组合验收与记录
具备并获准使用相应原生环境时 → N5 平台补证
A 明确选择及兼容策略批准后 → A0—A5
SQLite 原型明确委托后 → P1 → 结果支持且委托包含 P2 时继续 → 单独生产迁移决定
```

### 12.2 N1：Windows 验证稳定性（优先）

**问题依据：** 第 11.3 节已有 TEMP/ACL/执行账户、PowerShell 5.1 路径长度导致的夹具失败；两个 Git evidence 用例和 combined snapshot 握手在负载下失败，定向串行补验通过。后者尚不能定性为产品缺陷或纯环境噪声。

**工作与所有权：** 一名 agent 负责 Windows 验证入口、相关脚本契约与测试夹具；先复现再决定是否修改 `scripts/windows/verify.ps1`、`scripts/windows/tests/`、`tests/agent_management.rs`、`src/git_repository.rs`、`src/source_export.rs` 与 `src/source_history/remote_generation.rs` 中相关测试。先稳定环境，再定位 Git 子进程预算及快照 reader/publisher 握手阶段；区分夹具准备和被测竞争的成本。复用已有夹具，不建设新测试调度系统。明确短且位于仓库外的私有 TEMP、实际账户与 DACL、两种 PowerShell 的模块环境及独立 target/build 目录；这些是测试设置，不放宽产品 ACL 或自动给账户授予特权。

**验收：** 每个失败有原始日志、根因分类、改前复现和改后证据；时序用例优先显式就绪/继续握手与受控描述符，保留有界失败。相关用例在串行与受控并发下各进行固定批次的重复验证（初始各 10 次，开跑前固定上限），记录全部结果而非只保留成功轮次。涉及 PowerShell 时跑 5.1/7 外层调用契约；缺少权限的检查单独报告并在合适账户补证。单纯延长超时、永久串行化或多跑到绿不算解决。若证据表明是产品问题，先加确定性回归，并由主 agent 审核扩大后的相关代码范围。

### 12.3 N2：启动与查询成本测量（优先）

**问题依据：** 正式 ConPTY 最终通过，但修正前诊断样本约 7.5 秒、接近既有 8 秒阶段期限；该值不能代表修正后的耗时，尚无受控对照证明一般性能变化。M3 新增 revision 探测、quota DTO 复制及缓存检查的成本未量化。

**工作与所有权：** 另一名 agent 负责 `tests/tui_pty.rs`、`tests/tui_data_integration.rs` 的合成样本/测量，以及有证据需要修改的 `src/history_application.rs`、`src/history_query.rs`、`src/tui.rs` 和 `src/tui/tests.rs`。以隔离快照比较原 `abef359` 与已提交基线，固定工具链、构建 profile、绝对路径、数据、来源数量和执行账户；不 reset 当前 checkout、不读取真实历史。区分全新 V2 与已初始化状态，并确认没有因相对路径触发 legacy fallback。覆盖无远端、多个合成来源、缓存命中/失效、quota 变化及后台非当前页。保留正式 8 秒期限，临时诊断窗口单列。

**验收：** 先记录采样协议和次数，再测；进程首次启动与同环境重复启动分开，未控制系统文件缓存时不称真正冷缓存。建议每种条件至少 10 次，报告原始样本、样本中位数/尾部/最大值、超时数与运行负载；小样本不作稳定 p95 保证。比较准备/stage/flush、quota 合并和 revision 探测次数，记录可取得的 I/O、峰值内存、磁盘/二进制尺寸和构建成本，无法测量的明确留空。只针对已定位的重复工作优化，保留 ownership/profile、预算、有界重试与错误契约；不得缓存越过权限或 revision。优化后受影响测试及正式 ConPTY 通过，完整展示前后数据；没有足够差异证据时只交测量结论，不制造性能承诺。

### 12.4 N3：依赖公告审计（可与代码工作并行）

由主 agent 管理 `Cargo.toml` / `Cargo.lock`，先核对本地工具可用性，按仓库 `.github/workflows/dependency-audit.yml` 当前固定的 `cargo-audit 0.22.2` 和 `cargo audit --deny warnings` 做本地审计；执行时重新核对仓库要求，不把本文写成永久最新版本依据。记录工具版本、公告数据库版本/更新时间、完整命令、锁定文件哈希和结果。工具或网络不可用时如实记录缺口，不以网页检索替代审计，不因此触发 GitHub CI。

退出条件是每条发现都有依赖路径、生产/开发归属、适用性与处理决定；若需要修复依赖，只调整有证据的最小范围并做影响测试，不批量升级、不默默忽略 warning。审计完成不代表 A 或 SQLite 获批；本阶段不添加候选数据库或其他尚未选用的库。

### 12.5 N4：产品与架构决策材料（文档工作，可独立推进）

| 材料 | 具体交付与验收 | 决策门 |
| --- | --- | --- |
| A/C 产品取舍及 A0 草案 | 复用第 11.1 节消费者清单，只增量复核新 HEAD。用独立、完整复制、公共 100 + 分叉 20/30、facts 缺失/冲突和多来源同账户 quota 的合成例子列出 A/C 差异；记录放弃全局唯一总量的价值取舍。列明 CLI 默认、旧 `--source all`、集合 schema、UI 保存状态/逻辑 ID、partial/错误/退出码与回滚选项，不能以待实施时决定代替。缺真实使用证据时明确未知，不增加遥测或读取用户历史。 | 交付草案不等于选择 A。用户明确选择 A 并批准兼容规则后才能冻结 A0、改产品或删 facts；若选择保留 C，结束删除路线，不强推瘦身。 |
| A4 兼容/删除依赖草案 | 为新旧中心/agent、同机旧 recorder/TUI/CLI、中断重启、旧二进制重开新状态和备份恢复逐格提出支持或安全拒绝策略。把最后消费者与 A1—A4 对应，明确哪些中心逻辑可先退役、哪些 v5/shared 职责必须保留。验证方式具体到 spy、旧二进制夹具或恢复场景；不猜协议版本、不实际迁移数据。 | 延续第 9 节 A4 独立兼容边界；真实用户历史操作仍需明确授权及可用备份。 |
| SQLite 原型任务书 | 复用第 10 节门槛，明确 P1 的一次本地 observation、合成数据集、成功/停止条件、依赖与安全核对、同等持久性比较及可删除机制；P2 只候选一个远端增量页。A/C 未定时排除依赖 facts 去留的数据。 | 只准备任务书，不添加 rusqlite、不建原型后端。明确委托后才执行 P1；P1 结果支持且委托范围包含 P2 时才继续；生产切换、真实数据导入另行决定。 |

A 的产品决策和 SQLite 的本地原型委托可以独立发生；不得要求先采用 SQLite 才选 A，也不得把批准 A 当作批准 SQLite。N4 不重复撰写整套架构文档，结果增补到第 9、10 节相关决策记录即可。

A 获批后的实施仍按 A0 → A1/A2 → A3 → A4 → A5 控制依赖。A1 消费者迁移与 A2 停止 facts 的改动可以分工准备，但在旧 All 消费者仍依赖 facts 时不能单独启用 A2；共享 `cli.rs` 接入由主 agent 统一。复用 M3 的准备一次/多次查询和共享 quota，不再循环完整报告准备入口；v5 的 handler/exporter/wire DTO 等到 A4 才统一退役。

### 12.6 N5：平台补证及继续暂缓项

具备相应原生环境并获准使用后，补 Unix 新旧锁互操作、复制句柄/独立读者释放、私有文件替换与目录同步；macOS 补真实 launchd 接受/读取生成 plist 的行为。真实服务安装/卸载只在明确授权的隔离环境执行。记录平台/架构、精确源码、命令、结果和日志；交叉编译、纯 XML 测试不能顶替原生运行。当前 Windows 本机继续原生验证，不运行 Docker；没有环境就保留缺口，不自动访问远端或触发 CI。

R7 的 `tui-input`/`gix-url` 原型、R8 日志/通知/进程等库替换、唯一 daemon/IPC、真实历史基准和断电持久性实验仍不排入默认下一阶段。今后有具体需求和可验证收益再单列；未执行不写成验证通过或既有缺陷。

### 12.7 并行与下一次验收

主 agent 维护基线/证据、依赖及文档；N1、N2 分属两个不重叠文件集合，N4 可由第三名 agent 只做材料。N2 的正式测量等待 N1 环境配置稳定，期间可准备合成场景；测量与重测试不要并发争抢机器。若根因跨越文件所有权，先交主 agent 协调再修改，禁止双方同时编辑公共接口或依赖文件。

各包在迭代中只跑目标测试；主 agent 在源码稳定后统一跑一次相关 Windows 完整入口与组合复审。沿用未变源码的既有证据，新增修复按影响补验；只改规划文档不跑 Rust 全量。按第 11 节格式追加实际完成/未完成项、完整命令、源码身份、失败分类、净成本及尚未打开的门。提交、push、CI、发布分别遵照当时委托；规划提交时仅有本地提交与规划授权，后续实施委托与结果见第 13 节，始终没有自动执行外部操作的授权。

## 13. N1—N4 实施记录

### 13.1 本轮基线与范围

收到“按规划的继续”后，以 `62f6a3ab2a787759867379071ffd5fa1d00b4ba6` 的干净工作区开工，源码快照仍为 `24453cdcc122ebbba85ed4c484ee687ff5b9e3d2a47970845b280636f00c7544`。先重新阅读仓库指令、本机环境说明和测试流程，沿用 Windows 11 AMD64、Rust 1.97.0 原生验证。新证据位于 `target/verification/refactoring-next-20260926/`；第 11 节的历史日志及审核原文字节保持不变。

N1、N2 与 N4 分属三个 agent 的不重叠文件集合，主 agent 负责 N3、接口协调、组合验证和本记录。N2 正式采样避开构建与重测试；独立复审只读取文件。没有重做 M0—M6，也没有启动 A 产品切换、facts 删除、SQLite 原型或生产迁移。

### 13.2 N1：Windows 环境与确定性回归

`scripts/windows/verify.ps1` 新增 `-TestTempDir`，在测试/冒烟前打印实际账户、TEMP 与模块环境，拒绝不存在的目录、Git 工作区内目录及会让夹具继承其他身份访问权的目录。该检查只创建并检查临时探针，不修复 ACL、不授予特权。TEMP/TMP/PSModulePath 在成功和失败时恢复；Windows PowerShell 5.1 使用本引擎模块目录。另修复了 5.1 参数默认表达式中的 `PSScriptRoot` 问题：默认仓库路径改在脚本正文求值。安装集成夹具明确诊断 5.1 的 260 UTF-16 路径上限。

本次正常宿主账户使用仓库外短目录 `C:\Users\Ghost\AppData\Local\Temp\cm-next-20260926`；其 DACL 仅允许该账户、SYSTEM 和 Administrators，见 `n1/host-temp.json`。受限沙箱与正常账户的权限不同，未把正常账户通过写成沙箱也通过。

- 非仓库缓存回归使用受控 Git exit 128，并确认真实夹具祖先没有 `.git`。两个 materialization 回归仍执行真实 Git 初始化/发现/配置，随后重放捕获字节验证脱敏和共享缓存，避免把进程调度预算混入语义断言；新增超时仍保留项目、Git evidence 为 unavailable 的回归。原有真实 Git 子进程与预算覆盖保留，生产 750 ms / 2.5 s 限制不变。
- combined snapshot 回归在两个数据族之间用独立文件描述符直接验证锁竞争，读取完成后验证释放；在实际 manifest 发布点通过仅测试启用的 hook 验证发布者持有排他锁。删除以 100 ms 未完成推断阻塞的断言，没有提高产品或测试超时。
- 独立复审发现只检查 reader 会漏掉 publisher 锁回归，以及负向脚本用例可能掩盖环境恢复失败；分别补入实际发布点检查和父进程必须看到的恢复成功标记。

开跑前固定四个相关用例、串行及四线程各 10 批、每进程 90 秒上限。改前与改后均 **20 批 / 80 项通过**，日志全部保留于 `n1/before-*`、`n1/after-*`；改前没有复现此前三个负载敏感失败，因此只能说明夹具与断言更确定，不能声称已经证明历史失败的唯一根因。重复批次之后又补入 reader 回调必须执行的 `Cell<bool>` 断言，该末次变更由第 13.5 节最终库全量覆盖，不属于此前 20 批的产物；`n1/summary.json` 保留前后哈希。相关 Git 模块 16 项、agent_management 2 项通过。

PowerShell 7.4.1 / 5.1.26100.9444 的验证外层契约各 78 项通过；随后默认路径修复涉及的六种 valid-zero 调用在两种 shell 各补验 6 项通过。契约数由每 shell 60 增至 78，相应 UTM 结果校验由每个 PowerShell 引擎 87 更新至 105，双引擎合计 210，未启动 VM。宿主 Python UTM 合约 10 项通过；首次沙箱符号链接 OS 1314 失败及两次已修复的测试编译错误保留，不计通过。

### 13.3 N2：受控测量

新增显式 opt-in 的 `scripts/windows/measure-tui-history.py`、Cargo 管理的 ignored `tests/tui_measurement.rs` 和库内合成夹具准备入口；不依赖硬编码的 rlib 文件名，不读取真实历史。流程是先 `--phase plan` 冻结输出目录、两个产品二进制、来源提交、私有 TEMP 和样本数，再依次 `prepare`、`pilot`、`summarize-pilot`、`sample`、`summarize`。`--help` 列出参数；每个批次目录只允许启动一次，失败或中断记录及未执行条目均保留。重新诊断使用明确的新批次，不能覆写失败后宣称原批通过。

对照产品分别是 `abef359eac136a2b004c7e4b98890cee27bd37fe` 和 `0656a201edbb5517a9f3ecac78549cbc6d8a15c0` 的现有产物，先复制冻结，再以可执行 SHA-256 和按对应 Git 源码重算的 buildId 双重核对。它们对应 M0—M6 前后生产实现，本轮 N1 Rust 变化仅在测试编译中启用。原构建日志支持相同 Rust 1.97.0、Windows x64、未优化 profile；未记录的有效编译 flags 保持未知，未重新构建产品来补造旧证据。debug 产物分别为 31,955,968 / 32,064,512 字节，差 108,544 字节；不外推 release 尺寸或受控构建成本。

四个固定条件为全新本地状态、已初始化本地状态、三个合成持久来源的 All 查询、三个来源下本地 Summary/Trends 与后台 All。最后一项依次观察稳定输入、跨五分钟槽位的 quota 变更、quota 策略由三个纳入源变为两个。每个条件/二进制预定 10 次，共 80 次，交替基线/候选顺序；同一绝对合成 Codex home、模板字节、来源数量、账户和工具版本固定。进程首次启动、空应用状态与未受控的 OS 文件缓存明确区分；这些来源没有真实 SSH 或 live 快照，不声称覆盖 live 新鲜度。

试测修复了三类仪器问题：Windows 的 CRLF marker 与 Rust 固定字节不符；光标位置回复分片写入使 ConPTY 在产品 main 之前等待；产品在 Windows 上对日志持有排他字节锁，运行中读日志不能用于等待就绪。最终采用完整回复单次写入、屏幕就绪并在退出后用 trace 时间核对、按 UTC 阶段边界读取已解锁日志。原失败、终止清理及独立诊断保留；它们不是产品延迟样本。quota 原先加一秒会落在同一槽位，已改为跨槽位并检查相对记录增长；query 场景只以成功完成事件确证。

三个来源的试测显示查询完成可能跨越 12/24 秒观察窗口，因此在正式开批前最后冻结为每个稳态阶段 45 秒、至少一个完成 refresh，并验证实际 quota/策略变化；cache hit/miss 是观测结果，不用期望的命中结果筛掉样本。初始诊断上限 30 秒、稳态诊断上限 55 秒、helper 硬期限 240 秒、退出/句柄清理预算 5 秒。正式 ConPTY 的 8 秒期限完全不变；上述诊断窗口不算正式通过。窗口冻结后不再延长，也不重试正式失败样本。

正式 `n2/protocol.json` 的 SHA-256 为 `fa6aef4b4593934b6e5667dda3a611d0cccc133614ab90bb7f7f6e5909cf3504`；基线/候选产品 SHA-256 分别为 `e2deccc4287075feffe9ccf08dc5808117b2360b24d2cb9bc7a3cc5656a7b3bf` / `063915937f390e6ac2b1fcf4c6ce7985f685e5e9e58f7b1f16ffe775538a4eba`。正式批于 `2026-09-26T16:04:45Z`（香港时间 9 月 27 日零时）启动，结果不回写成前一日旧基线的测试证据。

原批于 `2026-09-26T16:59:31Z` 完成，**80/80 次均执行，70 次 helper 成功、10 次失败**，没有补跑。输入、产品、helper、协议及测量源码哈希均未变化，`inputBatchValid=true`；场景完整成立 70/80，`allScenariosEstablished=false`。9 次失败是在 30 秒内没有初始有效帧：All 两版各第 5、6 轮，后台候选第 1、2、5、7、9 轮（轮号从 0 开始）。另一次后台候选第 3 轮已有有效初始帧，但 45 秒 idle 阶段没有完成 refresh，不能认定稳态场景成立。没有 helper 硬期限、清理失败或 trace/event 解析错误。

下表每格预定 10 次；时间单位为秒，仅统计 trace 确认 V2 且初始查询成功的有效帧。超时属于至少 30 秒的右删失结果，未被填成 30 秒、0 秒或丢弃。次慢值只是样本描述，不是稳定 p95。

| 条件 | 产品 | 有效帧 n | 初始超时 | 完整场景 n | 中位数 | 次慢 | 最大值 | 有效帧中 >8 秒 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| fresh_local | 基线 | 10 | 0 | 10 | 9.684 | 11.640 | 13.070 | 10 |
| fresh_local | 候选 | 10 | 0 | 10 | 9.795 | 11.023 | 12.339 | 10 |
| initialized_local | 基线 | 10 | 0 | 10 | 4.325 | 4.751 | 5.022 | 0 |
| initialized_local | 候选 | 10 | 0 | 10 | 4.552 | 5.097 | 5.114 | 0 |
| three_remotes_all | 基线 | 8 | 2 | 8 | 14.548 | 18.251 | 18.325 | 8 |
| three_remotes_all | 候选 | 8 | 2 | 8 | 24.408 | 28.222 | 29.129 | 8 |
| three_remotes_background | 基线 | 10 | 0 | 10 | 11.845 | 23.749 | 27.904 | 10 |
| three_remotes_background | 候选 | 5 | 5 | 4 | 26.271 | 28.162 | 29.591 | 5 |

加上初始超时，除 initialized_local 外，各格已知超过 8 秒的下限都是 10/10；这些带仪器的时间不是正式 ConPTY 测试结果。后台 quota 记录 1→2、策略纳入源 3→2 和独立 Local/All 成功查询，在基线 10 次、候选 4 次完整场景中成立；idle 阶段对应 10/4 次观察到缓存复用，依据无新增 usage/account 查询的完成 refresh 推断，未把推断写成直接测得的 cache-hit 事件。候选另一次虽然完成后两阶段，仍因 idle 证据缺失不算完整场景。

fresh 的首个正式启动为 13.070 / 9.820 秒，其余 9 次中位数为 9.515 / 9.769 秒；所有组的首个及重复启动均另列于 `n2/metrics.json`。正式首个启动之前已经做过身份探测、pilot 和验证，因此不能称为 OS 冷缓存。同职责 `history.stage_load` 的基线/候选中位数分别为 fresh **3.625 / 3.792**、initialized **3.283 / 3.732**、All **11.954 / 18.349**、后台 **5.345 / 11.912** 秒（均只取有效初始子集）。本地组接近，但两个多来源场景存在明显的候选变慢信号，不能宣称无性能退化。

有效初始阶段的 account_load/quota_merge 在前三组均为 1→1 次，后台从 2→1 次，证明共享 quota 减少了逻辑加载，但没有证明端到端更快。M3 把 quota 加载移到查询上下文，两版 `history.v2.query` 的计时范围不同，不能直接用其耗时差宣称同职责加速。逻辑加载 span 不等同系统调用或物理磁盘读取数；metadata_load span 不覆盖所有新增 revision 探测，不能把该 span 数减少当作总探测减少。嵌套 span 的耗时也不能直接相加为总成本。全部诊断、原始 trace/屏幕、每次完整调用和批次状态保存在 `n2/`，派生统计中的缺失字段记未知，不以 0 代替。

资源计数覆盖全部尝试及整个进程，后台包括三个 45 秒窗口；失败提前退出会使其 CPU/I/O 更少，不能据此认定节省。各组 peak working set 中位数为 **26.81–28.20 MiB**。All 的基线/候选进程 CPU 中位数为 **14.711 / 21.523 秒**，读取 transfer 为 **101,901 / 147,826 字节**，写入 transfer 为 **32,702 / 32,338.5 字节**，最终状态字节数中位数均为 **24,155**；这组资源值含两版各自的两个超时尝试。其余 CPU、I/O、状态大小、逐行 busy 比例与首次/重复启动见 `metrics.json`。All 的全机 busy 中位数为 77.2% / 89.3%，负载差异限制了耗时归因。

只读源码与 trace 联合复审定位了可选缓存的重复准备：在三远端、无重试的 All 路径中，候选新增内层 query_inputs 的前后探测共 6 次，以及 Overview 消费已有 unified seed 前为可选缓存执行的额外探测共 6 次。后者涉及私有目录、root lock、manifest 和 binding 检查。内层检查在变化时重试并清 quota，外层检查只决定能否缓存，范围也不同，不能互相替代。单个 probe 没有独立 span，不能把全部耗时差归给它们。

原 80 次封存后，针对 Overview 的 supplied-seed 分支单独实施窄修改：保留 metadata 和每个远端 active-ref 的现有错误检查，清除可选缓存并直接使用完整查询结果；没有 seed 时保持原查询、前后版本检查和 TTL。代价是之后首次无 seed 请求需要正常查询建缓存。该改动不覆盖 Local + 后台 All 的 None 路径，不解决上表后台失败。三项定向回归及主 agent 的受影响组合验证见第 13.5 节。

随后独立比较旧候选 `0656a20` 与窄优化 `216705c017ec9b2e5073fdb3ba930aa185adb7be`，只测已初始化三远端 All，固定交替 10 对、初始期限仍为 30 秒，不重试。复用原绝对 home、模板、账户、helper 和时间输入；准备入口验证原 driver 哈希，采样入口要求成功身份验证记录绑定同一协议与提交 buildId。新协议 SHA-256 为 `a61dded18a7df27e245dcaf4f755d81b229c1a48da039f25ff0ff5da231d7e9c`；新产品 SHA-256 为 `88fd68860ad4f199ac9e62a988190a2a2de25409aff1492db1f457359dead02d`，buildId 为 `519dc8e737b6ef7679a6845e95d35a34c90ad70e879fe438b5d1bd097e97e444`。新 debug 产物 32,031,744 字节；仍不据此推断 release 尺寸或受控构建成本。

该批于 `2026-09-26T17:13:00Z` 至 `17:18:11Z` 执行，**20/20 场景成立、失败 0、初始超时 0**，全部输入、产品、helper、协议及源码哈希未变。以下时间为秒：

| 独立 All 对照 | 有效初始 n | 中位数 | 次慢 | 最大值 | 有效帧中 >8 秒 | 同职责 stage_load 中位数 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 旧候选 `0656a20` | 10 | 16.594 | 17.610 | 18.115 | 10 | 11.855 |
| 窄优化 `216705c` | 10 | 13.814 | 14.233 | 14.409 | 10 | 11.510 |

10 个有效配对的就绪时间差中位数为 **-3.066 秒**、比值中位数 **0.821**，本批每对均更快；这支持当前合成场景的改善，不是一般性能承诺。trace 中 stage_load 结束至 initial_data_ready 的间隔中位数为 **3.836 / 1.488 秒**，20 行均恰有一组匹配、没有缺失；该间隔包含其他后续工作，不等于单独 revision probe 耗时。首个本批启动为 16.334 / 14.191 秒，其余 9 次中位数为 16.740 / 13.744 秒。原候选在本批已比原 80 次批的 24.408 秒中位数快，说明不能把不同时间批次直接混作同一对照，也不能宣称已经恢复到 `abef359` 的水平。

本批全部尝试的进程 CPU 中位数为 **16.164 / 13.531 秒**、读取 transfer **152,103 / 128,792.5 字节**、写入 transfer **32,346 / 32,337 字节**、peak working set **28,805,120 / 28,889,088 字节**，最终状态均 **24,155 字节**。全机 busy 中位数为 **46.2% / 44.9%**，较原批明显不同。资源覆盖启动至退出，不能称为某个 probe 的独立成本；两版全量有效 flags 仍未完全记录。这 20 次只验证 All supplied-seed 路径，不重测 fresh 或后台，不覆盖真实 SSH/live 新鲜度，也不改变原 80 次的失败结论。原始行、调用、身份记录及派生统计独立保存于 `n2/seed-comparison/`，没有覆写原批证据。

进程 I/O transfer 不是物理磁盘访问，working set 不是 allocator 峰值；revision 探测、quota DTO clone、真实冷缓存及受控构建成本没有独立测量时记未知，不补造 0 或性能承诺。试测记录到较高的全机 busy 比例，主 agent 只读检查确认仍有浏览器、编辑器、用户现有监控应用及非本任务确认的构建活动；没有终止用户进程。本任务只保证自己的采样与编译/重测试不并发，逐行保留负载。共享宿主的变化不能靠交替次序完全消除，不以这些样本作一般性能或稳定 p95 保证。

### 13.4 N3 与 N4

N3 使用仓库固定的 `cargo-audit 0.22.2` 对未改变的 Cargo.lock 执行 `audit --deny warnings`，RustSec 数据库提交 `e2111519ba6d14a5da59a7b2e5c8083ae8a37c01`（更新时间 `2026-09-25T19:51:57+02:00`）。247 个锁定依赖、1,271 条公告，**0 漏洞 / 0 warning**，无忽略项或 target 过滤。完整命令、锁文件哈希和时点限制见 [依赖审计记录](dependency-audit.md) 及 `n3/audit.log/json`。首次离线安装因工具未缓存失败，随后固定版本仅安装到工作区忽略目录；未改变全局 PATH、Cargo.toml 或 Cargo.lock。

N4 已增补第 9、10 节：A/C 独立/复制/分叉/缺失 facts/quota 例子、A0 集合 schema 与 CLI/UI/错误草案、最后消费者及 A4 新旧中心/agent/同机进程/中断/旧版本重开/备份回滚矩阵；P1 的固定合成档位、revision 保留、数值/安全/持久性/资源硬门及 P2 独立委托边界。JSON 示例和相对链接已静态校验。以上是待批准材料，没有选择 A，也没有创建 SQLite 适配或添加依赖。

### 13.5 组合验证、成本与剩余边界

主 agent 在源码稳定后只运行一次完整 Windows 入口。源码/脚本输入快照为 `75d0c31656b589261ff933542fbc43935f36f4f89dc84250db6af9f153a5c721`，测试前后相同；每条命令的完整参数、账户、环境与文件哈希见同名 JSON。正常宿主账户和私有 TEMP 与第 13.2 节一致，`CARGO_NET_OFFLINE=true`，target/build 分别为 `D:\Workspace\codex-usage-monit\target\refactoring-final` 和 `D:\Workspace\codex-usage-monit\target\refactoring-final-build`。

| 实际检查 | 结果 | 证据文件（本轮目录下） |
| --- | --- | --- |
| `pwsh -NoProfile -File scripts/windows/verify.ps1 -CargoTargetDir D:\Workspace\codex-usage-monit\target\refactoring-final -CargoBuildDir D:\Workspace\codex-usage-monit\target\refactoring-final-build -TestTempDir C:\Users\Ghost\AppData\Local\Temp\cm-next-20260926` | fmt、Clippy all-targets `-D warnings`、25 项 Python 契约、78 项 verify、17 项 dev、10 项 ACL 契约通过；库 1,780 passed / 1 ignored；已到达的集成目标 113 passed / 1 failed / 2 ignored。ConPTY 初始数据超时使入口退出 1，后续目标及 CLI 冒烟当时未运行 | `windows-full.log/json` |
| `cargo test --locked --offline --no-fail-fast --test update_cli --test usage_evidence -- --test-threads=4` | 补完未运行的目标：13 passed / 1 ignored | `windows-remaining.log/json` |
| `cargo test --locked --offline --test tui_pty real_tui_pty_handles_keyboard_mouse_search_resize_and_exit -- --exact --test-threads=1` | 一次定向串行复验仍失败，8.13 秒；未重跑整套或放宽 8 秒期限 | `windows-conpty-focused.log/json` |
| 同一 `verify.ps1` 及三个目录参数，加 `-SkipFormat -SkipClippy -SkipTests` | CLI 原生构建、version、offline JSON/partial/非空任务冒烟通过；源码前后未变 | `windows-cli-smoke.log/json` |
| `python -W error::ResourceWarning -m unittest discover -s scripts/windows/tests -p test_measure_tui_history.py -v` | 测量工具的 6 项失败清理、证据解析和有效样本筛选契约通过 | `n2/driver-contracts-final.log` |
| `target/tools/actionlint-1.7.12/actionlint.exe -oneline` | 通过；官方 Windows amd64 归档摘要已核对，工具仅位于工作区忽略目录 | `actionlint.log/json`、`actionlint-install.json` |

上述窄优化前的全量入口与补完目标合计 **1,780 项库测试、126 项集成测试通过，1 项正式 ConPTY 用例失败**。默认忽略的四项分别是原有真实历史 benchmark、旧二进制兼容检查，以及两个新测量入口；前者未获真实历史委托，兼容检查沿用第 11 节锁实现未变的既有证据，新测量入口仅由第 13.3 节显式执行。该统计没有把失败重算为通过，也不是一次全套全绿。

原 80 次封存后，新增 supplied-seed 的三来源/警告/错误/缓存生命周期回归，以及共享 quota/策略失效定向回归，**3 项通过**。只修改 `src/tui.rs` 与其测试后，主 agent 按影响补验，没有重跑全量：

| 窄优化后的实际检查 | 结果 | 证据文件 |
| --- | --- | --- |
| `cargo test --locked --offline --lib tui::tests -- --test-threads=4` | **340 passed / 1 ignored**；含新增回归 | `windows-tui-final.log/json` |
| `cargo test --locked --offline --test tui_pty --test tui_data_integration -- --test-threads=1` | **4 passed**；正式 ConPTY 的 8 秒期限未改 | `windows-conpty-final.log/json` |
| `cargo clippy --locked --offline --all-targets -- -D warnings` | 通过 | `windows-clippy-final.log/json` |
| `cargo fmt --all -- --check` | 通过 | `windows-format-final.log/json` |

四条命令均绑定完整源码/脚本快照 `d6a942d9df7178626d5aa6147364d12ee810a8fe939b098bc91012bf7cca8114`，各自执行前后相同；ConPTY 记录含新产品构建日志。测试时 HEAD 尚为 `2f6b689`，这两个文件仍未提交；其逐文件哈希与随后 `216705c` 完全一致，保留原日志中的 HEAD，不改写成提交后运行。两个快照均覆盖相同的 191 个 Cargo/构建配置、src/tests/scripts 文件，不含文档、环境或二进制。最终源码与首次全量之间仅上述两个 TUI 文件变化，其他模块沿用相同字节的全量证据，不把两批计数相加冒充新增覆盖。后一次正式 ConPTY 通过满足该窄改动的本次验证，但不是消除共享宿主冷启动风险的证明。

两次 ConPTY 失败均已显示正常 TUI 框架并停在 `Finalizing usage snapshot...`，不符合试测中卡在 main 之前的光标回复仪器问题。高共享负载是观测到的条件，但不足以证明唯一根因；既有冷启动期限余量风险仍未关闭。

只读复审进一步确认，这条进度文案在初始历史写入/查询阶段未更新，不能据屏幕断言 rollout materialize 是瓶颈。试测 trace 中该步骤仅为亚毫秒，后续 history.stage_load 及此前未充分细分的初始化均有成本；单次试测不足以支持性能补丁。正式失败分类保留为“Windows 冷启动数据就绪预算违约已复现，产品成本与宿主负载贡献未隔离”，不标为纯环境问题或已证明的 M3 回归。

相对 `62f6a3a`，生产 Rust 只调整了第 13.3 节 Overview supplied-seed 分支的可选缓存策略，公共接口未变；其余 Rust 变更为测试、夹具或 `cfg(test)` hook。Cargo.toml、Cargo.lock、Rust 工具链与生产依赖图不变，没有新增库。按 `source_cost.py` 的物理行/非空行口径，生产 TUI 净减 **10 / 10 行**，Rust 测试及 hook 净增 **778 / 764 行**，验证与测量脚本净增 **846 / 779 行**。测试与测量能力有维护成本，不能以生产分支减行宣称总体瘦身。库 API 的未来 A/SQLite 设计只在文档中存在。

本地提交 `da4d5440f7b6135007e380ee4d873c088e8d40bf` 保存 N1，`2f6b689e84e02ac8e43368242ce9248a18401caf` 保存 N2 测量工具；两次提交没有改变已验证的工作区源码字节，原 80 次与完整验证的快照仍为 `75d0c31656b589261ff933542fbc43935f36f4f89dc84250db6af9f153a5c721`。`216705c017ec9b2e5073fdb3ba930aa185adb7be` 保存上述已补验的窄优化。文档静态检查覆盖 20 个本地链接、2 个 JSON 示例及原审查文档字节哈希；最终检查结果与文档提交后的 HEAD 另存本轮 `docs-check.json` / `implementation-record.json`。提交只写本地仓库，未 push、触发 CI、创建 tag 或发布。

本轮没有执行 N5：当前 Windows 本机未使用相应 Unix/macOS 原生环境；按委托不运行 Docker、UTM、远端或 hosted CI。真实服务安装、真实历史基准及断电实验仍未执行。A 产品决策、SQLite 原型委托及生产迁移的门继续关闭。

[E1]: https://docs.rs/semver/1.0.28/semver/struct.Version.html#method.cmp_precedence "semver 的升级优先级比较"
[E2]: https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock "标准库锁、竞争错误与句柄生命周期"
[E3]: https://docs.rs/tempfile/latest/tempfile/struct.NamedTempFile.html#method.persist "persist 不等于完整持久化发布"
[E4]: https://www.sqlite.org/wal.html "SQLite WAL 与 WAL-reset 修复；2026-09-26 核对"
