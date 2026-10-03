# SQLite 历史存储真实 SSH 同步验收记录（2026-10-03）

## 1. 结论与验收范围

本轮已经实际执行 Windows 中心到 SSH alias `local-mac` 的 macOS 远端同步：两端独立前台 recorder 摄取受控非空 rollout，中心经系统 OpenSSH 启动远端短命 exporter，再把聚合、session digest、精确 facts 和配额写入各自 SQLite。CLI JSON、只读 SQL 导出、来源策略重启和真实 ConPTY TUI 刷新提供了交叉证据。

已证明首次非空 bootstrap、增量更新、重复同步不重复累计、测试进程重启后持久化、连接失败后保留提交并恢复、来源 exclude/include、受控复制会话的精确 facts 跟进及金额去重、关闭日连续正用量样本的完整 digest/proof、真实旧配额副本的 SQL 导出/接收/保存，以及初始化保留边界和 schema 1/失库/损坏拒绝。现有单测与先前 CI 不作为这些实机场景的替代证据。

用户确认同账户并批准新增窄 LAN sshd 后，第10节补测已通过 same-account merge/separate、真实多页 continuation 与候选激活、已提交页后的同步中断恢复、journal 丢失后的 bootstrap-restarted，以及 Mac→Windows 的非空同步和双向配额不转发。最终反向链路使用固定 CMD launcher 直接调用，避免编码 PowerShell；原路径的间歇 SSH255 与用户报告的杀毒告警保留，原因未确定。仍未覆盖 SQL 事务内部/最终激活窄窗口、自然 retention expiry、双端重叠原始 quota、正式 recorder 服务部署、真实用户性能和断电恢复。普通报告继续有 partial，不宣称全部历史完整。

准备时已阅读仓库 AGENTS.md、`.agent/environment.local.md`、[测试流程](testing.md)、[远端使用说明](remote-usage.md)、[存储执行方案](storage-rewrite-execution-plan-2026-10-02.zh-CN.md)第 1—6 节及 7.3，以及 [README.zh-CN.md](../README.zh-CN.md) 的 recorder、远端同步和目录配置说明。

本轮没有发现需要修改生产源码的确定缺陷，没有生产修复提交，没有新增测试 tag，也没有重新触发 hosted CI。

## 2. 实际源码、平台与二进制

### 2.1 源码绑定

- 分支：`codex/sqlite-history-storage`。
- 两端实际测试提交：`c4209d5cf27f6b4aee89da7e1bc8b58790153786`，生产源码 clean；没有比交接基线更新的生产提交。
- 实现提交：`58af8e1b0ad1163f8446f207d22874301af092c3`。本轮口径遵循[存储方案 7.3](storage-rewrite-execution-plan-2026-10-02.zh-CN.md)；不恢复旧派生历史迁移或 A 方案。58af8e1 到测试 HEAD 的差异只有 `docs/v0.4-remote-usage-sync-design.md` 的文档链接一行替换。
- 验收 harness、受控样本、隔离配置、日志和辅助程序位于忽略的 `target/sqlite-real-machine-2026-10-03/`，不属于生产 build 输入。其变化不构成生产源码 dirty snapshot，但必须随证据保存，不能假定 Git 会同步这些文件。
- 既有 CI：[run 37046056597](https://github.com/ghostroller/codex-usage-monit/actions/runs/37046056597)。这是用户提供的交接时已有结果，本轮未重新核验该 CI，也未用它代替真实 SSH 验收。

两份 `git archive` 均带 PAX commit 标记 `c4209d5cf27f6b4aee89da7e1bc8b58790153786`。Windows tar 为 10,926,080 字节、SHA256 `1552a6dc1325bf95d4fdced1cbb23affb145db21b64bfdbce75c4e7d6f39af3a`；macOS tar 为 10,659,840 字节、SHA256 `ffe804adf72025a65a3b83fc21d82720f30b0e13ecebbe6b782405fdd66d552a`。234 个文件的原始差异全部是 CRLF/LF；归一化后没有内容差异、增删或改名。按 `build.rs` 的路径排序、长度 framing 和换行归一化规则独立计算，122 个构建输入得到相同 build ID。逐文件原始/归一化摘要保存在 `source-archive-audit.json`。

### 2.2 平台与运行身份

| 端点 | 平台与架构 | 实际运行身份 | 角色 |
| --- | --- | --- | --- |
| 本机 | Windows 10.0.26200、x64；Rust 1.97.0 MSVC | `Ghost`，高完整性管理员用户、非 SYSTEM | 中心，主动 SSH 同步、持久化接收、CLI 与 ConPTY TUI |
| `local-mac` | macOS 15.7.2、ARM64；Rust 1.97.0 | `user` | 独立 recorder、短命 SSH exporter |

本机 `.agent/environment.local.md` 指定使用原生 Windows 并不主动触发 GitHub CI；本轮遵循该补充说明。首轮没有停止、卸载或修改既有正式服务，也没有注册测试后台服务。补测新增的 SSH 服务见第10.3节；正式 Codex recorder 服务始终未改。

### 2.3 构建与实际执行路径

Windows 构建命令在 `D:\Workspace\codex-usage-monit` 执行：

```powershell
cargo build --locked --offline
```

Cargo 实际输出为 `D:\Workspace\codex-usage-monit\target\debug\codex-usage-monit.exe`，复制为独立验收二进制：

`D:\Workspace\codex-usage-monit\target\sqlite-real-machine-2026-10-03\bin\codex-usage-monit.exe`。

macOS 在 `/Users/user/Workspace/codex-usage-monit` 执行：

```sh
CARGO_TARGET_DIR=/Users/user/Workspace/codex-usage-monit/target/review-lock-storage \
CARGO_BUILD_BUILD_DIR=/Users/user/Workspace/codex-usage-monit/target/review-lock-storage-build \
CARGO_NET_OFFLINE=true cargo build --locked
```

Cargo 实际输出为 `/Users/user/Workspace/codex-usage-monit/target/review-lock-storage/debug/codex-usage-monit`，复制到 `/Users/user/sqlite-real-machine-2026-10-03/bin/codex-usage-monit`。macOS 构建耗时 0.28 秒是复用匹配源码的开发构建缓存；源码归一化和运行时 build ID 另行核对，未用耗时替代身份检查。

| 项目 | Windows | macOS |
| --- | --- | --- |
| binary SHA256 | `0c07c477a419c635e27ae9125ef69ec4d56c4cb1973011b3c2863283a0fdf386` | `9efeaa51dd11657734b9e660f04b90a44df797a985158164d9444f1d888a45ad` |
| build ID | `4437198a3d1b417f7b94975c96dc9bdff07b1623090a98596fc2ff02a4e3d8c3` | 同左 |
| package / target | 0.5.2 / x86_64-pc-windows-msvc | 0.5.2 / aarch64-apple-darwin |
| protocol | 5 | 5 |
| 数据 revisions | history 2、metric 5、estimator 8、project breakdown 2、API pricing catalog 5 | 同左 |
| effective catalog fingerprint | `model-catalog-sha256-v1-6427b2d0d1ed3e4b923cb56693e8979a37a7cd11f49dc5766e36209c59b60707` | 同左 |

两端实际执行 `remote-agent info --sha256` 并核对输出；对应 `binary-info.stdout`、`binary-info.command.json` 和构建日志随证据保存。本轮没有调用 official Release、`remote deploy` 或 `deploy-dev`，没有更新正式 CLI 或 recorder。

## 3. 隔离、身份与时间窗口

主验收根目录：

- 中心：`D:\Workspace\codex-usage-monit\target\sqlite-real-machine-2026-10-03\center`。
- 远端：`/Users/user/sqlite-real-machine-2026-10-03/remote`。
- 中心 source：`node-ec8b8a231d4ea6360c7bf446337585c4`。
- 远端 source / 精确报告 selector：`node-0951ab7a05236a9001b25da81b06b9c6`。
- 中心 DB：`center/state/history-v2/505486f9cf646fec/history.sqlite3`。
- 远端 DB：`/Users/user/sqlite-real-machine-2026-10-03/remote/state/history-v2/4d3231c986cf1a0c/history.sqlite3`。

各端分别固定 `<root>/codex` 为 CODEX_HOME，`<root>/state`、`<root>/config`、`<root>/cache` 为三个应用目录。主样本是受控 rollout，没有复制 state-root、SourceIdentity 或 auth；相同会话的受控副本仅复制明确的 thread/event 时间与 token 证据，cwd 采用各端现存私人项目目录。所有采集、报告、同步与 exporter 均使用 `--offline --redact-content`。

Windows 私人测试目录采用正常用户 ACL；macOS 私人目录/launcher 为 0700、普通样本文件为 0600。本轮不使用 `--history-dir`；若单独使用它，末尾须为 history-v1，实际 DB 仍在 state-root/history-v2/{profile}/history.sqlite3，且不能替代这四个目录的隔离。SQL 只读检查以 SQLite URI `mode=ro` 打开并设置 `PRAGMA query_only=ON`。没有为正向场景编辑数据库制造数值或 proof。负向失库/损坏/schema 样本仅作用于已停止写者的可丢弃试验目录。

中心变量不会传到远端；远端独立 launcher 为：

```sh
#!/bin/sh
export CODEX_HOME=/Users/user/sqlite-real-machine-2026-10-03/remote/codex
export CODEX_USAGE_MONIT_STATE_DIR=/Users/user/sqlite-real-machine-2026-10-03/remote/state
export CODEX_USAGE_MONIT_CONFIG_DIR=/Users/user/sqlite-real-machine-2026-10-03/remote/config
export CODEX_USAGE_MONIT_CACHE_DIR=/Users/user/sqlite-real-machine-2026-10-03/remote/cache
export TZ=Asia/Hong_Kong
exec /Users/user/sqlite-real-machine-2026-10-03/bin/codex-usage-monit \
  --offline --redact-content \
  --codex-home /Users/user/sqlite-real-machine-2026-10-03/remote/codex \
  --trace-log /Users/user/sqlite-real-machine-2026-10-03/remote/logs/exporter-$$.trace.jsonl "$@"
```

launcher 保留 stdin/stdout/stderr 和参数；stdout 不输出 banner、不合并 stderr。SSH host key 和 batch authentication 由正常系统 OpenSSH 配置处理。

用户当地验收日期为 Asia/Hong_Kong 的 2026-10-03；机器证据记录 UTC。构建/环境准备约从 UTC 18:56 开始，主场景由 `2026-10-02T18:58:05Z` 的远端取证运行至 19:17 的完整关闭日报告；最终两端进程及二进制复核于 `2026-10-02T19:26:05.176588Z` 完成，即当地 2026-10-03 02:56—03:26。macOS 时钟约比中心落后 2 秒，逐命令 Summary 的 generatedAt/window 会有几秒差异；数值对照针对两端窗口共同包含的固定样本，而不是声称每条命令的 startsAt/endsAt 字节完全相同。

## 4. 实际命令与过程记录

harness 执行的 CLI 真实 argv、环境、开始/结束 UTC、退出码分别保存在 `<name>.command.json`；stdout/stderr 分开保存。下列为相同环境下实际执行的命令结构，完整逐次展开值以 command JSON 为准。

```powershell
$Case = 'D:\Workspace\codex-usage-monit\target\sqlite-real-machine-2026-10-03'
$Center = "$Case\center"
$WinBin = "$Case\bin\codex-usage-monit.exe"
$env:CODEX_HOME = "$Center\codex"
$env:CODEX_USAGE_MONIT_STATE_DIR = "$Center\state"
$env:CODEX_USAGE_MONIT_CONFIG_DIR = "$Center\config"
$env:CODEX_USAGE_MONIT_CACHE_DIR = "$Center\cache"
$Node = 'node-0951ab7a05236a9001b25da81b06b9c6'

& $WinBin --offline --redact-content --codex-home "$Center\codex" remote-agent info --sha256
& $WinBin --offline --redact-content --codex-home "$Center\codex" record --foreground --local-interval-seconds 5 --account-interval-seconds 30 --status-file "$Center\state\test-recorder-status.json"
& $WinBin --offline --redact-content --codex-home "$Center\codex" remote add acceptance-mac --ssh-host local-mac --agent-executable /Users/user/sqlite-real-machine-2026-10-03/remote/launcher
& $WinBin --offline --redact-content --codex-home "$Center\codex" remote pair acceptance-mac
& $WinBin --offline --redact-content --codex-home "$Center\codex" remote test acceptance-mac
& $WinBin --offline --redact-content --codex-home "$Center\codex" remote sync acceptance-mac
& $WinBin --offline --redact-content --codex-home "$Center\codex" summary --range 7d --grain 1h --format json --source all
& $WinBin --offline --redact-content --codex-home "$Center\codex" trends --day-offset 0 --format json --source all
& $WinBin --offline --redact-content --codex-home "$Center\codex" remote source exclude $Node
& $WinBin --offline --redact-content --codex-home "$Center\codex" remote source include $Node
```

reports 同时对 local、上述 NODE_ID、all 运行；远端通过同一 launcher/profile 运行 local 对照。recorder 是有界前台子进程。初始 Windows harness 的 readiness 字段错误，超时后仅终止自身测试 PID，exit 1；一个重启步骤也曾误读旧 heartbeat 后终止超时进程。这些失败保留，初始摄取只按 heartbeat/SQL 判定，不把终止码当自然完成。随后 readiness 改为匹配当前 PID、lastHistoryHeartbeat 和无错误状态；Windows 使用绑定 PID/startedAt/build ID 的正常 cooperative stop request，macOS 使用 SIGINT，priced 与策略重启等 recorder 均 exit 0。最后两端无遗留测试进程，见 `process-final.json`；没有动正式进程。

TUI 在 `exec_command(tty=true)` 的原生 Windows 终端会话 27024 运行，PID=46692。`center/logs/tui-trace.jsonl` 记录实际 command=tui（无子命令）、offline=true、redactContent=true、lookbackDays=7、maxFiles=500，启动于 `2026-10-02T19:06:28.378995500Z`；私有四目录环境和 binary 与中心相同。原始启动 argv 未另外保存成 command JSON，这是交互证据的记录限制。相同配置的复现启动形式如下；实际按键、自动刷新和退出以 VT 记录及 `tui-session-evidence.json` 为准。

```powershell
& $WinBin --offline --redact-content --codex-home "$Center\codex" --trace-log "$Center\logs\tui-trace.jsonl"
```

同步未启用自动轮询；所有连接是明确的一次性 sync。`remote test` 的 exit 0 只用于就绪检查；后续有实际非空 sync、SQL 行和报告对照。


关键命令的实际时间与退出码（UTC；完整 argv/环境在该 JSON，stdout/stderr 同 basename）：

| 命令 metadata | 开始 | 结束 | exit |
| --- | --- | --- | --- |
| `center/logs/bootstrap-sync.command.json` | `2026-10-02T18:59:10.829534Z` | `2026-10-02T18:59:15.832012Z` | 0 |
| `center/logs/increment-sync.command.json` | `2026-10-02T19:01:01.985896Z` | `2026-10-02T19:01:05.311020Z` | 0 |
| `center/logs/failure-sync.command.json` | `2026-10-02T19:03:46.468849Z` | `2026-10-02T19:03:47.881231Z` | 1 |
| `center/logs/recovered-sync.command.json` | `2026-10-02T19:03:55.416161Z` | `2026-10-02T19:03:58.793105Z` | 0 |
| `center/logs/policy-restart-recorder.command.json` | `2026-10-02T19:04:10.127965Z` | `2026-10-02T19:04:13.305669Z` | 0 |
| `quota-center/logs/quota-fixed-sync.command.json` | `2026-10-02T19:12:51.624257Z` | `2026-10-02T19:12:55.037063Z` | 0 |
| `quota-center/logs/quota-fixed-repeat-sync.command.json` | `2026-10-02T19:13:03.236204Z` | `2026-10-02T19:13:06.766299Z` | 0 |
| `center/logs/full-day-sync-0.command.json` | `2026-10-02T19:16:53.126223Z` | `2026-10-02T19:16:57.844775Z` | 0 |
| `center/logs/full-day-sync-1.command.json` | `2026-10-02T19:16:57.961457Z` | `2026-10-02T19:17:02.494256Z` | 0 |
| `center/logs/full-day-sync-2.command.json` | `2026-10-02T19:17:02.620793Z` | `2026-10-02T19:17:06.723008Z` | 0 |
| `center/logs/full-day-sync-3.command.json` | `2026-10-02T19:17:06.801247Z` | `2026-10-02T19:17:12.078163Z` | 0 |
| `center/logs/full-day-sync-4.command.json` | `2026-10-02T19:17:12.161884Z` | `2026-10-02T19:17:15.487620Z` | 0 |
| `center/logs/full-day-sync-5.command.json` | `2026-10-02T19:17:15.570313Z` | `2026-10-02T19:17:18.852900Z` | 0 |
| `center/logs/full-day.summary.all.command.json` | `2026-10-02T19:17:23.691009Z` | `2026-10-02T19:17:25.219556Z` | 2 |
| `center/logs/full-day.trends.all.command.json` | `2026-10-02T19:17:25.222545Z` | `2026-10-02T19:17:26.652900Z` | 2 |

普通报告 exit 2 的 partial 诊断与同步的 exit 0 aggregate complete 分别记录。首轮没有观察到 sync continuation/bootstrap-restarted，也没有将报告 partial 当成这些续跑状态。最后六次 aggregate 均 complete/pages=1；facts 另行经过 local-activated、remote-activated、not-needed，最终无 attention/awaiting-exact-digest。

## 5. 场景结果与数值对照

| 场景 | 预期 | 实际 | 结果及证据 |
| --- | --- | --- | --- |
| 首次 bootstrap | 两端非空 SQLite；中心精确远端等于远端 local；共同会话不重复累计 | 中心 local 2500、远端/中心 exact 3900；All 4900 = 2500 + 3900 − 共同 1500 | **通过，限定为观察数值**。`bootstrap.*`、`bootstrap-recorder.*`；初次 All 仍有 dedup_unavailable，facts 完成另见下一行 |
| 精确 facts 跟进 | 真 SSH 拉取事实；两端 manifest/proof 跟进，移除未完成去重诊断 | sync facts 从 local-activated → remote-activated → not-needed；All 4900；`duplicate_session_dedup_unavailable` 消失 | **通过**。`exact-dedup.*`、SQL facts/manifests/digest；仍保留项目/model partial |
| 增量 | 远端新增 600，旧值不丢失 | exact 3900 → 4500；All 4900 → 5500；local 2500 | **通过**。`increment.*`、`increment-recorder.*` |
| 幂等与重启 | 无新增时重复 sync；停止并重启测试进程后不累计重复 | 重复 sync aggregate complete；重启后 All 5500，原用量与同步状态仍存在 | **通过**。repeat/restarted 对应 command JSON、`restarted.*`、SQL active/cursor/local revision。未要求 DB/WAL 字节不变 |
| 明确金额的复制会话 | 两端各保留新增共同 600；All 只计一次金额 | local 3100、exact 5100、All 6100；各 exact 和 All 的 API min=max 均为 2,117,500,000 pico-USD | **通过**。`priced.*`；新样本提供真实可解析的明确 default tier；旧无 tier 样本继续不猜价格 |
| 连接失败与恢复 | 失败不删已提交数据；恢复后继续且不重复 | 测试配置临时指向 `.invalid` DNS；应用 exit 1，底层 SSH 255；失败报告仍 local 3100/exact 5100/All 6100；恢复 local-mac 后 sync 成功，数值相同 | **通过，连接失败路径**。`failure-*`、`failed.*`、`restore-edit.*`、`recovered-*`；没有停真实 SSH 服务 |
| 来源 exclude/include | All 排除后只含 local；exact 数据保留；重启后策略仍有效 | exclude All 3100、exact 5100；重启仍相同；include All 恢复 6100 | **通过**。`excluded.*`、`excluded-restart.*`、`included.*`、source policy SQL；精确 excluded 报告明确标识 |
| 显式项目映射 | 独立 source 不靠相同路径自动合并；已知受控项目经正常 API 明确合并 | 公共 ProjectMappingStore CAS API revision 2→3；All 仍 6100；replica_project_conflict 消失 | **通过**。`merge_projects.rs`、`logs/project-merge.log`、`project-merged.*`。不改 DB 或 proof |
| 原生 TUI 刷新 | 真实交互终端中，新增 SSH 同步结果可见 | Windows ConPTY 中实际发送 `u`、`7`，All 6100→6400；`q` 退出 0；后续 CLI exact 5400、local 3100、All 6400 | **通过，限实际按键与自动数值刷新；未验证完整帧、鼠标或 compact 布局**。`logs/tui-refresh-terminal.json`、`logs/tui-exit-terminal.json`、`tui-final.*`、远端 `tui-increment-recorder.*` |
| 稀疏关闭日复制会话 | facts/proof 对新增 1200 精确跟进；缺少全天覆盖须保持 partial | local 4300、exact 6600、All 7600；API min=max 5,096,000,000 pico-USD；两端对应 facts active，validated digest 相同事件 fingerprint；coverageComplete=false、coveredThrough 停在 UTC 日起点 | **通过，精确事件数值/金额；全天完整性未证明**。`closed-proof.*`、`logs/closed-proof.log` |
| 96 个正调用的关闭日覆盖 | 每 15m 一个精确非零调用，合法覆盖连续 bucket；额外 192 tokens 和 1,512,000,000 pico-USD 在 All 仅计一次 | 最终 local 4492、exact 6792、All 7792；All callCount=103，pricedSamples=98、pricedTokens=1992；API min=max 6,608,000,000 pico-USD（$0.006608），只计一次；两端关闭日的两个共享会话均 coverageComplete=true、coveredThrough=2026-10-02T00:00:00Z | **通过，限定为该受控关闭日与精确复制事件**。6 次有界真实 SSH sync；每命令独立 trace、SQL manifest/digest/facts；`center/logs/full-day-sync-0..5.command.json` 和对应 stdout/独立 trace；报告及 SQL 为 `full-day.*`，精确窗口独立复算为 `event-validation.json` |

金额单位为 pico-USD；2,117,500,000 pico-USD = $0.0021175。新增 priced 样本的 gpt-5.3-codex default tier，500 input 中 100 cached，100 output：400×1,750,000 + 100×175,000 + 100×14,000,000。被同步的两个副本不把金额加成两倍。closed-proof 阶段总 5,096,000,000 pico-USD 也只计算 each distinct observed event once。

### 5.1 partial 与 proof 的准确含义

初始受控样本未提供 service tier，故 API cost 为已知下界，不把缺失 tier 默认为 standard。后续 priced 样本在 `turn_context` 前添加 `event_msg/thread_settings_applied` 的明确 `service_tier=default`；只是新受控输入，不修改已有 SQL。

相同 thread/event 副本保留独立 source identity。ObservedProjectKey 包含各端 NodeId/密钥，即使 cwd 字符串相同也不会自动共享项目身份。显式项目 Merge 只改变中心用户配置职责，消除已知项目归属冲突，不能代替 facts。

`duplicate_session_model_breakdown_partial` 在现有 facts union 重建 thread/project 组时明确保留：facts 持有精确 additive token/费用指标，但不足以恢复完整 model 维度。此警告不等于相同事件已重复收费。

稀疏 closed-proof 的不完整不是因为文件名、SSH 或 SQLite 损坏。`digest_coverage` 从 UTC 日零点逐一寻找连续 15m bucket；23:50 的单个事件缺少 00:00 bucket，产生 `session_range_coverage_gap` 和 `digest_event_outside_coverage`。recorder 连续观察从实际启动开始，历史数据也可能带 `rollout_local_coverage_unverified`。closed day 的时间已经过去、完整扫描或成功 heartbeat，都不能证明全天缺失 bucket 为零。

active facts 验证的是已观察 digest 的精确事件/数量/metrics/binding/proof，允许 digest 的整日 coverage 仍 incomplete。因此 facts=not-needed 或 sync exit 0 不能单独升级为全天完整性证明。最终 7d All 的覆盖为 98/672 个 bucket（14.58%），sourcePartial=true。普通 7d Summary 和 24h Trends 因起止落在桶内、当前 open bucket、recorder coverage 起点或 model 维度而 exit 2；这些报告不与 sync continuation 或 remote test 失败混为一谈。

补充的 96-call 样本没有伪造 coverage 标记：同一已关闭 UTC 日，每 15m bucket 有一个真实解析的正用量受控事件；两端运行正常一次性 local Summary 并继续 SSH 同步。SQL 中 `019f-test-closed-priced-shared` 的 1 个事件和 `019f-test-full-day-shared` 的 96 个事件，都具备两端 active facts、validated digest、coverageComplete=true，coveredThrough 均为 `2026-10-02T00:00:00Z`。两端事件 fingerprint 分别为 `session-digest-sha256-v1-33a723caf753e67a81a390cd52f5f4ad8220931b326912eca2b7f31d560d08a0` 和 `session-digest-sha256-v1-77479d215811092df91f0f3ed4a736e6ec35937290074b6576703886b2e8916d`。此前稀疏样本的 partial 证据继续保留；本次补充不回写其当时结论。

96-call 场景的 Oct1 整日数据不全部位于当前 24h Trends。最终窗口为 `2026-10-01T19:30:00Z`—`2026-10-02T19:30:00Z` 时只包含其中 18 个调用，预期 local 4336、exact 6636、All 7636；三个实际 JSON 均与独立逐事件复算一致；`event-validation.json` 按各 CLI JSON 的真实时间窗口验算 Summary 和 Trends 共 6 个数值，全部通过（local 100、remote 102、去重并集 103 个事件）。受控原始 fixture 只由测试 harness 取证下载，未通过用量同步协议传输原始日志。不能把 7d Summary 7792 与 day-offset 0 Trends 总数直接要求相等。整份 7d/model 维度报告继续 partial，关闭日受控 proof 的通过不替代更广历史覆盖。

## 6. 真实旧配额副本的 SQL 导出与接收

经明确授权，在专用 quota 根目录保留了 2026-08-29—31 的 84 条真实旧配额观测副本。未修改原样本内容，没有复制或修改 auth；首轮尚未确认同账户，后续用户确认及 merge/双向补测见第10节。

- 远端专用 root：`/Users/user/sqlite-real-machine-2026-10-03/quota-remote`。
- 中心专用 root：`D:\Workspace\codex-usage-monit\target\sqlite-real-machine-2026-10-03\quota-center`。
- 该来源独立 NODE_ID：`node-5c2b5f79e85288e0740bff78cbf0369f`。
- 专用 root 各自仍隔离 CODEX_HOME/state/config/cache；launcher 设置其自己的绝对路径。

| 检查 | 实际 | 结果 |
| --- | --- | --- |
| 旧副本 → 来源 SQL 本机 account | 84 条；逐 payload 比较一致 | 通过 |
| SQL quota 本机导出 → SSH 接收 → 中心 SQL | 3 个日记录、84 个 points；来源 SQL 与中心接收 payload 比较一致 | 通过 |
| 重复同步、重启后再同步 | sync exit 0，仍 84 条；没有重复累计 | 通过 |
| separate-quota | `quotaMatchesLocalAccount=false` 持久化 | 通过 |
| merge-quota | 首轮未运行；用户后续确认同账户，补测见第10.1节 | 首轮未覆盖，补测通过 |
| 双向导入配额不转发 | 首轮未建立反向链路；后续真实 Mac→Windows 补测见第10.3节 | 首轮未覆盖，补测通过 |

配额初次副本父目录被 harness 建成 0755，正常权限 guard 拒绝，recorder exit 1；保留失败记录后仅把该私有父目录改为 0700，再重跑成功。早期比较器也曾把接收 SQL 的 3 个日记录与来源 84 个 points 直接比较，已改为逐 point payload 对照；修正只发生在验收 harness，未编辑 SQL 业务值。

最终核对摘要为 `quota-final-comparison.json`；原始本机 SQL、接收 SQL、独立命令和 exporter/trace 在 `quota-center/logs`、`quota-macos-fixed-evidence/quota-remote/logs` 和 `logs/quota-fixed.log`。本文仅记录数量、日期范围、身份和核对结果，不公开真实配额点值。

这些旧样本仍落在本次导出保留域内，但不在当前 24h Trends 窗口；空当前 quota 曲线不能作为导出/接收失败。没有在线账户 API 采样证据，不宣称 live quota 已覆盖。

## 7. 初始化、拒绝转换、失库与损坏

该节采用合成 quota/control 旧文件夹具，真实旧配额另见第 6 节。Windows 原生边界 harness 最终第三次 run 为 `20261002T191042.909149Z`，56 项检查全部通过。日志：`logs/boundaries-third.log`；结构化结果：`boundaries/20261002T191042.909149Z/result.json`。

| 场景 | 预期与实际 | 状态 |
| --- | --- | --- |
| 旧文件初始化 | 仅保留 quota、SourceMetadata/用户策略、local revision 高水位和未完成来源删除意图；旧 bucket/weekly/digest/facts/proof/游标/pending 不导入；receipt 表明 derivedHistoryImported=false | 通过 |
| 未完成删除意图 | 预置合法旧 purge 标记，分别有/无 metadata；即使 metadata 不完整，保留 purge intention；实际 `remote source include` 控制状态变更被拒绝（不可逆 purge pending fence）。本轮没有另外尝试 SSH 重新配对 | 通过 |
| schema 1 开发库 | 在具有正确 ownership 前置条件的可丢弃 specimen 上拒绝自动转换；不删库或回退旧文件 | 通过 |
| 激活后丢库 | 停止全部 harness 写者后，仅删除可丢弃 specimen 的 DB；source lifecycle exit 1，health exit 2/readOnly；没有静默重建 | 通过 |
| 损坏 DB | 停止写者后，仅损坏可丢弃 specimen；明确 file is not a database，lifecycle exit 1、health exit 2/readOnly；没有修复或旧文件 fallback | 通过 |
| ownership/旧 quota 保留 | 拒绝读写后 ownership manifest 和旧 quota 保持，错误没有造新 epoch | 通过 |

前两次边界执行存在 harness 夹具问题：错误 local source label，以及 schema 1 specimen 缺少正确 ownership。失败记录仍保存，修正试验前置条件后重做最终 run；不能把这两次归类为产品缺陷或删除失败证据。改动只在忽略的验收 harness，不是生产修复。

所有破坏性样本目录都有绝对目标 containment 检查与写者退出记录；主中心、主远端和真实用户目录没有进行丢库/损坏操作。

## 8. SQLite 与真实 SSH 的证据链

主同步与配额验收数据库均为 schema 2，application_id=1129663816（0x43554d48），只读 `PRAGMA integrity_check` 返回 ok。`history_records` 的 bucket、session-digest、facts、quota 等实际记录，和 `history_state` 的 receipt、local committed revision、remote active generation、ingest cursor、source policy、fact manifest 来自 SQLite；没有以旧 JSON shard 值代替 SQL 检查。

SQLite 检查工具限制也保留：macOS 系统 Python SQLite 3.43.2 在所有写者退出、WAL 已空时曾报只读 WAL 打开错误。仅确认无写者且无非空 WAL 后，inspector 才使用 immutable=1 只读连接；它的 connection journal_mode=delete 不用于断言产品 WAL 关闭。当前实现的初始化 receipt 记录 SQLite 3.53.2，正常 URI 只读检查及中心最终 SQL 返回 journal_mode=wal。数据库没有通过该 workaround 写入或重建。

失败恢复的独立 SQL 核对见 `event-validation.json`：连接失败前后 active ingest generation 均为 `ingest-gen-e0569b5d31c5edee73be88e0619ae9bf`，cursor generation=14675412464320305668、sequence=9，未推进；恢复后业务数值相同。

交叉链路包括：

1. 两端实际二进制 info/checksum 与独立 source 身份。
2. bounded recorder PID/heartbeat 与 local SQLite observation。
3. 真实 remote add/pair/test 和由中心发起的 OpenSSH sync。
4. 远端 local 报告与中心 exact NODE_ID 的数值对照。
5. 中心 All 的观察 token 与明确金额去重，facts activation/proof/cursor 的 SQL 记录。
6. 来源策略与进程重启后的相同 SQL 数值。
7. 真实连接失败保留旧提交，恢复后可继续同步。
8. 真实 ConPTY TUI 读取新增 SSH 持久化数值。
9. 专用真实旧 quota 来源 SQL → SSH exporter → 中心接收 SQL 的 payload 相等。

主日志模式为 `<phase>.summary.<selector>.stdout`、`<phase>.trends.<selector>.stdout`、`<phase>.db.json`，配套 `<name>.command.json` 和 `.stderr`。`evidence-comparison.json` 汇总已有报告和 SQL proof 记录；`compare_evidence.py` 仅读这些已保存文件，不打开或修改业务 DB，也不自行伪造 cryptographic proof。

初始 harness 多条命令共用 `trace.jsonl`，后来的命令覆盖了早期 trace。这是证据保存限制：早期 bootstrap/增量/策略依赖分开的 CLI 输出、真实命令 metadata 和 SQL 导出，不能声称完整保留每一次早期 SSH span。最后的关闭日覆盖批次改为每命令独立内容脱敏 trace；6 个独立 trace 文件的真实 SSH exchange 均有 outcome=ok，并与 SQL 发布记录一起归档。`transport-trace-index.json` 包含逐 span 摘要；例如 full-day-sync-0 于 `2026-10-02T19:16:55.347897500Z` 完成 exchange，request 768 B、response 10325 B、decoded response 214159 B、stderr 0 B。

## 9. 首轮边界、补测对应与原证据归档

| 项目 | 首轮边界及补测状态 |
| --- | --- |
| 多页 aggregate bootstrap 与 continuation/bootstrap-restarted | 首轮未触发；第10.2节真实六页补测通过，journal 丢失恢复通过 |
| 页提交后的中途断连、候选未激活状态恢复 | 首轮只有连接建立失败；第10.2节提交934行候选页后中断及恢复通过，SQL事务内部/最终激活窄窗口仍未覆盖 |
| same-account quota merge | 用户后续明确确认同账户；第10.1节 merge/separate 与重启通过，双端重叠原始采样冲突未覆盖 |
| 双向 quota 不转发 | 第10.3节真实双向 SSH + SQL 补测通过，含 Windows 非空合并投影导出挑战 |
| 更广完整历史 | 稀疏 closed-proof 的当时结果仍 partial；96-call 仅证明指定受控关闭日。密集样本的通过不证明真实用户删除/缺失日志不存在，7d/model 维度仍 partial |
| 正式 recorder 服务部署、正式安装升级 | 始终未执行；用户批准的新增窄 LAN sshd 配置另见第10.3节 |
| 真实用户历史性能、断电恢复 | 未执行，不新增性能或故障承诺 |
| 平台全量回归/新 hosted CI | 本轮无生产修改；已有三平台/CI 证据仍以各自提交为准，没有重新借用为实机结论 |

证据当前位于 `D:\Workspace\codex-usage-monit\target\sqlite-real-machine-2026-10-03`；远端目录为 `/Users/user/sqlite-real-machine-2026-10-03`。已下载 `macos-logs-final.tar.gz`、`quota-logs-fixed.tar.gz` 等中间归档。最终证据已独立打包，见下列摘要；中间 tar 不代替最终证据包，target 不随 Git 自动传输。

证据包绝对路径：`D:\Workspace\codex-usage-monit\target\sqlite-real-machine-2026-10-03\acceptance-evidence.zip`。

- SHA256：`0afaf67abc219c277c0842234b62baae631d07169b05d8f717a0740797c4addb`。
- 大小：1,099,760 字节；894 个文件，170 份逐命令 metadata，6 份含实际 SSH span 的独立 trace。
- 包含内容：CLI stdout/stderr 与命令元数据、内容脱敏 trace、只读 SQL 导出、受控 rollout、副本初始化夹具的安全结果、失败与恢复记录、harness 源码及逐文件 `evidence-manifest.json`。真实旧 quota 点值仅保存在此私有证据中。
- 排除内容：全部 state/config/cache 子树、auth、SourceIdentity、anchor/lock、现场数据库、二进制和源码 tar；身份秘密不作为可移交证据。构建二进制和源归档仍保留在各自测试机的私人目录。
- 归档复核曾发现边界身份文件误收，以及配额解包文件 ACL 异常，已修正打包器和私有文件权限后重新生成。最终 `archive-privacy-audit.json` 确认排除项命中 0、敏感字段命中 0、manifest SHA 不匹配 0；未公开身份或配额值。证据 zip 与真实 quota 输出采用仅 Ghost 可读的私人 ACL。

首轮构建与实机场景快照均为 clean c4209d5；`source-after.json` 记录了归档时的 clean 状态，`process-final.json` 记录远端同 SHA/clean、两端 binary SHA 不变且没有测试进程。最终唯一工作区新增文件为本报告（未提交），不改变 build 输入；本轮无生产修复及对应修复提交，不重新运行无关全量测试或 CI。报告完成后仅检查文档引用、残留占位和 Git 差异。

## 10. 同账户确认与第二轮补测（2026-10-03）

用户在首轮结束后确认两台设备使用同一账户，并明确批准新增 Windows LAN sshd、窄入站规则和反向验收。第二轮继续使用首轮两个开发二进制；两端源码均为 c4209d5，Windows 仅本报告新增未提交、trackedDiff=[]，Mac clean。测试时本报告的 dirty snapshot SHA256 为 6528efbc57ffc5391d90523113b57d00c91f62be43ae35a1dca3bf2d18dc7abc；随后只补文档。binary/build ID、protocol 与 effective catalog 不变，独立记录见 supplement/logs/source-before.json 和 supplement/paging/{center,interrupt-center}/logs/provenance.json、supplement/paging/macos-evidence/paging/logs/provenance.json 及 controller-logs/final-*.stdout。

### 10.1 merge-quota、重启与 separate-quota

同账户前置条件由用户明确确认，没有读取 Codex auth 或通过样本自动推断账户。仍使用专用 quota-center/quota-remote，CLI merge/separate 均 exit 0。私有 logs/quota-merge-validation.json 的 10 个检查通过。

| 场景 | 预期 | 实际 | 结果 |
| --- | --- | --- | --- |
| merge-quota | 同账户来源进入账户全局投影；不相加百分比 | local/NODE_ID/all 从本机原始 0 点变为 84 点；三种 scope 的点和历史 weekly 曲线一致，policy=true | 通过 |
| 前台进程重启、重复真实 SSH sync | 用户策略和投影持久化 | recorder clean exit 0、有效 heartbeat；repeat sync exit 0；三种 scope 仍 84 点，曲线相同 | 通过 |
| separate-quota | 退出账户合并但保留原始接收样本 | policy=false，三种 scope 回到原始本机 0 点；SQL 原始远端 84 点均保留且 payload 与批准的原样本相同 | 通过 |

该组本机原始 0 点、远端原始 84 点；没有覆盖两端同时持有重叠原始采样时的冲突选择及跨来源去重。真实旧样本仅位于 Aug29—31，CLI Trends 的 day-offset 实际上限为 7；不能用当前空曲线证明合并失败。行为核对采用当前公开只读 HistoryQuery/Trends API，读取 [2026-08-29T00:00Z,2026-09-01T00:00Z)，as_of=2026-08-31T06:51:49.954397Z（晚于最新观察 1 秒、早于其 reset）。这是历史查询参数，没有改时钟、样本或 SQL。helper 确认既有 SQLite V2Active，仅输出白名单；源文件 query_quota.rs、实际链接命令和 rlib/源码/EXE hash 在 logs/helper-build.json。

保留了一次无效比较器要求的失败：把合并投影和原始样本要求字节完全相等，13 个 reset 值相差 1 秒。当前合并规则按 120 秒漂移将 reset 聚类锚定到最早 reset；所有百分比、observedAt 和其它字段保持，存储 raw payload 仍完全相同。logs/quota-projection-semantic-check.json 全通过，原失败及字段差异也保留，未编辑 DB 修复结果。

### 10.2 多页 continuation、提交页中断和 journal 丢失恢复

本批采用独立根：Windows supplement/paging/center（node-f252473a1b843dadc22cf9eaa4bea431，profile0f2c06f5198bc4d1）、interrupt-center（node-b9191d1df95e1d808435e8ebe0e3e8d9，profile4bb03ed95165ae33）；Mac /Users/user/sqlite-real-machine-2026-10-03/paging（node-f01da6ea5b988f42e1c4cb1788556932，profilec6f83134b966d1ea）。各自四目录/身份独立，没有操作首轮主数据或正式 recorder。

实际基础参数为 --offline --redact-content --days 30 --max-files 500 --codex-home ROOT/codex --trace-log ROOT/logs/NAME.trace.jsonl；remote add/pair/test/sync 指向 local-mac 的该根无 banner launcher。报告为 summary --range 30d --grain 1h --format json --source local|NODE_ID|all；完整 argv/环境、UTC、exit 在各根 logs/*.command.json。

受控远端 128 会话×29 闭合 UTC 日（Sep03—Oct01），3712 次明确 default tier 的精确非零 input1/output1 调用，共 7424 tokens。中心独有 2 tokens，All 预期 7426。正常 exporter 形成 6496 个变更（2784 bucket+3712 digest）；compact entries 45,850,268 B，journal state 53,682,804 B，低于 128MiB 容量且 retentionFloor=0。在真实 8MiB durable 页和每轮四页限制下实际触发六页，没有放宽预算。

| 场景 | 预期 | 实际 | 结果 |
| --- | --- | --- | --- |
| 多页首次 bootstrap | 不完整候选不发布；同命令续跑后激活 | 首轮 exit2/continuation/pages4，cursor15294500242225372248:4310，candidate4310行、readyToActivate=false、无active。exact missing exit1/数值0，All仅local2。续两页 exit0，sequence6496、同candidate激活，exact7424/local2/All7426；重复不增 | 通过 |
| 已提交第一页后的中断 | SQL已提交页和cursor保留，恢复继续，不提前激活 | 第2次export前仅私人launcher gate等待；marker出现时SQL已提交934候选bucket、cursor:934。只终止本次sync（exit1），generation/cursor/exactRange/934行前后保持，无active。解除gate后四页continuation至5799，再一页complete至6496并激活；重复保持7426 | 通过，限定命中阶段 |
| exporter journal整体丢失 | 真实CursorExpired/GenerationMismatch触发替换bootstrap；旧active保留 | 全部测试写者退出后，仅将私人redacted exporter namespace（state+anchor）整体rename到同根备份。下次真实SSH exit2/bootstrap-restarted/pages0，cursorless新候选，旧active仍可读7424。再四页continuation+两页complete，new generation15478855123695708381:6496激活，业务数值不增 | 通过，限定journal丢失恢复 |

稳定阶段 exact API min=max 58,464,000,000 pico-USD，All 58,479,750,000 pico-USD；逐受控事件按实际 CLI window 复算一致。reports exit2 的原因仅 range_starts_within_15m_bucket，sourcePartial=false；不把该报告称作整个30d历史完整。本批没有复制会话，facts=not-needed，不替代首轮完整C/proof证据。

supplement/paging/validation-result.json：40/40检查通过，52份实际 Windows CLI/recorder metadata；16份中心独立trace包含27个成功SSH exchange，被终止sync留下1个未完成span，未伪造outcome。新root所有前台recorder exit0；最终各测试进程为空。未命中SQL事务内部、最终页commit到activation之间的窄窗口和自然35d retention expiry，这些仍未覆盖。

关键 UTC：首次多页 19:52:36.118694—19:53:40.366348 exit2，续跑19:53:50.526261—19:54:12.233761 exit0；中断19:55:01.052639—19:55:13.646234 exit1，恢复19:55:17.239331—19:55:48.526469（2→0）；journal丢失19:56:54.921077—19:57:01.279603 exit2/bootstrap-restarted，恢复19:57:12.239540—19:58:29.680565（2→0），重复19:58:39.869924—19:58:52.772948 exit0。所有日期均2026-10-02 UTC，即当地10月3日。

### 10.3 Windows LAN sshd、直接 CMD 调用与双向配额验收

用户明确批准新增这台 Windows 的 OpenSSH Server 和窄 LAN 入站规则。本轮新增的是 SSH 服务；正式 Codex CLI/recorder 安装及系统服务仍没有修改。安装前 sshd/capability/config/规则不存在，见 supplement/logs/sshd-before.*；安装由系统 Add-WindowsCapability 完成，RestartNeeded=false，没有重启机器。sshd 为 LocalSystem、手动启动，当前 Running，仅监听 192.168.100.20:22。安装默认的 OpenSSH-Server-In-TCP 宽规则已禁用；新 CodexAcceptance-SSH-LAN 规则同时约束 TCP22、LocalAddress192.168.100.20、RemoteAddress192.168.100.78、以太网接口、sshd.exe 和 sshd 服务。网络分类保持 Public，DefaultShell 未设置，公钥登录 Ghost；PasswordAuthentication=no，agent/TCP forwarding 均关闭。

配置、administrator authorized_keys 和选用的 Ed25519 主机私钥为正常 SYSTEM/Administrators protected ACL、owner=Administrators。仅复制 local-mac 的公钥，没有手工读取或复制其私钥；SSH 使用原有凭据正常签名。Windows 主机公钥经既有受信 Windows→local-mac 链路写入专用 known_hosts，StrictHostKeyChecking=yes，没有接受未核验的新主机密钥。公钥指纹：

- Mac 登录公钥：SHA256:dgP0xvK8aM9RDvNwkNXA50yBKYIP2FJ+SxCrPBb4+VI。
- Windows SSH host Ed25519：SHA256:3UqgD5DbXKUfY1uaR2yYcBhskq/KeeTMRbgfi47e3xE。

实际 Mac→Windows whoami /user 的 SID 为 S-1-5-21-455578651-527547421-1392258901-1001（Ghost）；whoami /groups 的完整性 SID 为 S-1-16-12288（高），没有拿桌面进程身份代替 SSH token 检查。私有客户端文件为 /Users/user/sqlite-real-machine-2026-10-03/reverse-client/ssh_config、known_hosts、bin/ssh；wrapper 只 exec /usr/bin/ssh -F 私有配置，诊断日志另用 -E 写文件，没有协议 stdout banner 或 stderr 合并。

初始安装的 Start-Service 失败，服务 exit1067。保留失败诊断后发现：管理员前台 sshd 能监听，新生成的选用主机私钥还带 Ghost ACE。仅把本次生成的该 key 调整为上述服务 ACL 后，Start-Service 成功；可复现配置脚本已补上这一步。没有读取私钥内容、绕过权限 guard 或停止正式服务。证据见 logs/sshd-install-and-repair.json、sshd-start-failure-*、host-key-acl-before.json、sshd-configured-validation.json。

初始 Windows 绝对 launcher 路径使当前 remote_executable_command 采用 PowerShell -EncodedCommand。第一次 discovery 与另一次 probe 实际 SSH255，各 CLI exit1；两次失败均保留。直接重放 discovery、后续相同配置 remote test 曾成功0，尚未确定间歇失败原因。用户随后报告杀毒告警（操作结果“已允许”）；其编码内容解码仅为执行本测试 launcher remote-agent info 和返回退出码，没有 auth 读取。告警与 SSH255 的因果关系未证实，不能把它包装成已定位的产品缺陷。

为回应用户更易审阅调用的要求，在已验证的 SSH cwd C:\Users\Ghost 新增仅 Ghost 访问的独立测试 launcher：C:\Users\Ghost\codex-usage-acceptance-20261003.cmd。它仍设置 quota-center 的四个绝对目录、相同开发二进制及 --offline --redact-content，原样转发 %* 和标准流，不输出 banner。通过正常 remote edit 把该私人来源的 agent-executable 改为 shell-safe token codex-usage-acceptance-20261003.cmd，系统默认 cmd.exe 直接执行它；没有改全局 PATH/默认 shell/杀毒策略，没有更新正式安装。当前源码的 Windows 绝对路径 EncodedCommand 分支仍存在；本轮不是该通用传输实现的修复或全面验收。

实际配置/复测命令（在 Mac 四目录与私有 PATH 下）：

```sh
BIN=/Users/user/sqlite-real-machine-2026-10-03/bin/codex-usage-monit
"$BIN" --offline --redact-content remote edit acceptance-win \
  --agent-executable codex-usage-acceptance-20261003.cmd
"$BIN" --offline --redact-content remote test acceptance-win
"$BIN" --offline --redact-content remote sync acceptance-win
# 首次 sync 建立 SourceMetadata 后才运行
"$BIN" --offline --redact-content remote source merge-quota \
  node-11ee9aca5abdd1adece88b929d96ba50
# 私有客户端的直接诊断调用，不使用编码 PowerShell
/usr/bin/ssh -F /Users/user/sqlite-real-machine-2026-10-03/reverse-client/ssh_config \
  acceptance-win-lan 'codex-usage-acceptance-20261003.cmd remote-agent info --sha256'
```

CODEX_HOME=/Users/user/sqlite-real-machine-2026-10-03/quota-remote/codex、STATE/CONFIG/CACHE 分别为该 root 的 state/config/cache；private PATH 前缀为 reverse-client/bin。实际逐命令 argv 和环境在 reverse-macos-evidence/quota-remote/logs/*.command.json，client DEBUGLOG 的 Sending command 索引在 logs/ssh-direct-command-index.json：最终批次 10 次直接 launcher 调用，没有 EncodedCommand。root scripts 的 outer exit0 不作为 CLI 成功；r4 每步都核对实际 command metadata、fresh startedAt 和 recorder PID/heartbeat。

两端继续使用 quota-center/quota-remote，独立身份如下：Windows node-11ee9aca5abdd1adece88b929d96ba50 / profile89b61a4c75c740db；Mac node-5c2b5f79e85288e0740bff78cbf0369f / profilece91926e27d9c4f3。source generation 均1，binary SHA/build/protocol 与第2节一致；Mac 主动接收 Windows 时角色反转，但没有复制身份或 state-root。

| 场景 | 预期 | 实际 | 结果 |
| --- | --- | --- | --- |
| Mac真实SSH登录、add/pair/test | 验证端口、公钥及实际agent身份 | add/pair exit0；直接CMD test exit0；remote-agent info --sha256 匹配 Windows binary/build/protocol，probe catalog 与第2节一致 | 通过，最终直接调用路径 |
| 反向非空bootstrap和facts跟进 | Mac收到Windows本机用量，SQL发布来源；共同事件只计一次 | 3次sync均exit0/pages1/complete，facts依次local-activated→remote-activated→not-needed；Win local2500/exactMac3900/All4900，Mac local3900/exactWin2500/All4900；两端shared1500只计一次 | 通过，观察用量去重 |
| 双向配额不循环转发 | exporter只导出本机原始quota，不能导出账户合并投影 | 先明确开启Windows merge，使本机投影有84个导入点，但本机raw仍0；反向bootstrap后Mac native仍84、importWin为0；随后Win→Mac→Win重复与Mac recorder重启后均不增。Windows retained importMac84原始payload保持 | 通过，含非空合并投影挑战 |
| 策略与重启 | 同账户策略持久化；接收0不影响Mac自有84 | Mac bootstrap后merge exit0，foreground recorder exit0且当前PID/heartbeat有效；postrestart sync exit0，原始quota与接收状态保持。Windows最后separate exit0恢复默认策略 | 通过 |
| 反向更广coverage | 缺少整日精确覆盖不能升格完整 | shared两端active facts与token fingerprint一致，但validatedDigest coverageComplete=false，项目fingerprint不同；reports exit2仍有model/project/open bucket等partial | 通过正确降级；不证明完整C/全天coverage |

关键 UTC（2026-10-02，即当地10月3日）：direct-edit 20:24:12.074836—12.091257 exit0；direct-test 20:24:12.121295—12.961500 exit0；r4test 20:24:26.050644—26.827374 exit0；首次3轮sync 20:24:26.888984—30.363405 均exit0；Mac merge20:24:30.439971—30.447907 exit0；Mac roundtrip20:24:34.426220—35.043455 exit0；recorder重启20:24:35.089379—36.380164 exit0；postrestart20:24:36.450359—37.069328 exit0。Windows对应完整时间、argv、exit在 supplement/logs/reverse-r4-*.command.json；同窗口Summary/Trends对照、SQL、SourceMetadata/游标/facts记录在两个 quota 根的 supplement-reverse-* 文件。

独立文件只读比较器 compare_reverse.py 退出0，logs/reverse-validation.json 的142/142检查通过：两端schema2/integrity、原始/接收配额、报告数值、激活游标、重启策略和r4实际CLI状态均核对；r4协议trace两向共8次成功、失败0。验证结果 SHA256为0ea104a7d68e1ffa3680c0862c4e65beac23c42a0fbcc3fd3a24c528b47779e4。首轮比较器139/142的三个误判也保存：exact Trends weekly缺失仍应标partial，以及Windows路径分隔符需归一化；仅修比较器，不改业务证据。

一次 harness 顺序错误也保留：r2在bootstrap前尝试merge，exit1/source metadata is missing。随后调整为先sync建立来源、再merge，未改DB或生产逻辑。反向验证、重启和重复完成后两端无测试CLI/recorder/exporter残留；新获授权的手动启动sshd仍Running以供LAN使用。重启Windows后需要管理员 Start-Service sshd；本轮没有把它设为自动启动。

### 10.4 补测后的边界

本次补齐了same-account merge/separate、真实SSH多页continuation与候选激活、已提交页后的中断恢复、exporter journal丢失触发bootstrap-restarted，以及双向quota不转发。以下仍未覆盖：SQL事务内部中断、最终页commit到activation间的窄窗口、自然35d retention expiry、两端重叠原始quota的冲突选择、在线账户API采样、正式recorder系统服务部署/升级、真实用户历史性能和断电恢复。首次绝对Windows路径的编码PowerShell调用有间歇SSH255与用户报告告警，其环境原因未确定；最终直接CMD路径的通过不抹去该失败记录。

没有生产源码修复、修复提交、测试tag或新CI。独立helper/harness修正只在忽略target；实际生产快照和第2节开发二进制保持。证据分为首轮与补测两包，首轮SHA256不变。

### 10.5 补充证据归档

独立补充包（首轮包不覆盖）：

`D:\Workspace\codex-usage-monit\target\sqlite-real-machine-2026-10-03\supplement\acceptance-supplement-evidence.zip`。

- SHA256：`5a3aa45d5e5410554304bc4ca1acd2191d9643a52fa86ac5771c046c16d654d9`；2,894,204 字节，1080 个输入文件加 manifest/privacy audit，共1082个成员。
- 包含40份 quota-center补充日志、166个Mac反向文件，以及CLI元数据、只读SQL导出、真实SSH脱敏trace、受控多页fixture、独立helper/比较器、首次失败及修正前比较器、LAN配置/公钥与直接CMD launcher；没有真实身份秘密、auth、private key、全部state/config/cache子树、现场DB或二进制。
- `supplement-evidence-manifest.json` 逐文件大小/hash；`supplement-privacy-audit.json` 和 `supplement-bundle-result.json` 确认成员路径、秘密/私钥/API-key字段、二进制/SQLite magic、CRC与全部manifest hash复查通过。普通日志中的惰性路径引用允许保留，不等于把所指文件打包。
- 三个Windows原生编码stdout仅按具体文件白名单用CP936做扫描，原始bytes/hash保留；FIDO公共算法名误报只排除四个精确公共名，真实API-key规则不放宽；预扫误报记录在logs/bundle-preflight-review.json。输出包和伴随JSON使用仅Ghost可读的私人ACL。
- 首轮包仍为1,099,760字节、SHA256 `0afaf67abc219c277c0842234b62baae631d07169b05d8f717a0740797c4addb`；打包前后独立核对未变。

`logs/source-process-after.json`记录两端c4209d5、Windows仅本报告dirty且trackedDiff=[]、Mac clean、两端binary hash不变和无遗留测试CLI/recorder/exporter；`logs/sshd-final-status.json`于2026-10-02T20:32:36.0582693Z确认新获授权sshd仍Running/Manual、仅192.168.100.20:22。本报告的最后包摘要在打包后追加，未改任何测试输入；最终链接/状态检查记录单独保存在supplement/report-final-review.json。target证据与C:\Users\Ghost私有launcher均不随Git自动同步。

## 11. 后续主要实现审查（2026-10-03）

按用户补充请求，重新审查当前 `c4209d5cf27f6b4aee89da7e1bc8b58790153786` 的数据库、初始化、必要状态保留、CLI/TUI/recorder 接入和远端 aggregate/facts 状态机，依据存储方案第1—6节及7.3。tracked 源码未修改；dirty 仍只有本验收文档。本节在既有证据包形成后追加，两个包的内容和SHA256保持；旧 `report-final-review.json` 的文档哈希绑定追加前版本。本节的定向探针与审查笔记另存 `target/review-sqlite-2026-10-03`，不将它们算作真实SSH或完整TUI验收。

| 项目 | 预期 | 实际与证据 | 结论 |
| --- | --- | --- | --- |
| 双privacy初始化恢复时另一namespace的ownership丢失 | 已有profile数据库时，manifest及anchor丢失必须拒绝重造epoch | 当前namespace为Migrating、SQL receipts已提交，另一namespace的manifest与anchor同时丢失后，`sqlite_history_initialization.rs:77`直接初始化另一namespace，重造epoch2并匹配旧receipt。新建私有空库的正常恢复exit0；仅丢manifest控制exit2；两文件同时丢失却exit0并进入V2Active。SQL控制内容逐项不变，没有业务历史被覆盖或补造。`ownership-recovery-20261002T204453Z/result.json`、`review-provenance.json`记录公共runtime API、故障阶段、只读SQL、命令和哈希 | **失败，P2已复现**；已有数据库拒绝检查只覆盖当前runtime namespace，缺少另一namespace组合回归 |
| facts发布后的TUI缓存版本 | facts/proof激活改变查询输入时，使旧projection失效 | `sqlite_publish_fact_batch`发布active manifest/游标而不推进query-visible版本；`HistoryProjectionRevision`（`history_application.rs:719`）未纳入facts版本。全新隔离根通过公开fenced API激活Local及SSH各1条10-token fact，均activated=true/cleanupPending=false，SQL记录与游标1:1可见，但完整缓存版本输入相同。`facts-cache-proof.json`及`facts-cache-proof-execution.json`记录2026-10-02T20:50:41Z—20:50:50Z/exit0 | **版本失效遗漏已确认，P2**；真实TUI陈旧显示未重放。最小探针没有remote aggregate active ref。代码推导的触发为外部CLI/recorder在aggregate提交后跟进facts，TUI恰在两阶段之间缓存；旧用量、金额或partial可保持到30秒TTL。TUI自身remote操作完成会强制刷新，不能用同一推断覆盖该路径 |
| GC工作单元及写锁边界 | 沿用累计资源预算，写事务短，长解码在事务外 | `source_history.rs:1713`把整profile的namespace枚举、JSON解码、逐条删除及全部来源facts GC放进同一个BEGIN IMMEDIATE；budget在每namespace及每facts generation重建。`sqlite_evidence.rs:969`起读取完整active facts并在同一外层事务复制保留代。nested write仅savepoint，不释放writer。250ms busy只限制等锁时间 | **静态资源契约缺口，P2**；缺少整轮累计预算、分批进度和持锁边界回归。未测实际时延，不宣称已卡死或丢数据 |
| WAL/SHM稳定对象身份 | 主库及侧文件在打开与使用期间拒绝对象替换 | `database.rs:592`只复查侧文件当前路径的权限、类型和链接数；主库identity/SQLite main handle检查未绑定侧文件对象。实际bundled Unix VFS的SHM有O_NOFOLLOW，但未找到已打开WAL/SHM与当前pathname的同一性复核。现有替换测试只覆盖主库 | **静态安全契约缺口**；需要原生Unix受控侧文件替换/丢失回归及身份fence。未复现损坏，Windows打开对象的删除/重命名限制须分别验证 |

ownership复现只在新建可丢弃fixture中，在API进程退出后改变该fixture的manifest阶段并移除另一namespace的两文件；没有编辑SQL，也没有触碰正式目录或先前验收数据库。它模拟other先Active、current尚Migrating的可达activation顺序，不代表发生过真实现场数据丢失。复现脚本exit0表示已复现预期失败，不表示产品契约通过；其首次tuple/list断言错误保留为harness-failure，未进入故障注入。

facts探针仅证明激活事实已进入SQLite而版本输入不变；没有通过数据库编辑、伪造CLI通过结果或实际SSH来构造它。helper源码SHA256为`87f5a44b0925bccbf93bc599b752deb8507342af36680ea715554cc208dc1685`，二进制SHA256为`5a3d5d3cae9af36ceadf6481e136e0ad970908be33862785127d3a2afdf6d435`；两个探针链接当前开发库SHA256 `da118451f665fdacf16737813e5bbc965087877495d60abd7a836b614cfb8062`。ownership结果SHA256为`63e316b0cca72baee352723e3af5ca0516ddf2febdfde213a4c1002139c982ef`，facts结果为`c11468df62d7ac3cc4bd059e230ec53cd2616bf66bb430f57a9ce1d26aba24ac`。native Windows 11 build26200/x64，Rust1.97.0；完整argv、时间、helper源码及结果在各自执行记录。

未发现旧JSON生产历史回退、旧派生历史重新导入、配额导出读取合并投影或CLI/TUI/recorder漏接SQLite的额外入口。receipt复核的低层writer边界依赖调用方启动gate；未找到正常生产操作删除receipt的路径，因此不凭外部任意删receipt单独认定缺陷。上述新发现不改变前面各实机样本的实际通过记录，但当前实现仍有待修复边界；事务内部/最终activation窗口、自然35天到期、断电及性能等未覆盖项继续按10.4列明。

本轮未修复生产源码、未提交、未新增全量测试或CI、未创建tag；没有变更正式服务、SSH配置、杀毒策略、原始日志或auth。后续最小修复及定向回归应重新绑定源码快照，并遵守仓库本地优先和检查点CI规则。细节见`boundaries-review.zh-CN.md`、`sync-review.zh-CN.md`及`gc-integration-review.zh-CN.md`；target证据不随Git同步。

## 12. 审查问题修复与重新验证（2026-10-03）

用户在第11节审查后明确要求修复。以下证据对应 `c4209d5cf27f6b4aee89da7e1bc8b58790153786` 上的修复快照；第1—10节仍绑定原实现，不把它们算作修复后代码的实机验收。修复证据根为 Windows `D:/Workspace/codex-usage-monit/target/sqlite-review-fixes-2026-10-03`、Mac `/Users/user/sqlite-review-fixes-2026-10-03`，均为私人测试目录，不同步正式安装、服务或 auth。

### 12.1 已完成的定向回归

| 修复 | 预期及实际 | 结果与证据 |
| --- | --- | --- |
| 另一 privacy 的初始化所有权丢失 | 当前 namespace 正在 Migrating、SQL receipt 已提交时，另一 namespace 的 manifest 与 anchor 同时丢失必须拒绝重造。新组合回归修复前 selected1/Cargo101；修复后初始化模块8/8，正常空库、单文件丢失、配额与控制状态保留的邻近用例也通过 | **定向通过**；`ownership/{before-fix,after-fix}.{json,stdout,stderr}`。仅修复 SQLite 初始化分支，不恢复旧派生历史导入 |
| 外部 facts/proof 发布使 TUI 缓存失效 | 引入 profile 共享的 `facts-query-publication.json` SQL stamp；active manifest、proof、cursor 和 stamp 同事务发布。Local30/Remote40/common10 的完整 union 为60，区别于任一权威来源。真实 TuiHistoryStore 已缓存后，Local、Remote、同 cursor 的 proof-only 发布均使缓存失效；no-op 不推进 stamp；故障回滚保留所有旧状态，正常重试只推进一次 | **定向通过**；新模块2/2，修复前真实缓存断言失败，修复后 `facts-cache-fixed-v3.*` exit0、源码/build 输入前后稳定。临时 trigger 仅在已验证 fixture 的 scoped connection 内模拟事务故障，不更改生产 schema |
| Unix WAL/SHM 实际对象身份 | 通过公开 FILESTAT / JOURNAL_POINTER 获取 SQLite 实际 main/WAL/SHM fd，借用 fd 做 fstat，不 open/dup/close。WAL、SHM 各覆盖移除及同权限替换，拒绝嵌套访问、回滚未提交值、保留已提交值。原生 Mac 新回归修复前 Cargo101；修复后数据库模块12/12、跨进程 writer 锁回归通过 | **定向通过**；`wal-baseline-evidence` 与 `wal-fixed-v2-evidence`。修复快照 archive SHA256 `e70d022e34562f9bb6e913ca770ffb425cc9e0e38569e461cf85d8bfdfe0b54c`，database.rs SHA256 `29c0d9b693d7558639632e8abe551402915241e6e6528a959d1ae938ce65cbde` |
| Windows WAL/SHM 生命周期 | preopen side guards 不共享 DELETE；实际 WAL HANDLE 与 guard FileID 对照，SQLite SHM 本身也不共享 DELETE。打开期间 rename/remove 均拒绝；先释放 guard 再关闭 SQLite，正常 writer-close 可清理侧文件；读回已提交值不丢失 | **定向通过**；数据库模块8/8，`windows-database-fixed.*` exit0、源码/build 输入前后稳定。最初测试把 read-only 重开后可能残留的辅助文件误判为 writer-close 失败；已保留失败日志并改为检查真正的 writer-close 边界 |
| FILESTAT 构建与快照 | 默认 tracked `.cargo/config.toml` 启用 `SQLITE_ENABLE_FILESTAT`，build ID 纳入该配置；Docker/UTM 源码快照包含产品配置，机器私有配置继续排除。新 Linux snapshot 回归修复前失败、修复后1/1；Windows runner 契约10/10 | **定向通过**；`build-wiring/final-summary.json`。SQLite 不在 compileoption_used 列出 FILESTAT，构建探测改为直接调用所需公开 API；旧失败作为测试诊断保留 |
| GC 累计预算、锁及恢复 | 每生产调用共享4096条/128MiB输入预算，长业务解码在 SQL writer 外，短事务以进度、payload、来源及 manifest CAS 发布。SQLite 保存续跑进度；facts candidate 分页、完整核对后激活，旧代分页回收。七项确定性回归验证跨 namespace 预算、重启、候选不可见、准备时另一 writer 可提交、旧页回滚、另一 privacy 删除后不能复活来源、满额元数据与有界日统计 | **定向通过**；`gc-focused-final.log` selected7/Cargo0、`gc-focused-source-binding.json`；删除 unused helper 后 strict Clippy0。不是吞吐、峰值内存或锁时延基准 |

Mac 数据库定向完整命令为 `cargo test --locked --lib source_history::database::tests:: -- --test-threads=1 --nocapture`，Darwin arm64/macOS15.7.2、Rust1.97.0，2026-10-02T21:34:26.447259Z—21:34:43.001461Z/Cargo0。Windows 对应 `cargo test --locked --offline --lib source_history::database::tests:: -- --test-threads=1`，Windows11 x64、Rust1.97.0，21:39:51.798360Z—21:41:06.923358Z/Cargo0，Ghost 私有 TEMP。facts 新模块命令/平台及全源码哈希见 `facts-cache-fixed-v3.execution.json`，21:44:38—21:47:33 UTC。所有时间为2026-10-02 UTC，即当地10月3日。

Unix FILESTAT 不可用时明确拒绝打开历史，不回退到仅检查当前路径。外部自定义 `LIBSQLITE3_FLAGS` 必须包含该功能；配置不 force 覆盖用户环境。机器定制可使用用户 Cargo 配置、环境或显式 `cargo --config .cargo/config.local.toml`，不自动加载 config.local.toml。

Windows 原生5.1的最小启动正常，版本5.1.26100.9444。注释解析测试最初继承PowerShell7的模块路径而找不到 Get-FileHash；只给测试子进程设置原生 PSModulePath 后，5.1/7均2272个非注释token与原脚本一致、解析错误0。没有 EncodedCommand、ExecutionPolicy 覆盖或系统环境修改。诊断在 `powershell-startup/diagnosis-summary.json`；早先0xffffffff退出未稳定复现，未归因杀毒软件。

### 12.2 完整检查中发现的夹具与执行环境问题

第一轮完整修复检查 `batch-v4` 的 Mac、Linux 各有28项失败：26项旧夹具直接写 SQL、绕过 ownership 初始化；一项 GC 缓存测试假定单次调用完成整轮；一项 Unix 超时夹具失去 Git 可执行权限。保留 guard 与业务断言，仅让普通查询夹具先初始化、purge 在授权 writer 内建立 ingest，并让 GC 缓存测试最多64单元续跑至本轮完成。两项专门检验 Uninitialized 的用例仍使用未初始化夹具。归档按 Git mode 恢复权限，未改超时用例生产逻辑。

原生 Windows 完整检查 `windows-full-batch-v4` 实际1713 passed / 28 failed / 1 ignored，Cargo101、outer PowerShell1，源输入前后稳定；format、Clippy、安装/Python及PowerShell5.1/7契约通过。失败中的26项初始化夹具和一项 GC 与上述一致；另一项是 `windows_remote_invocation_runs_under_cmd_and_both_powershells`。私有冻结 libtest 单项复测仍失败，用户随即提供杀毒软件“已阻止”的精确进程链：该测试二进制→cmd→powershell `-EncodedCommand`，内容只调用 `tools\agent.exe /d /c 'echo literal-ready'`。这证明本次复测命中了编码调用拦截，不将它归为 SQL 缺陷或仅凭猜测归因整轮负载。停止所有编码探测，没有关闭防护、增加白名单或改执行策略。日志在 `transport-v4-probe/isolated.*`；旧完整失败不计入最终通过结果。

夹具修正后的 `batch-v5` archive SHA256 为 `361109c4944a0db32c8d11de8c0da1991e35cf874d74ec58a5490a8ea224f87f`。Mac 原生整组 `history_query::tests::` 53/53、purge exact1/1、Unix timeout exact1/1均Cargo0、sourceStable=true，UTC22:26:46—22:27:37；完整命令及逐文件哈希见 `batch-v5-{query,purge,timeout}-evidence/result.json`。Windows GC cache 单项1/1、Cargo0，UTC22:26:43—22:28:17，源输入前后稳定，见 `gc-cache-v5-focused.execution.json`。GC原删除、缓存失效和重复 pass 断言均保留。

`batch-v5` 的完整 Mac 与 Docker Linux arm64 检查也通过，分别1918与1915项 Rust passed、0 failed、3 ignored，含原生PTY2项；单独gallery重跑和跨进程测试内部子进程的selected1输出不重复计数。format、strict Clippy、Python80（6项Windows专属跳过）/runner10、安装及offline CLI smoke均通过。Mac UTC22:29:23—22:31:38、Linux22:29:25—22:31:49，outer及内层verify真实exit0，源输入稳定。Linux的实际guest/aarch64结果见 `batch-v5-linux-evidence/result.json`，Docker snapshot SHA256 `da324523c5f8e01083a88caf524ecd4ce76c5ebdcb57cf2639745c20fe1c1d16`；完整日志 `/Volumes/File/codex-usage-monit-docker-build/runs/20261002T222926Z-arm64-87847/verify.log`。Mac见 `batch-v5-native-evidence`。这两个结果绑定SQLite修复，尚不覆盖其后为编码调用拦截追加的transport修改。

### 12.3 Windows 远端调用与防护拦截

用户报告的精确进程链和失败测试保留在12.2。生产 Windows 原生路径改为明文、唯一原生命令调用，例如：

```text
powershell.exe -NoProfile -NonInteractive -Command "& 'C:\Users\O''Brien 用户\App Data\codex-usage-monit.exe' 'remote-agent' 'export'"
```

路径与每个参数分别作 PowerShell 单引号 literal；在启动前拒绝外层 shell 会扩展的 `$`、反引号、`%`、`!`、双引号、`^`、控制字符及 U+2018—U+201F 引号。原生路径的 `.cmd/.bat` 与空参数也拒绝；安全的 PATH launcher 名仍可直接使用。含空格、中文、ASCII 单引号的 `.exe` 在原生 cmd、PowerShell5.1、PowerShell7中执行成功。15次实际调用均采用明文；危险输入只检查启动前拒绝，不执行注入探测。没有使用 ExecutionPolicy 绕过、白名单或关闭防护。

PowerShell 的唯一原生命令会把原生 exit2 归一为 exit1。manager 仅在明确 Windows wrapper、真实 SSH exit1且严格校验的 apply-report 明确 `outcome=partial` 时按 partial处理；身份、scope、CLI status、heartbeat 校验保留。其余错误仍拒绝。11项 manager 回归和原生三种 shell 的退出码检查通过，见 `transport-plaintext-*`。

首次 v6完整检查仍遗漏了 `tests/agent_management.rs` 中旧的内联编码调用：库1742项通过后，该集成测试被阻止、Cargo101。这个失败没有被改写或计入通过。测试随后改为匹配明文生产调用，原 checksum、immutable install、不创建 recorder/生产state等断言保留；v7重跑全部集成测试通过。

安装管理的独立 bootstrap trampoline 仍使用编码 PowerShell，本次没有扩大改造。Windows实际测试 `remote_agent_manager::tests::official_bootstrap_runs_over_stdin_in_both_windows_shells` 明确跳过；相关自动安装路径本轮未验收。全仓可执行入口审计及未覆盖边界在 `encoded-entrypoint-inventory-v7.json`。明文提高调用可审阅性，不能保证任何杀毒软件都不会告警。

### 12.4 最终修复快照的本地验证

最终不可变源码包为 `batch-v7.tar.gz`，SHA256 `875b1c76ebcefbfe026192d80a57519731a2e7a07b28c214de6c8371f04ce25b`；manifest记录基线 SHA、dirty 状态、逐文件本机及 LF 归一化哈希、Git mode。Mac/Docker 测试隔离解压快照，正式 Mac checkout 不修改。Windows检查 checkout，运行前后逐文件稳定；没有把较早的绿灯直接算入后来生产输入。

| 平台与实际命令 | 实际结果 | 时间与日志 |
| --- | --- | --- |
| macOS15.7.2 / aarch64，隔离快照 `sh scripts/verify-unix.sh` | Rust **1920 passed / 0 failed / 3 ignored**；format、strict Clippy、Python/runner、安装及 offline smoke通过；真实verify exit0、sourceStable=true | 2026-10-02 UTC22:59:14—23:01:44；`v7-completion-download/batch-v7-native-evidence/{result.json,stdout.log,stderr.log}` |
| Docker原生Linux / aarch64，隔离快照 `sh scripts/test-linux-docker.sh` | Rust **1917 passed / 0 failed / 3 ignored**；相关完整检查通过；真实runner/verify exit0、sourceStable=true | UTC22:59:15—23:01:40；`v7-completion-download/batch-v7-linux-evidence`；完整guest日志 `/Volumes/File/codex-usage-monit-docker-build/runs/20261002T225916Z-arm64-18544/verify.log` |
| Windows11 build26200 / x64，v6 `scripts/windows/verify.ps1 -ScriptContractsOnly`；`cargo test --locked --offline --all-targets -- --skip remote_agent_manager::tests::official_bootstrap_runs_over_stdin_in_both_windows_shells`；v7 `cargo test --locked --offline --test '*'`、`scripts/windows/verify.ps1 -SkipTests` | 核对后的分组件证据 **1870 passed / 0 failed / 4 existing ignored / 1 explicit filtered**：v6库1742，v7全部集成128含实际ConPTY2；PowerShell5.1/7及脚本契约、format、all-target strict Clippy、build、version/offline JSON smoke通过 | v6从UTC22:42:02开始，原整条Cargo101保留；v7恢复从22:59:07开始；`windows-native-reconciled-v7-summary.json`、两份execution及stdout/stderr日志 |

Windows不是一次完整 all-target 命令绿灯。`v6-v7-input-reconciliation.json` 证明v6→v7唯一变化为上面的集成夹具；`src/`、Cargo、build.rs、tracked Cargo配置等产品build输入完全不变，最终集成/Clippy均针对v7。完整原始argv、TEMP ACL、工具链、起止时间、退出码和源文件哈希在execution JSON；分组件恢复汇总明确保留v6原失败101及v7恢复0。Mac/Linux的7份下载文件均与远端SHA256一致，见download-manifest。没有主动触发新GitHub CI、push或测试tag；既有CI run37046056597不代表修复后快照。

### 12.5 修复后真实 SSH + SQLite 重新验收

使用全新独立测试 root，四个目录固定在每个 root 的 `codex/state/config/cache`；全部 recorder、CLI、exporter 为 `--offline --redact-content --codex-home <root>/codex`，无auth、无真实日志修改、无state-root/SourceIdentity复制。Windows中心 root为 `D:/Workspace/codex-usage-monit/target/sqlite-review-fixes-2026-10-03/real-ssh/center`；Mac远端 root为 `/Users/user/sqlite-review-fixes-2026-10-03/real-ssh/remote`。角色反转时仍用各自原root。两端私有权限正常，使用 bounded foreground recorder，不注册服务或使用 deploy-dev。

| 身份 | Windows中心 | Mac远端 |
| --- | --- | --- |
| 平台/架构 | Windows11 build26200 / x86_64 | macOS15.7.2 / arm64 |
| 本轮开发二进制绝对路径 | `D:/Workspace/codex-usage-monit/target/sqlite-review-fixes-2026-10-03/real-ssh/center/bin/codex-usage-monit.exe` | `/Users/user/sqlite-review-fixes-2026-10-03/real-ssh/remote/bin/codex-usage-monit` |
| 实际Cargo输出 | `D:/Workspace/codex-usage-monit/target/debug/codex-usage-monit.exe` | `/Users/user/sqlite-review-fixes-2026-10-03/cargo-target/debug/codex-usage-monit` |
| SHA256 | `aa131b9ae4694b505dd273ce24358ae626a2da03bb741e7bba485e2cdcc01f0b` | `def2ca26458fd5abfc8bd1818a4f22f4543031f73493ff5e5da50b31a265e8b1` |
| NODE_ID / generation | `node-31e54b084ffa62e54e6e74ea7a5c0bc6` / 1 | `node-168db45e4a38e03d64647884d8804f36` / 1 |
| profile / SQLite | `7ef99c29f4f78ba0`；root/state/history-v2/7ef99c29f4f78ba0/history.sqlite3 | `c68a03cc2f570636`；root/state/history-v2/c68a03cc2f570636/history.sqlite3 |

两端 `remote-agent info --sha256` 的 build ID一致：`04e6e72985e4fde257a9ac3136e63acad89268908bc345aeaf7e07373c3201a7`；version0.5.2、protocol5、各自target匹配。离线 bundled catalog metricRevision5/estimatorRevision8/apiPricingRevision5，fingerprint `model-catalog-sha256-v1-6427b2d0d1ed3e4b923cb56693e8979a37a7cd11f49dc5766e36209c59b60707`；启动日志与SQL protocol binding交叉核对。SQLite均schema2、applicationId1129663816、integrity_check=ok；只读检查使用mode=ro/query_only，拒绝编辑数据库制造结果。

Mac launcher设置四个本端绝对目录并 `exec ... "$@"`，逐exporter PID单独trace；Windows reverse launcher为独立std-only Rust `.exe`，设置相同四目录后继承stdin/stdout/stderr及转发args_os，真实子进程退出码传播。两者stdout无banner，stderr不合并。两端使用真实system SSH、公钥认证和固定主机key。Mac私有SSH client指定 `-F` 和 `-E` 诊断日志，未改正式SSH配置、DefaultShell、正式recorder或防护策略。

逐命令 metadata及原始stdout/stderr在 Windows `real-ssh/center/logs`、Mac原始root下的 `logs` 以及 `/Users/user/sqlite-review-fixes-2026-10-03/real-ssh/reverse-client/logs`；Mac副本的预定Windows位置为 `real-ssh/macos-logs`，传输审核状态见12.8。私有harness `node-revalidation.py` 保存真正CLI exit；外层Python exit0不替代CLI状态。主要实际命令如下（四目录环境均已设置；BIN为上表绝对路径）：

```text
BIN --offline --redact-content --codex-home ROOT/codex --trace-log LOG remote-agent info --sha256
BIN ... record --foreground --local-interval-seconds 5 --account-interval-seconds 30 --status-file ROOT/state/test-recorder-status.json
# Windows中心：
BIN ... remote add acceptance-mac --ssh-host local-mac --agent-executable /Users/user/sqlite-review-fixes-2026-10-03/real-ssh/remote/launcher
BIN ... remote pair acceptance-mac
BIN ... remote test acceptance-mac
BIN ... remote sync acceptance-mac
# 两端及指定来源的报告：
BIN ... summary --range 7d --grain 1h --format json --source local|NODE_ID|all
BIN ... trends --day-offset 0 --format json --source local|NODE_ID|all
# 私有连接故障及恢复：
BIN ... remote edit acceptance-mac --ssh-host acceptance-no-such-host.invalid
BIN ... remote sync acceptance-mac
BIN ... remote edit acceptance-mac --ssh-host local-mac
BIN ... remote sync acceptance-mac
BIN ... remote source exclude node-168db45e4a38e03d64647884d8804f36
BIN ... remote source include node-168db45e4a38e03d64647884d8804f36
# Mac反向（私有SSH PATH）：
BIN ... remote add acceptance-win --ssh-host acceptance-win-lan --agent-executable D:/Workspace/codex-usage-monit/target/sqlite-review-fixes-2026-10-03/real-ssh/center/launcher.exe
BIN ... remote pair acceptance-win
BIN ... remote test acceptance-win
BIN ... remote sync acceptance-win
```

Summary范围为每次请求的滚动7天，Trends为滚动24小时，比较器按JSON实际startsAt/endsAt和15分钟bucket核对；不能拿两类不同窗口的总数互比。受控样本 UTC2026-10-02T18:50 的base shared1500/local1000/remote2400和远端两次增量600/300均在同18:45桶；另复制96个正值call覆盖UTC10月1日整天，每桶2tokens，共192。两端独立monitor身份，复制的是测试rollout会话，项目绝对路径故意不同。

| 阶段 | 中心local | exact Mac | 中心all | all API equivalent / calls | 结果 |
| --- | ---: | ---: | ---: | --- | --- |
| bootstrap | 2500 | 3900 | 4900 | $0.018340 / 3 | 通过；shared1500只计一次，SQL非空 |
| full-day完成两端facts跟进 | 2692 | 4092 | 5092 | $0.019852 / 99 | 通过闭合日精确事实及去重；不声称整个7d完整 |
| 增量600 | 2692 | 4692 | 5692 | $0.0219695 / 100 | 通过；旧数据保留 |
| TUI增量300 | 2692 | 4992 | 5992 | $0.02302825 / 101 | 自动从5.7K刷新6.0K，CLI/SQL精确5992 |
| exclude（含测试recorder重启） | 2692 | 4992 | 2692 | $0.011382 / 98 | 通过；exact仍保留4992及excluded warning |
| include | 2692 | 4992 | 5992 | $0.02302825 / 101 | 通过；恢复all，SourceMetadata持久化 |

首次full-day sync exit0/aggregate complete但facts=attention。旧候选 coveredThrough=10月1日00:00/coverageComplete=false；facts独立扫描得到闭合日complete=true，严格binding比较失败。此解释来自旧cooldown key、recorder coverage口径、trace与源码的交叉推断，首次具体error文本没有捕获，不包装成直接日志事实。第二轮remote-activated，第三轮not-needed只是公平cursor扫尾；随后summary刷新本地digest revision9，再有限第4轮sync得到 **local-activated**。没有降低exact checks或修改SQL。失败trace SHA256 `c7b8351f…e7f688da`保留；不将其归为GC写锁问题。

`full-day-final.db.json` 中中心Local与ImportedRemote各96条唯一exact facts、各自active manifest/proof/counts/day统计、完整日coverage和token fingerprint相符；proof匹配各端project fingerprint及远端generation/revision binding，`facts-query-publication.json`已推进。两端token fingerprint为 `session-digest-sha256-v1-77479d215811092df91f0f3ed4a736e6ec35937290074b6576703886b2e8916d`。远端exporter实时物化并写自己的文件journal，远端本机SQL未必存自源activefacts；不凭该差异误判未进入SQL或强制复制state。独立audit exit0，52 pass、6 expected partial、0fail；SHA256 `e0f6b6b58f66d67b7651eb377198141dc16fe7dfb03953dc9d60f599869b7b71`。

最终all的 `duplicate_session_dedup_unavailable` 已消失；剩余 `coverage_starts_within_local_bucket`、`duplicate_session_model_breakdown_partial`、`history_bucket_open`、`range_starts_within_15m_bucket`、`replica_project_conflict` 正确保留。整日事件去重有完整精确证据，7d空白、当前bucket、模型分解和不同项目归属不能强行标complete。Trends还无quota/weekly readout，因此报告exit2是partial，不是remote test失败或sync continuation。

重复sync exit0/not-needed，没有新增用量。Windows recorder PID64452于UTC23:25:18启动、23:25:19有效heartbeat，绑定PID/start/build协作停止exit0；重启后sync0、已提交数据及游标保留。故障sync于私有 `.invalid` 主机得到CLI1/SSH255/DNS失败；失败前后SQL业务和进度保留；恢复local-mac后sync0。不要求SQL/WAL字节不变，新0token覆盖bucket与正常revision可以推进。policy restart PID10736正常停止exit0，exclude重启后仍all2692。

原生ConPTY session39222使用 `u/7/q`，TUI PID63700、退出0；原始增量VT捕获在 `tui-{baseline,refresh}-terminal.json`，精确JSON在 `tui.summary.all.stdout`。并发外部sync及report时出现暂态ERR/known0，后自动恢复6.0K；该帧和diagnostics完整保留，不把它算作无错误并发体验，也不声称compact、mouse或全TUI布局验收。

反向真实Mac→Windows的add/pair/test均exit0；四轮非空sync均exit0，facts依次local-activated→remote-activated→local-activated→remote-activated。前三轮数值已5992但Windows来源的96闭合日facts尚未激活，第四轮才使两来源各97facts、四个active manifests可见；完整日96及当前日shared1分别核对，当前日proof仍partial。最终Mac local4992/exactWindows2692/all5992，calls100/98/101，API equivalent也为$0.02302825，导入用量没有再次作为Mac原始来源转发。Mac bounded recorder PID49548正常SIGINT/exit0，重启后sync0/not-needed，业务数值及aggregate cursor sequence100保持。Windows原生路径从真实sshd默认cmd进入明文PowerShell，再执行private launcher.exe；SSH DEBUGLOG的Sending command单独核对，没有EncodedCommand，验证了真实stdin/stdout协议与两端SQL发布，而不只是remote test成功。

TUI诊断显示UTC23:27:40和23:27:58两次查询失败，随后23:27:47和23:28:06恢复；前一次包括 `history database files must have exactly one hard link`，两次都有 `database is locked`。只读audit核对 `validate_file`、250ms busy timeout、TUI错误回落与基线源码相同。缺少出错文件路径、实际link count及DeletePending，不能证明发生了nlinks=0、新sideguard失链或污染。末连接清理与pre-pin窗口是静态候选竞态，尚需确定性验证；没有放宽ownership/ACL/identity检查、盲目增加timeout或据此修生产代码。结果记录为 **recovered-with-transient-ERR**，audit SHA256 `26f1c08b58b1e4b33a160614962d5904e366b290a288342f9e5be362253b22c0`，原21行诊断副本SHA256 `d00ff28771141d37e5f3a90258fe4d328726e86092fa22bb2e4d37c671f892fc`。

### 12.6 修复后覆盖范围与保留限制

| 场景 | 修复后结果 | 证据边界 |
| --- | --- | --- |
| bootstrap、增量、幂等、双向SSH | **通过** | 同build开发二进制、真实两向system SSH、公钥/host key、独立身份、非空SQL、CLI窗口对照和脱敏trace交叉确认 |
| recorder重启、连接失败恢复、exclude/include | **通过** | 实际前台PID/heartbeat/正常停止、SQL业务和aggregate/facts状态保存；故障仅私有host设置，无真实sshd停止 |
| 完整闭合日C去重与facts/proof跟进 | **通过限定样本** | 两端项目不同，因此需要两来源exact facts；中心及反向中心SQL各核96闭合日完整proof，192tokens只计一次；当前日、7d覆盖及项目/模型分解继续partial |
| TUI外部刷新 | **刷新通过，暂态错误保留** | 真实ConPTY5.7K→6.0K；原始ERR帧、错误及恢复诊断保留，无“全程无错误”声明；确定性pre-pin清理竞态尚未验证 |
| 初始化ownership组合、事实发布rollback、累计GC预算/续跑/CAS、WAL/SHM对象身份 | **定向及三平台回归通过** | 第12.1的新可丢弃API/SQL夹具；不是正式历史故障注入、断电或性能基准 |
| 非空真实quota、same-account merge/separate、反向不转发quota | **本次修复快照未重采** | 第10.1/10.3真实84采样证据仅绑定原c420快照；本轮fresh roots raw/import quota为空，不能把零条记录算作非空quota验收；相关完整回归通过 |
| 实际SSH多页、中途kill、journal丢失bootstrap-restarted | **修复后未重新执行** | 第10.2成功仅绑定原快照；本轮aggregate皆单页，首次facts attention有限续跑另有证据，不混作aggregate continuation |
| schema1/丢库/损坏及旧文件迁移CLI再验收 | **修复后未逐场景重放** | 保留原验收和受影响初始化组合回归；旧派生历史不恢复、不引入旧生产backend |
| Windows自动安装bootstrap、正式服务升级、在线quota API、x64 Linux、35d自然过期、SQL事务/activation窄窗口、断电及真实性能 | **未覆盖** | 明确编码bootstrap skip1、无正式部署或性能基准；不把构建、旧绿灯或模拟测试替代实机证据 |

本轮没有修改正式安装、Codex auth、原始日志、默认shell、sshd/firewall或杀毒策略。先前获准安装的Windows LAN sshd保留Manual/Running；测试前台进程正常结束，私有DB及日志保留。修复没有增加旧历史兼容、恢复A方案或改变身份/配置/export journal原文件职责。

### 12.7 修复提交与证据绑定

- SQLite修复提交：`dd84037caeef2bdf96915330650c4816ec88b9f8`，包含四项边界修复、定向回归、普通夹具ownership初始化及FILESTAT构建/平台快照接线。
- Windows明文SSH提交：`005ed51524595b47ddd5a4cc064efa40aaf6dd63`，包含literal验证、Windows partial退出码限定处理、原生shell/manager/集成回归及远端说明。

`committed-source-binding.json` 逐文件核对最终commit blob、本机归一化内容与v7测试archive；所有产品、测试、脚本和配置输入相同。唯一尚待随后文档提交的差异为存储方案7.3说明，验收主文本来就未放入build输入。按build.rs重新计算126个输入，build ID仍为04e6e729…3201a7，匹配两端实际binary info；没有为提交重新使用旧二进制或把后续代码变化留在dirty状态。文档提交仅保存报告与方案说明，不改变已经验收的源码。

### 12.8 本轮证据归档状态

自动审批先拒绝复制Mac整个日志目录，后又拒绝复制就地完成扫描的具体白名单归档，理由是用量、路径和来源元数据仍可能敏感，需要人类明确授权该归档和Windows目的地。两次均在进程创建前拒绝，没有传输、拆分scp或改用工具输出规避。精确授权问题已提交用户，等待答复；Windows记录在 `real-ssh/archive-transfer-review.json`。此项阻碍证据传输与最后的统一双端文件比较器，不改变上述已经执行的真实SSH/SQL结果。

Mac本地可审阅产物：

`/Users/user/sqlite-review-fixes-2026-10-03/real-ssh/remote/privacy-archives/controlled-logs-v7-2026-10-03.tar.gz`

- SHA256 `ba6723b09ea87a0761a8c61af364604b3738f124da5774151d4183d677213c5d`，508,706bytes；345常规成员为342份人工样本日志和3份隐私审计JSON。
- 受控fixture/provenance、四目录环境、auth不存在、fresh SQL raw/import quota0及无凭据模式已核对；UTF8、无SQLite/executable magic、路径/无链接、源与成员SHA稳定检查通过。archive0600、parent0700、成员0600。
- 非UTF8原始Windows OEM `whoami-user.stdout` 236bytes原件仍在Mac，SHA256 `929d204fd492eeba32f8477b090095c46d46c4534ce3d883677176fd9918210a`；明确排除，不改写成假UTF8。原343文件审计format-fail保留，新342候选审计通过，SHA256 `e6758c0868ff041d97d640326f5f2c862f3eed119c3130935d533c84eca6df70`。

预定目的地为 Windows `D:/Workspace/codex-usage-monit/target/sqlite-review-fixes-2026-10-03/real-ssh/controlled-macos-logs.tar.gz`。截至此记录尚不存在，统一 `revalidation-comparison.json` 和本轮 `fixes-evidence.zip` 尚未生成，不把准备好的比较/打包脚本算作实测通过。中心facts只读audit、Mac反向64/64 audit和本地平台结果分别已有；Mac `logs/reverse.validation.json` SHA256 `d8e12075b8629764b029ceda09976a41150be71549d2cb31a3f7b6fd4ed45fc5`。

原首轮与补测ZIP仍保留各自第9/10.5节SHA256，本轮未覆盖它们。`target`中的新证据不随Git同步；无新CI、push、tag或正式部署。用户批准具体传输后再补最后的文件对照及独立包哈希，不能在批准前声称已有Windows副本或统一包。

## 13. Windows 安装 bootstrap 改用私有脚本文件（2026-10-03）

### 13.1 修改与源码绑定

用户要求使用更安全的调用方式后，完成实现提交 **`b67a6a7dbfe789835ebb1b3663fb7d691c7ef120`**。普通 agent 同步的明文调用仍沿用第12节实现；本节补齐此前主动跳过的安装 bootstrap 启动层，而不是重新宣称整条 SQLite 实机验收已覆盖新快照。

Windows 官方远端准备改为两次独立 SSH：固定明文 receiver 只接收 stdin 数据，创建当前 SID 专属、受保护 ACL 的随机目录，拒绝既存目录和 reparse point；最多读取65,537bytes并拒绝超过64KiB，核对脚本原始字节 SHA256，再以 CreateNew 写入带 UTF-8 BOM 的 `bootstrap.ps1`。随后运行 `powershell.exe -NoProfile -NonInteractive -File .\.codex-usage-monit-bootstrap-<nonce>\bootstrap.ps1`。接收器不执行输入，没有 EncodedCommand、ScriptBlock::Create 或 Invoke-Expression。Expected JSON 的 Base64 仍只是元数据，不是可执行命令编码。

本机 Windows update 直接运行私有绝对路径的 `-File`；平台探测和固定文件清理也改为明文。准备仍保留五分钟下载预算、输出限制与取消语义。清理仅删除已知文件及空目录，拒绝 reparse point，不删除 receiver 拒绝的预存目录；Release 准备失败时由其原有 finally 清理自己的目录。未成功准备的目录不会被外层盲目删除。新增 receiver 纳入 `build.rs` 源码身份输入。

版本、build ID、协议、平台、大小和 SHA256 校验、候选 info/install 后校验、安装和服务 gate 保持原有要求。官方模式仍只经 SSH 在远端取得固定 HTTPS Release，没有 SCP 可执行文件兜底。没有修改执行策略、杀毒软件、正式安装、auth、sshd、默认 shell 或服务；文件入口更便于检查，不保证下载和安装行为不会被安全软件告警。

测试基线 HEAD 为 `85c855afeceb3b978e3c2382e71ee6ef8a19168c`，dirty 为 `build.rs`、manager Rust、receiver、两份说明文档。最终 v3 归档：

- Windows `D:/Workspace/codex-usage-monit/target/sqlite-bootstrap-safe-2026-10-03/source-v3.tar.gz`；SHA256 **`56b252ccc10c05b052b29ff91fd80405abe511006d5115be27534a4892a054c8`**。
- Mac 私有工作根 `/Users/user/sqlite-bootstrap-safe-2026-10-03`。隔离 checkout 用可用的 c420 对象作 Git 记账基线，实际代码是上述85c dirty归档；CRLF归一化、Git executable mode恢复、实际归档和源码稳定性分别记录，不能把c420当实际测试源码。
- 127个归一化构建输入的 build ID **`1bc17f0a7efd0298f73451c4eed111bb2aad5a904a659fd16ff482f0f72ede71`**。提交后逐文件核对 Git blob、测试快照与当前内容一致，工作树 clean；`validation-summary.json` 保存绑定和真实失败结果。

### 13.2 平台、二进制及本地结果

三端 Rust1.97.0，info均exit0、version0.5.2/schema1/protocol5、build ID同上。匹配源码的开发二进制未进入正式安装：

| 平台 | 实际二进制绝对路径 | SHA256 |
| --- | --- | --- |
| Windows11 AMD64 / x86_64-pc-windows-msvc | `D:/Workspace/codex-usage-monit/target/debug/codex-usage-monit.exe` | 最终双shell smoke后 `252c86e0e3ccae7c25f0ccb63c129bc2dc84c38fb93cc8737c57f6dc56ea6f94` |
| macOS15.7.2 arm64 / aarch64-apple-darwin | `/Users/user/sqlite-review-fixes-2026-10-03/cargo-target/debug/codex-usage-monit` | `539ba8521743f4e0c8c3cf166fc527cac2d631da0058a1332bbf32bfb5cd4b09` |
| Docker Linux aarch64 / aarch64-unknown-linux-gnu | `/Volumes/File/codex-usage-monit-docker-build/target-linux-arm64/debug/codex-usage-monit` | `78af7a53a9a6e6bd0e4f85790e6c2a20175cd3ff57755b929bb62c0ac54f3791` |

Windows各编译阶段的artifact独立记录：完整Rust批次时 `eb9100c0727ae9bbb5ffc4835ca0b9d178a4d951523a5807001dd69186d73c95`，定向ConPTY诊断时 `9207b195d9e5d2bc81e2c4275421a40d2cf8b56b1f57dd697cd879d122f70a7c`，最终smoke为上表252c。均为同源码build ID；不把不同编译产物混为同一个SHA。info前固定空的私有CODEX_HOME/state/config/cache，未读取正式历史或创建正式身份。最终Windowsinfo在 `windows.final-agent-info.json`，初次info在 `windows.checkpoint-agent-info.json`；Mac/Linuxinfo在 `platform-ssh-results/logs-v3`。

证据根为 `D:/Workspace/codex-usage-monit/target/sqlite-bootstrap-safe-2026-10-03`，下表日志相对此根；Python verifier在相邻 `D:/Workspace/codex-usage-monit/target/safer-bootstrap-2026-10-03`。Windows TEMP为Ghost-only `C:/Users/Ghost/AppData/Local/Temp/sr-20261003-5257353c`，Cargo target/build实际均为仓库 `target`。

| 检查与预期 | 实际、退出码与时间（UTC） | 结果与日志 |
| --- | --- | --- |
| manager校验、部署gates；3外层cmd/PS5.1/PS7 × 2内层PS5.1/7 × 6启动场景 | v2定向18pass/0fail/exit0；最终v3完整lib同18项通过。36组合覆盖正常、语法错、明确exit1、篡改、超限、既存sentinel，以及Unicode/空格/单引号路径。Release准备另有2shell×成功/坏校验和4子场景，下载夹具不可执行 | **通过**；`manager-focused-5.log`、`windows-checkpoint-v3.log`。不把v2当最终build证据，v3由完整lib绑定 |
| shared Release verifier | 原样13方法通过，PS5.1.26100.9444/7.4.1各12子场景，共24，exit0；09:57:10–09:57:32；三个未修改verifier输入SHA前后稳定 | **通过**；`python-full-verifier.result.json` SHA `37ce788f9e59f84ebd319842b002f11e0b083b7e14e5628079692a97ef3ada94` |
| Windows格式、Clippy、全部Rust目标 | 格式/Clippy0；完整runner在ConPTY处exit1，Cargo101。已执行1858pass/1fail/3ignore；随后补update_cli4pass/1ignore及usage_evidence9pass/exit0。唯一目标汇总 **1871pass/1fail/4ignore**，无编码bootstrap过滤 | **完整批次失败**；`windows-checkpoint-v3.log`、`windows-remaining-targets.log`。完整日志创建10:09:42，失败和诊断不隐藏 |
| Windows5.1/7实际文件入口离线CLI smoke | 两shell分别 `verify.ps1 -SkipFormat -SkipClippy -SkipTests`，脚本exit0；offline snapshot预期partial/CLI2由原有JSON断言接受 | **通过**；`windows-smoke-51.log`、`windows-smoke-7.log`。未运行无关的Bypass installer fixtures，不能称完整脚本合同全部通过 |
| macOS受影响本地回归 | `sh scripts/verify-unix.sh --filter remote_agent_manager` 17pass/0；随后 `cargo test --locked --offline --test agent_management -- --nocapture --test-threads=1` 2pass/0；10:10:26–10:11:17 | **定向通过**；`platform-ssh-results/logs-v3/native.result.json` SHA `db56520dd5fb5118d46676f56109fc678d4beccfb36578cf531dfdf148721f12` |
| Docker Linux受影响回归 | `sh scripts/test-linux-docker.sh --filter remote_agent_manager` 17pass/exit0；10:10:29–10:11:17；snapshot SHA `b5c5940dd5f5ea7a6c00131944cb8cd9098d0374549a663a7af9b8eafd411c78` | **定向通过**；`platform-ssh-results/logs-v3/linux-run/result.json` SHA `0129ab164054512ac7d1c75de357ed5a70d0ac28c69dbc14ea9d03062e3b3e46`；非新全Linux/macOS套件 |

实际Windows完整入口为 PowerShell7.4.1：

```powershell
& scripts/windows/verify.ps1 -RepositoryPath D:/Workspace/codex-usage-monit -CargoTargetDir D:/Workspace/codex-usage-monit/target -CargoBuildDir D:/Workspace/codex-usage-monit/target -TestTempDir C:/Users/Ghost/AppData/Local/Temp/sr-20261003-5257353c -SkipSmoke
cargo test --locked --test tui_pty -- --nocapture --test-threads=1
cargo test --locked --test update_cli --test usage_evidence -- --test-threads=1
# 两shell均使用普通 -File，无Bypass；完整参数也保存于validation-summary.json：
powershell.exe -NoProfile -NonInteractive -File scripts/windows/verify.ps1 -RepositoryPath D:/Workspace/codex-usage-monit -CargoTargetDir D:/Workspace/codex-usage-monit/target -CargoBuildDir D:/Workspace/codex-usage-monit/target -TestTempDir C:/Users/Ghost/AppData/Local/Temp/sr-20261003-5257353c -SkipFormat -SkipClippy -SkipTests
pwsh.exe -NoProfile -NonInteractive -File scripts/windows/verify.ps1 -RepositoryPath D:/Workspace/codex-usage-monit -CargoTargetDir D:/Workspace/codex-usage-monit/target -CargoBuildDir D:/Workspace/codex-usage-monit/target -TestTempDir C:/Users/Ghost/AppData/Local/Temp/sr-20261003-5257353c -SkipFormat -SkipClippy -SkipTests
```

首次编译因先前短TEMP已清理而LNK1104，重建当前SID私有TEMP后解决；新测试编译借用、mock误认receiver内部cleanup及PowerShell自身启动cwd的非法`[]` pattern问题在夹具修正后通过，保留 `manager-focused-{1,3,4}.log`。v2完整检查曾在新测试的 unnecessary_unwrap Clippy处失败，改成if let并重新绑定v3后通过lint。没有删除失败记录、放宽安全检查、修改执行策略或用旧绿灯替代新结果。

### 13.3 真实反向SSH启动链

Mac通过现有私有 `/Users/user/sqlite-review-fixes-2026-10-03/real-ssh/reverse-client/ssh_config` 的 `acceptance-win-lan`，真实连接Ghost@192.168.100.20:22。每条保持严格host-key检查，核对ED25519 `SHA256:3UqgD5DbXKUfY1uaR2yYcBhskq/KeeTMRbgfi47e3xE`。默认Windows SSH cwd为 `C:/Users/Ghost`，只创建随机独立SID私有bootstrap目录，不写正式state。内容完全为合成脚本，没有Release下载或候选安装。

| 场景 | 预期 | 实际 | 判定 |
| --- | --- | --- | --- |
| 接收并执行正常脚本 | receiver0/空stdout；-File0/唯一JSON | 符合，原始UTF8+BOM文件、SHA与ACL交叉核对 | **通过** |
| 不完整语法 | 保存成功；文件解析失败而非stdin EOF假成功 | receiver0，-File1/ParserError/空stdout | **通过** |
| 输入篡改 | SHA不符拒绝，候选不执行 | receiver1，自己的暂存目录已清理 | **通过** |
| 预存私有目录 | 拒绝覆盖、sentinel原样 | receiver1，sentinel SHA不变；只在测试核对完成后清理自己创建的fixture | **通过** |

UTC10:13:47–10:14:06，4/4场景、19条实际SSH、**56/56检查**。owner为当前Ghost SID1001、DACL受保护且仅该SID、目录非reparse；所有本轮自己创建的stage最终均不存在。完整可审阅命令、合成stdin、退出码、stderr/stdout、SSH Sending command/TCP/key、ACL及哈希位于 `platform-ssh-results/logs-v3/synthetic-reverse-ssh`；`result.json` SHA256 **`32d11e4e5ab5d442c0321729d0733d76e1bfc404deca31ae702311fbbefbb18e`**。receiver归一化SHA `024a13d3713f51c9be4d5013537b4ae32a5c17edec27a4c4a7a1fbea7d595785` 与已提交源码一致。

### 13.4 ConPTY失败与覆盖边界

全量中 `real_tui_pty_handles_keyboard_mouse_search_resize_and_exit` 在原有8秒门限等待初始fixture会话失败；原样单线程focused再次同样失败（Cargo101，另一ConPTY测试通过）。未强称偶发。原样测试源码、TUI、rollout、startup和SQLite stage/load与85c逐路径一致，新bootstrap调用不进入这项离线、无remotes配置的初始加载路径。

进一步用独立rustc test harness绑定旧 `aa131b9a…`/04e6 CLI作同条件对照，同样exit101；当前 `9207b195…` 冻结CLI的诊断副本只增加日志/保留私有目录，保持原8秒谓词，也exit101。两个harness编译0，CLI/原始测试SHA稳定，不替换当前Cargo二进制。当前首帧约235ms，rollout scan约3.381ms、materialize180µs已完成；`history.stage_load` span开始后到门限结束仍未finish，故Finalizing文案不能定位为rollout卡住。已有history stage/load在当前环境可重复超时，内部阶段和原因 **仍未定位**；不据此扩大本次调用层修复、盲加timeout或宣称完整Windows全绿。

证据在 `tui-diagnostic`，`baseline-trace.result.json` SHA `7a7d1b90bcae6684fcc9cb53d7f13f833192c462370f6ea1141bb429930a8032`，operation日志SHA `0be27fb018b9b7142fec20ec58d0763d65bb497815f1a3bdc96eb03c9511c4a5`；仅复制content-free日志，保留的可丢弃fixture在短TEMP `.tmpO4geIs`，受控PID30312已kill/wait。没有读取正式DB、停止正式recorder或修改原TUI测试。

本节证明了新Windows bootstrap的原生shell合同、下载校验控制流和真实SSH“接收文件→执行→错误状态→清理”链。**正式Release可用性、成功官方安装/服务升级仍未覆盖**，当前未发布分支不能退回旧Release。没有为新build重新运行非空SQLite双向用量、quota、facts/proof、多页恢复或TUI同步刷新；第12节真实SQL证据仍绑定其原04e6 build，不因本节source变化自动升级。第12.8待授权的旧用量归档仍未传输；这里只回传本轮源码/编译和合成启动日志。无新CI、push、tag或正式部署。

此前 `transport-v4-probe/frozen-v4-libtest.exe` 保留的是旧编码调用，只作为原始失败证据；不要拿它复跑本轮安全调用。新合同由当前Cargo测试二进制及已核对build ID的开发CLI执行。

用户随后要求分析 ConPTY 超时；进一步的独立观察已确认当前二进制最终在16.086秒出现fixture数据，原8秒失败仍保留。阶段计时、原因分析和源码/命令绑定见 [Windows ConPTY 初始数据超时诊断](conpty-startup-timeout-analysis-2026-10-03.zh-CN.md)，不追溯修改本节当时的验收结果。

## 14. ConPTY启动超时修复与本地检查补充

用户要求修复后，本地实现提交 **`2ac404617a60bbcd25e4908dba69f81113d139f9`** 将revision所需SQL读取合并到短read snapshot，并移除Windows同一已绑定数据库对象的重复完整验证。ownership/project mapping双读、receipt、live ACL、全部祖先reparse校验、文件身份、硬链接、SQLite真实HANDLE和WAL/SHM guards均保留；没有改原8秒门限、fixture、TUI交互代码或同步协议。

| 场景 | 预期 | 实际 | 判定 |
| --- | --- | --- | --- |
| 原ConPTY初始fixture数据 | 原8秒内可见并完成交互 | 原两项ConPTY均通过；独立非仪器CLI观察6.411187s ready、q退出0，修复前16.085970s | **通过**，仅本次受控fixture回归，非正式历史性能基准 |
| revision并发与初始化边界 | 一致SQL stamps/policy、保留外部文件变动检测；不创建/重建缺失库 | 三项新回归通过；仅恢复旧revision函数的exact回归test101，命中并发快照断言 | **通过**，确定性红灯/绿灯证据 |
| Windows文件安全与rollback | DB/WAL/SHM权限扩大、硬链接或当前路径身份不符时拒绝nested use，保留旧提交 | 实际Win32 DACL扩大和hardlink测试均拒绝/回滚，ACL恢复，旧提交保留、新写未入库 | **通过**，11项数据库focused回归 |
| 稳定源码完整本地检查 | 对本轮实现完成平台对应的全套检查 | Windows1878、Mac1923、Linux ARM64 1920项Rust通过，各0失败；完整entry均exit0 | **通过**；ignored为4/3/3，非零退出的离线partial smoke按既有合同检查 |
| 本轮build的非空双向SSH、quota、facts/proof、多页恢复 | 重新绑定实际同步证据 | 本轮未重跑这些场景 | **未覆盖**；第12节证据仍绑定原04e6build |

测试时源码是deec0256+三份声明的dirty文件，现已保存为上述实现提交；sourceHash `6a1f4dd99d51957039daaf353c2a3af2a4314fd50c51aecdd21c888595731e72`，三平台build ID均 **`f9bf593a9e8b33eece0dc099fc9cb5f8b2dd80db1ebe7b09f04198ac606f9451`**。原失败证据保留，第13节结果不追溯改写。完整命令、平台、UTC、各开发binary路径/SHA256/info、dirty snapshot及限制作进一步说明，见 [诊断与修复第7节](conpty-startup-timeout-analysis-2026-10-03.zh-CN.md#7-修复与原门限回归)。

日志根为 `D:/Workspace/codex-usage-monit/target/conpty-startup-fix-2026-10-03`（focused、red-revision、observer、Windows完整 `validation-summary.json`），跨平台合成日志和source summary位于 `D:/Workspace/codex-usage-monit/target/fix/platforms`。Mac端仅通过真实local-mac SSH传输源码和本轮合成日志，未传输待授权的旧用量归档。未更新正式安装/recorder/sshd或防护策略，未跑新CI、push、tag或正式部署。
