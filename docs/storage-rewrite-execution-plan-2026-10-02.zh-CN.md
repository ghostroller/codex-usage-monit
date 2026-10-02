# 历史存储重写执行方案（2026-10-02）

> 2026-10-03 范围更新：按用户最新决定，旧配额采样保留到 SQLite；来源策略、本地编号高水位与未完成来源删除意图等控制状态保留；其余旧派生历史不迁移，使用现有采集、回填与远端同步按需重建。生产仅保留 SQLite 历史后端；本轮实现与验证状态见 [7.3](#73-sqlite-唯一后端与按需重建)。

## 1. 决定与实施范围

用户本次明确决定不采用 A 方案，不通过删除去重功能减负，优先重写存储机制，并授权在新分支实现。本方案是本次任务的现行口径；2026-09-26 的提案、审核和执行方案保留为历史记录，其中 A 退役路线不再推进，SQLite 仍未获委托的描述不适用于本次任务。

保留现有 C 产品语义：本地/远端精确来源、AllIncluded 跨源归并、复制/分叉识别、facts 补齐、digest proof、完整性/partial、quota provenance、金额精度、报告 JSON 和退出码。CLI/TUI 共用编排继续复用。目标是将历史持久化的事务、一致读取和恢复责任交给 SQLite，而不是减少业务能力。

初始基线为 `10ddd7b`（保存此前完成的 CLI/TUI 编排收敛），分支为 `codex/sqlite-history-storage`；本轮以前一批文档提交 `10739bc` 为起点继续收敛。开发和验证只使用隔离合成数据与必要旧配额/控制状态夹具，不操作用户真实 history-root、真实 SSH 或服务部署。生产重写已获授权，不再以旧原型门请求重复许可。旧历史全量导入已退出本次范围。

## 2. 存储边界

SQLite 管理 `SourceHistoryStore` 所有新的历史数据与中心摄取状态：account quota、bucket、weekly、session digest、来源 metadata、revision/回填/保留状态、远端 active/staging 数据、quota/live、摄取游标与页身份，以及完整 session facts 和其激活状态。

来源身份/anchor、ownership/profile lease、remotes 配置、项目映射、带宽/health、服务和安装升级状态继续使用原文件及现有责任。它们不属于待丢弃的派生历史。远端 agent 的协议导出 journal/分页快照也保持既有协议职责；这些不是中心历史数据库，不将它们计作已替换。底层存储重写不改变 v5 wire DTO 或价格 catalog。

保留 `SourceHistoryStore` / `SourceHistoryWriter` 和既有业务 DTO 作为接入边界。SQLite 按记录存储和索引，不把完整 JSON 分片搬进一个 BLOB 后继续运行原来的 redo/COW 引擎。复杂 payload 可以采用经既有校验的无损记录编码；索引字段、行身份与事务发布由数据库管理，金额继续由 Rust 受检精确计算。

## 3. 必须保持的契约

| 契约 | 实现要求 |
| --- | --- |
| revision 不复用 | 独立持久化保留高水位，再提交数据事务；失败允许空洞，不回滚已经发放的编号。超出 SQLite 有符号整数范围的业务编号使用无损编码 |
| 本地共同发布 | account、bucket、weekly、digest 与 metadata 同事务；不完整扫描、partial、reconcile tombstone、quota reset 漂移及同 revision 冲突规则不变 |
| 远端页共同发布 | 页数据、quota/live、页身份、游标和激活状态同成同败；重复页幂等，失败不推进游标 |
| bootstrap 可见性 | 多页候选在完成前不可见；保留 logical generation、exact binding、expected-active CAS 和最终激活，不依赖复制目录发布 |
| facts 与去重 | 完整 facts batch、validated digest proof、来源 binding、active version/CAS、分页 cursor 和 reconciliation 规则保留；候选准备与激活分开 |
| 一致读取 | 一次应用查询在同一数据库读事务中读取所需数据族；外部 ownership、profile、来源策略和项目映射仍有前后 fence 与有界重试 |
| 权限和对象安全 | 数据库、WAL、SHM 与父目录保持私有权限，拒绝符号链接/对象替换和不可信 schema；不能仅验证一次打开前路径就宣称等价 |
| 资源有界 | 沿用源数量、记录和解码字节预算；写事务短、busy 等待有界，网络及长解析在事务外；记录 checkpoint 和文件增长策略 |
| 旧进程 | SQLite 初始化受 ownership/profile/service 协调；版本/epoch 发布让旧写者拒绝操作，旧 bucket/facts/cursor 不再作为查询或写入后端 |

SQLite 起始配置采用 WAL、`synchronous=FULL`、外键和有界 busy 策略；实际耐久性、侧文件安全和平台支持以测试为准。数据库锁不替代应用租约。SQL 只查询和索引精确编码，不把 `u128` 金额转换为 REAL 或普通 SQL SUM。

## 4. 工作包与顺序

| 工作包 | 工作与产物 | 完成条件 |
| --- | --- | --- |
| S0：规划与基线 | 保存编排基础、新分支、冻结业务与历史格式契约；建立本方案 | 范围明确，A 删除路线退出，去重回归清单保留 |
| S1：数据库基础 | 选择并锁定 Rust 包/实际 SQLite，schema、私有打开、读写事务、精确编码、busy/checkpoint 与错误映射 | 实际版本/编译选项可记录；安全、事务、数值和多进程回归通过 |
| S2：本地 observation | 接入 account/bucket/weekly/digest/metadata；独立 revision 保留、marker 和 retention | 同批一致读取、中断恢复、不复用编号、partial/reconcile/quota 规则通过 |
| S3：远端聚合摄取 | 接入 active/staging、remote quota/live、页状态与 cursor，替代跨文件 WAL/COW 发布 | expected-active、重复页、失败页、generation 改变及 bootstrap 中断回归通过 |
| S4：facts 与查询 | 完整 batch staging/激活、proof/cursor；统一 SQL snapshot，沿用现有 C reconciliation | 独立、复制、分叉、缺失 facts、同 revision 冲突、partial 和 exact 查询结果不变 |
| S5：初始化与必要状态保留 | 受 ownership/profile/service 协调，保留旧配额采样，以及 SourceMetadata（含来源用户策略）、本地 revision 高水位与未完成来源删除意图；发布 SQLite 初始化 receipt/epoch | 不导入旧用量、facts、digest、pending 页或 cursor；旧写者拒绝，初始化中断可恢复，已激活丢库不重建 |
| S6：退役旧引擎与适配 | 删除 File/V1 历史后端、全量历史迁移、本地 redo、每页物理 COW 和跨文件游标补偿；仅保留必要配额及策略/编号/删除意图读取 | CLI/TUI/recorder/同步从构造起绑定 SQL；无旧历史读回退或双写，完整 C 去重与协议能力保留 |
| S7：组合验证与交付 | 原生 macOS、Docker Linux、UTM Windows 的相关完整检查、独立审查、文档和源码证据 | 结果绑定最终源码；失败/未执行项如实记录，不以旧批次替代 |

S1 统一管理数据库接口与依赖；S2、S3、S4 可在接口冻结后按不同模块并行。必要状态保留及 ownership 发布由主 agent 统一。影响范围测试在修改中执行，平台完整检查在批次稳定后执行。既有 Windows 启动器清理失败独立保留诊断，不通过重跑或提高超时掩盖。

## 5. 初始化、保留与按需重建

生产入口统一使用 SQLite，`SourceHistoryStore` 不再提供 File 后端。`history-v1` 仍是既有 CLI 路径绑定名称，实际数据库位于同一 state-root 的 `history-v2/{profile}/history.sqlite3`。绑定或权限失败可显示本进程当前采集结果的只读内存视图；等待旧 recorder 退出或初始化协调时不读取旧 JSON 历史。

| 状态 | 本轮处理 |
| --- | --- |
| 旧 account/remote quota 采样 | 有界校验后保留到 SQLite；这些过去的服务端观测不能从 rollout 重算。账户归属、来源 opt-in、provenance 与 quota 合并规则保留 |
| SourceMetadata（含 label、来源用户策略）、本地 revision 高水位 | 作为非派生控制状态保留到 SQL，避免重建重置 label、Include/Exclude、隐私/同账户策略或复用已发放编号；数据提交版本与预留高水位继续分离 |
| 未完成来源删除意图（旧 `source-purge.json`） | 经有界 typed 校验，与必要状态在同一初始化 SQL 事务中保留；包括 metadata 已删但外部 mapping 清理未完成的 claim。用户已申请的不可逆删除继续阻止重新配对，不由旧历史重建复活来源 |
| 来源身份/anchor、remotes 与自动同步配置、项目映射、服务/安装状态 | 保留原文件和协调规则；不将整个 state-root 当作缓存删除 |
| 旧 bucket/weekly 用量、session digest、facts/proof、回填完成标记、live、ingest cursor、pending publication | 不导入；新数据继续由现有采集、Summary 回填、aggregate bootstrap 与 facts 跟进产生，不兼容旧历史引擎 |
| 远端 wire DTO、agent export journal/分页快照 | 保留原协议职责；中心改用 SQL 不代表复制或双向共享 SQLite 文件 |

初始化先取得确切 profile/ownership 与必要服务协调，发布版本 2 的 `Migrating`/新 epoch，阻断旧写者。必要状态在 SQL 事务中保存，并写入新的 initialization receipt，再发布 `V2Active`。未完成来源删除意图属于用户控制状态，不是可丢弃的派生历史；其保留不会引回旧 bucket/facts/cursor 或完整文件引擎。两个 privacy namespace 共同受 fence；提交后激活前中断可从 receipt 恢复，已完成的必要状态保留不重复从旧副本覆盖 SQL 当前值。旧 pending observation/page 不恢复为业务提交。

本轮数据库使用 schema version 2，initialization receipt 与此前开发版的 full-migration receipt 不兼容。旧 schema version 1 的开发预览库明确拒绝，不能在旧 `Migrating` 状态下继承其已导入的派生历史；库不会被自动删除、覆盖或转换成新空库。存在旧 active manifest 而缺少匹配新 receipt 时同样拒绝。正常 `v0.5.2` 等旧文件历史中的配额采样仍按上述范围保留，不受开发版数据库不兼容规则影响。已有 SQL Active 库缺失、为空、损坏、对象不可信或 schema/receipt 不支持时同样 fail closed，不能静默重建或退回旧 JSON。profile 数据库已存在而本 namespace 的 ownership manifest 丢失或损坏时也拒绝重新初始化，避免重造 epoch 意外匹配旧 receipt。首次初始化尚未激活时留下的自身空库发布，可在验证对象身份和既有 ownership 后恢复。这与丢失已激活数据是不同状态。

历史重建由应用的准备流程按需触发：普通采集保留既有 lookback 和文件预算；Summary 使用既有有界回填窗口；远端需要时重新 bootstrap，再执行增量/facts 跟进。只读查询只读取 SQLite，不自行扫描、同步或写入。启动不主动解析全部旧历史，不新增全历史重建命令。已删除、截断或超出扫描预算的 Codex 原始日志不能保证重新生成；覆盖不足继续显示 partial/下界。旧配额保留不会补造缺失用量。

未增加自动回退或备份 CLI，也不声称新 SQL 数据能无损交给旧二进制。旧派生文件不再是活动后端；本轮不自动删除它们。SQLite 一致备份必须包含 WAL 中已提交的状态，不能直接复制使用中的主数据库文件作为完整备份。

## 6. 验证与收益记录

本批不承诺固定提速或减行百分比，以业务、安全和平台契约验证作为交付条件。性能基准留作后续测量：用固定合成档位比较同等持久性下的首次打开、重复读取、提交、磁盘增长/写放大和构建成本，记录原始样本、失败数、配置和环境；未控制 OS 缓存不称真正冷缓存。

收益按实际删除的责任记录：本地 redo/多族补偿、每页整代文件复制、文件 manifest 发布、数据与游标的跨文件恢复，以及减少的分片读改写。新增 SQL/schema、必要状态保留、初始化恢复、数据库权限、checkpoint/忙等待和平台依赖的维护成本同时计入。不以只移动代码或删除测试计作净减负。

相关回归除新增数据库契约外，保留既有 replica/facts、Summary/Trends/Health、CLI JSON/退出码、TUI 键鼠/compact、profile/redaction/只读及真实 PTY/ConPTY。平台命令和证据按 [testing.md](testing.md)；记录 commit、dirty snapshot、架构、完整命令、日志、结果与跳过项。最后阶段只在有具体本地证据的集成检查点考虑 hosted CI，不创建测试 tag。

## 7. 逐批实施与验证记录

以下开工条目及 7.1/7.2 保留前两批实现的历史事实，绑定各自源码，不代表 2026-10-03 收敛后的状态或验证；本轮见 7.3。

- 2026-10-02：本次用户决定和生产实施范围已记录；`10ddd7b` 保存编排基础，已创建 `codex/sqlite-history-storage`。
- 已锁定 `rusqlite =0.40.2`（`bundled`、`limits`），实际链接 `libsqlite3-sys 0.38.2` 内的 SQLite **3.53.2**，source ID 为 `2026-06-03 19:12:13 d6e03d8c777cfa2d35e3b60d8ec3e0187f3e9f99d8e2ee9cac695fd6fcdf1a24`。该版本包含 WAL-reset 修复；不将 Rust crate 版本当作 SQLite 版本，也不将它称作最新 SQLite。[实际 bundled 源码](https://docs.rs/crate/libsqlite3-sys/0.38.2/source/sqlite3/sqlite3.h)与[官方发布记录](https://www.sqlite.org/changes.html)已核对。
- macOS 构建通过实际链接的 `rusqlite` 查询 `sqlite_version()`、`sqlite_source_id()` 和 `PRAGMA compile_options`，完整输出位于 `target/sqlite-storage-2026-10-02/sqlite-build-info.log`；含 `THREADSAFE=1`、`ENABLE_API_ARMOR` 和 `MUTEX_PTHREADS`。编译默认 checkpoint 为 1000 页，应用明确覆盖为 256 页；编译长度上限也由数据库打开时收紧至 128 MiB，不能将编译默认值当成实际运行配置。
- 数据库位于 `history-v2/{profile}/history.sqlite3`，使用两张按记录存储的 STRICT/WITHOUT ROWID 表及时间/metadata 索引。普通 busy 上限 250ms；config fence 内的远端页与 facts 激活采用立即失败的写锁；自动 checkpoint 为 256 页，沿用保留时钟进行逻辑 GC。长期读事务仍可能阻止 WAL 截断；逻辑 GC 不保证立即缩小主文件，未添加每次 GC 的 VACUUM。
- Unix 不对已打开数据库另开/关闭身份检查 fd，以免释放同进程其他连接的 POSIX 锁；采用 SQLite 自身 fd 的 `HAS_MOVED` 和路径对象检查。首次创建的临时 fd 在发布主文件前关闭，空库发布中断可验证并清理自身临时硬链接。Windows 检查实际 SQLite HANDLE、ACL/reparse 和对象身份，使用 create-new 发布。
- S1—S5 的生产路径已接入，冻结前 49 项 SQLite 定向回归及全 target Clippy 通过；这不是最终平台检查证据。独立复审已修正旧 FileV2 的 CLI/TUI/自动同步就绪入口、迁移期间旧 epoch 只读、旧备份复活和空库重建等边界。最终验证结果将补于本节。
- 正常 SQLite 运行已不使用本地 redo 文件、每页物理 COW、active manifest 文件和游标跨文件发布。旧文件实现仍承担旧 V1→FileV2→SQLite 的首次升级及格式夹具验证，未物理删除全部旧实现，不称净代码量下降；既有 agent 导出 journal 保留。尚未进行同耐久性性能基准，不给出提速或百分比减负结论。
- 本地 revision 高水位与已提交数据版本分开保存：预留编号单独提交，projection stamp 随数据事务发布，避免跨 selector 缓存把「编号已预留、数据尚未提交」当作新快照。迁移初始化中断可恢复自己的空库发布；已激活的丢库/空库仍拒绝重建。另一 privacy namespace 首次导入只更新该新本地 namespace 的 policy，重复旧副本不能覆盖当前策略。
- SQL 查询和外层 TUI projection 缓存同时监测两种 privacy 的已提交版本：配额按 profile 共享，另一 privacy 写者提交后也必须失效。远端 generation 枚举只解码 `generation.json`，不会将同 namespace 的 quota header 混读为 generation；回归覆盖带配额的多页 bootstrap、增量重放、tombstone 与代清理。

### 7.1 冻结源码与平台检查

存储实现提交为 `188726780a45672bc27f87dc3feb0a06f7c270bb`。以下检查均针对该提交，开始时工作树干净；后续仅补本文档，不修改构建输入。三平台 Rust 均为 1.97.0。

| 平台与范围 | 结果 | 日志与身份 |
| --- | --- | --- |
| 原生 macOS 15.7.2 / ARM64，完整 Unix 流程 | 通过：1900 项 lib、2 项真实 PTY、其余 Rust target、90 项 Python、format/Clippy、preview、installer 与 CLI smoke；默认忽略 3 项测量测试 | `target/sqlite-storage-2026-10-02/macos-full-20261002T175011/{result.json,verify-unix.log}`；构建输入 SHA-256 `639b5bd1dce9f2f47edad92aaf2125e24e165be4d3f3d96e4a778c7e30bac333`，前后相同 |
| Docker Linux / ARM64，完整 Unix 流程 | 通过：1897 项 lib、2 项真实 PTY及其余完整流程；默认测量测试未执行 | `/Volumes/File/codex-usage-monit-docker-build/runs/20261002T095019Z-arm64-92946/{result.json,verify.log}`；隔离快照 SHA-256 `262cc9d381dbf381ce71c9b0437dba6812aa317a02b270c338363decd6dd2a65` |
| UTM Windows 11 ARM64，实际执行 x64 MSVC target，完整流程 | **未全绿**：1842 项 lib、2 项 ConPTY、format/Clippy、Python 和 PowerShell 契约通过；Rust 总计 1960 通过、1 失败、4 忽略，停止于下述旧启动器问题 | `target/sqlite-storage-2026-10-02/windows-full-x64/729b4d245eb149b393bbdf36f547e8d3/{result.json,verify.log,interactive-context.json,task-cleanup.json}`；source ZIP SHA-256 `3dd3f811f2a39e633348eb901dfe87aa33ad9fb78843954540ce107a7ab7e45a` |

完整命令：

```sh
# 原生 macOS：此证据 wrapper 记录前后源码 hash，并调用下面的原生入口。
python3 -B target/sqlite-storage-2026-10-02/verify_native.py
# 实际原生入口及环境：
CARGO_TARGET_DIR=/Users/user/Workspace/codex-usage-monit/target/review-lock-storage \
CARGO_BUILD_BUILD_DIR=/Users/user/Workspace/codex-usage-monit/target/review-lock-storage-build \
CARGO_NET_OFFLINE=true sh scripts/verify-unix.sh

# Docker 默认 native architecture，此机为 Linux ARM64。
sh scripts/test-linux-docker.sh

# 调用现有 UTM runner，临时 InteractiveToken 任务使实际测试在登录用户下执行。
python3 -B target/orchestration-convergence-2026-10-02/verify_windows_interactive.py \
  --interactive-user 'WIN-MM0JRLGM2Q3\user' \
  --python-dir 'C:\Tools\codex-usage-monit\python-3.13.16-arm64' \
  --private-temp 'C:\Users\user\AppData\Local\Temp' \
  --toolchain-home 'C:\Users\user' \
  --pwsh-path 'C:\Tools\codex-usage-monit\powershell-7.6.5-arm64\pwsh.exe' \
  --target x86_64-pc-windows-msvc --timeout 1800 \
  --output-dir target/sqlite-storage-2026-10-02/windows-full-x64
```

Windows run 的实际身份为 `WIN-MM0JRLGM2Q3\user`、session 1，使用其私有 TEMP；临时任务已确认删除且无残留进程。34 个带 SQLite 名称的 Windows 回归均通过；Unix 专用 fd/硬链接回归在 macOS/Linux 执行，不能将 Windows 条件编译跳过的分支计作原生覆盖。

Windows 失败为 `tests/update_cli.rs:210` 的 `running_portable_launcher_with_different_bytes_passes_real_proxy_contract`：兼容性探测结束后残留 `.launcher-probe-*`。该失败在此前编排批次已经复现；`src/update.rs` 与该测试相对本次基线 `10ddd7b` 无变化。归类为既有启动器清理问题：清理代码忽略删除错误，确切 Windows 错误码尚未采集，不归因于 ARM64，也不称偶发。因 cargo test 在这里停止，后续 `tests/usage_evidence.rs` target 与独立 Windows CLI build/smoke 未执行；真实 CLI 数据集成测试已通过。此次未重跑完整套件、未改超时、未删断言掩盖失败。以上是 `1887267` 检查当时的记录；后续错误码诊断、修复及验证见 [7.2](#72-启动器跟进修复与验证)。

本批没有运行 hosted CI、Linux x64/musl 发布验证、真实 SSH、真实用户历史性能基准或服务部署；本地流程不等于发布验收。S0—S5 已完成生产接入，S6 已完成正常运行路径替换而保留必要旧格式代码，S7 的本地检查已执行并保留上述 Windows 未通过项。未宣称全平台全绿或净代码量减负。

### 7.2 启动器跟进修复与验证

启动器修复提交为 `3fa8f28ebbc9ce81b54520cec5a59686221aa957`，仅修改 `Cargo.toml`、`src/update.rs` 和 `src/update/tests.rs`。这是 7.1 所列既有失败的独立跟进，不改写 `1887267` 的历史检查结果。

诊断 run `0b7100a4e2f64df3b2bb2cd8b413cb93` 在真实代理测试中采集到两个探测 `.exe` 删除失败的 OS 5，探测元数据和锁文件删除成功，随后目录删除为 OS 145。直接使用 `SetFileInformationByHandle(FileDispositionInfoEx, DELETE | POSIX_SEMANTICS)` 的候选方案，在真实 `SEC_IMAGE` 映像夹具与原代理测试中仍返回 OS 5，故已撤回；实验记录保留于 `target/windows-launcher-fix-2026-10-02/{diagnostic/0b7100a4e2f64df3b2bb2cd8b413cb93,windows-focused/250fdf8ffab14ab79c10edc00a61b8d6,proxy-api-diagnostic/319eaa8005c0463cac816578b897f8f6}/{result.json,verify.log}`。这组结果不证明 ARM64 是根因，也不将 Rust 的 `fs::remove_file` 简化为仅调用传统 `DeleteFileW`。

最终继续使用 `fs::remove_file`，仅对实际文件删除返回的 OS 5/32 等待并重试。一次显式清理中的全部已知文件共用 **1 秒等待预算**，每次等待最多 10ms；不增加原有 **15 秒探测执行超时**。每次删除前保留 reparse 路径检查；不存在的对象视为成功。显式清理失败返回带路径的 `update_probe_cleanup_failed`，保留原始 `io::Error` 和 OS 错误码，不能继续报告兼容成功；`Drop` 只执行一次不等待的兜底清理。目录仅用 `remove_dir` 删除，所有目录删除错误均不重试；不递归处理未知内容，非空目录的 OS 145 保持可观察。

新增四项确定性 Windows 回归：真实映像映射、禁止 DELETE 共享的句柄、缺失对象及重复清理、未知文件与目录保留。映像夹具用传统 `DeleteFileW` 确认 OS 5 基线；若标准库能直接 unlink，则保留映像 view 并验证其 `MZ` 可读，若删除失败则只在失败回调中释放映像后重试。共享句柄夹具确保至少一次真实失败后释放句柄重试成功，另用持久 blocker 和有限回调预算验证 OS 32 可观察且能够终止。测试不依赖固定 sleep；原 `running_portable_launcher_with_different_bytes_passes_real_proxy_contract` 的真实代理及无残留断言保持不变。

定向检查在 `deacede416db8e20d9f4bd4bc05a97393f5fde91` 加上述三个未提交文件的冻结快照上执行，随后保存为 `3fa8f28`。原生 macOS 构建输入 SHA-256 为 `d4e6523fcc7d190425f1a22e65f193b174e8cb9d6487e9c25a980b04294864c2`，检查前后相同；Windows 定向和最终全量 run 的 source ZIP SHA-256 均为 `f2f43b057f93725b62b83d07fd0deb014689e4fe3e110000c73f7e5429350853`。独立复核确认这些快照的构建输入逐文件匹配 `3fa8f28`。两种摘要的统计范围不同，不将其字符串互相比对。

| 平台与范围 | 结果 | 日志与身份 |
| --- | --- | --- |
| 原生 macOS 15.7.2 / ARM64，format、全 target Clippy、`update` 定向检查 | 通过，exit 0：53 项匹配 Rust 测试；Windows 专用四项回归在此条件编译跳过 | `target/windows-launcher-fix-2026-10-02/macos-focused-20261002T224744/{result.json,verify-unix.log,source-before.json,source-after.json}`；上述 dirty snapshot 与输入摘要 |
| UTM Windows 11 ARM64，实际执行 x64 MSVC target，`launcher` 定向检查 | 通过：13 项测试、1 项既有忽略；含四项新回归和原真实代理测试 | `target/windows-launcher-fix-2026-10-02/windows-retry-focused/34635e11cc644526a05fd824e3fc5e53/{result.json,verify.log,interactive-context.json,task-cleanup.json}`；上述 dirty snapshot 与 source ZIP 摘要 |
| UTM Windows 11 ARM64，实际执行 x64 MSVC target，完整流程 | **通过**：Rust 总计 1974 通过、0 失败、4 项既有忽略，含 1846 项 lib、2 项 ConPTY、原真实代理测试和此前未执行的 9 项 usage evidence；format/Clippy、25 项 Python、PowerShell 契约、CLI build 与两项 smoke 通过 | `target/windows-launcher-fix-2026-10-02/windows-full-x64/cc660dcbd73742d8a8b086d29bc781ee/{result.json,verify.log,interactive-context.json,task-cleanup.json}`；干净提交 `3fa8f28` 与上述 source ZIP；`2026-10-02T14:51:30Z`—`14:59:29Z` |

完整命令：

```sh
# macOS wrapper 记录前后构建输入，执行 format、Clippy 和 update 定向检查。
python3 -B target/windows-launcher-fix-2026-10-02/verify_native.py
# wrapper 使用的环境及完整原生入口：
export CARGO_TARGET_DIR=/Users/user/Workspace/codex-usage-monit/target/review-lock-storage
export CARGO_BUILD_BUILD_DIR=/Users/user/Workspace/codex-usage-monit/target/review-lock-storage-build
export CARGO_NET_OFFLINE=true
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
sh scripts/verify-unix.sh --filter update

# Windows launcher 定向检查；临时 InteractiveToken 任务使用登录用户及其私有 TEMP。
python3 -B target/orchestration-convergence-2026-10-02/verify_windows_interactive.py \
  --interactive-user 'WIN-MM0JRLGM2Q3\user' \
  --python-dir 'C:\Tools\codex-usage-monit\python-3.13.16-arm64' \
  --private-temp 'C:\Users\user\AppData\Local\Temp' \
  --toolchain-home 'C:\Users\user' \
  --pwsh-path 'C:\Tools\codex-usage-monit\powershell-7.6.5-arm64\pwsh.exe' \
  --target x86_64-pc-windows-msvc --timeout 900 \
  --focused --test-filter launcher \
  --output-dir target/windows-launcher-fix-2026-10-02/windows-retry-focused

# 最终干净提交的 Windows 完整检查。
python3 -B target/orchestration-convergence-2026-10-02/verify_windows_interactive.py \
  --interactive-user 'WIN-MM0JRLGM2Q3\user' \
  --python-dir 'C:\Tools\codex-usage-monit\python-3.13.16-arm64' \
  --private-temp 'C:\Users\user\AppData\Local\Temp' \
  --toolchain-home 'C:\Users\user' \
  --pwsh-path 'C:\Tools\codex-usage-monit\powershell-7.6.5-arm64\pwsh.exe' \
  --target x86_64-pc-windows-msvc --timeout 1800 \
  --output-dir target/windows-launcher-fix-2026-10-02/windows-full-x64
```

Windows 定向和全量 run 的实际身份均为 `WIN-MM0JRLGM2Q3\user`、session 1，Rust 1.97.0，使用私有 TEMP；任务已删除且无残留进程。全量的 guest 结果为 `passed`、`scope=full`，`sourceDirty` 的 tracked/untracked 均为 false，日志完成至版本输出及 offline JSON snapshot smoke。Python 中的 Windows installer/setup/bootstrap 契约均执行 PowerShell 5.1/7；独立 verification、development launcher、permission repair 脚本契约在 PowerShell 5.1 分别通过 78、17、10 项。

四项忽略为三项既有测量用例及需要独立旧构建的 `different_build_running_portable_launcher_keeps_proxy_protocol`；PE overlay 真实代理测试已经通过，不能替代这个可选的不同构建验收。中途 run `88691bc45f7a408f8f11ee9f94be377e` 因 UTM 控制通道返回无关旧日志错误而取消、未启动 Rust 测试；延迟创建的唯一任务随后核对并清理，保留 `target/windows-launcher-fix-2026-10-02/handle-diagnostic/88691bc45f7a408f8f11ee9f94be377e/{result.json,result-recovered.json,task-cleanup-recovered.json}`，不计作通过或产品测试失败。

本次跟进未执行新的 Linux 全量流程、hosted CI、Linux x64/musl 发布验证、真实 SSH、用户真实历史迁移或性能基准、服务部署。7.1 的 Unix 完整证据仍绑定 `1887267`；本次修改均限于 Windows 代码、Windows 专用回归及测试 feature，另执行了上述 macOS 定向检查。该跟进批次的 Windows 清理阻塞已解除，`3fa8f28` 的 Windows 默认完整流程通过；随后文档提交 `10739bc` 未改变该批已验证构建输入。本轮存储收敛另行验证，不能沿用这次通过结论。

### 7.3 SQLite 唯一后端与按需重建

2026-10-03，用户明确追加决定：保留旧不可重建的配额采样；来源策略、编号高水位及未完成来源删除意图作为非派生控制状态保留。移除其余旧派生历史迁移与 File/V1 适配，继续保留完整 C 去重/facts。基线为 `10739bc`；本轮实现已冻结，最终平台结果与源码绑定信息见下文。本节不引用前两批通过记录替代本轮检查。

| 工作包 | 本轮状态 |
| --- | --- |
| S0 | 最新数据保留/重建范围已记录，A 退役继续不实施 |
| S1 | 沿用已实现的 SQL/core 安全与事务基础；schema version 2、新初始化 receipt、旧 schema 1 开发库拒绝和失库拒绝均已实现并通过定向回归 |
| S2—S4 | SQL 本地数据族、远端聚合/facts 与一致快照继续保留；旧 redo/页补偿/COW 适配已删除，业务回归使用真实 SQL，精确 facts/proof、复制/分叉/conflict 与原子发布保留 |
| S5 | 以 `sqlite_history_initialization` 和最小 `sqlite_retained_state` 替代全量导入；仅保留配额，以及用户来源策略、编号高水位和未完成来源删除意图等控制状态，初始化 7 项回归覆盖有界保留、回滚重试、中断恢复、双 privacy fence、删除意图与失库拒绝 |
| S6 | CLI/TUI/recorder/同步改为从构造起绑定 SQL，File/V1 fallback 和旧格式业务迁移测试已移除；生产已无旧引擎消费者；损坏/丢失 ownership 不重造 epoch，仅保留本进程只读内存视图 |
| S7 | 定向回归、独立复核及 macOS/Linux/Windows 完整检查全部通过；测试快照与最终非 Markdown 输入逐文件核对，提交后的 hosted 检查点另行绑定该提交 SHA |

当前接入边界见 [history_application](../src/history_application.rs)、[history_query](../src/history_query.rs)、[history_runtime](../src/history_runtime.rs)、[SQLite 初始化](../src/sqlite_history_initialization.rs)和[最小保留状态](../src/sqlite_retained_state.rs)。只读内存降级保留；Settings 持有的 `SourceHistoryStore` 从初始绑定即为 SQL，消除 File 副本在后续 SQL 激活后继续读取旧来源策略的路径。查询与 TUI projection 缓存继续监测两种 privacy 的 committed stamp，并加入远端 SQL 发布编号和实际 GC 删除的 profile 发布编号；增量页保持相同 generation、独立 GC 未增加本地 observation 时也须失效，不以时间戳代替发布编号。无数据变化的提交或 GC 不因此推进发布编号。本机配额导出仅在 SQL Active 下读取本机 account，receipt 与 account 位于同一 SQL 快照，并检查外部 ownership 前后未变；缺库、缺 receipt、V1/Migrating 均不会读取旧 JSON 或伪造空配额。正常尚未使用 recorder 的无库/未初始化来源允许空配额。

最终本地完整检查均以 `10739bcf5f2ecf48dc737776c91893de1fb6b6ef` 加本轮 dirty snapshot 为输入，Rust 1.97.0。三平台测试期间源码冻结；最后仅补本文档证据。macOS 的非 Markdown 输入摘要为 `c28a8b5d031948c19a6f9f770f6b0121f204b68097de3ed26656a431e6feb34a`，前后相同；提交前再次逐文件核对（包括 SVG 和其他构建资源），核对记录保存为 `target/sqlite-demand-rebuild-2026-10-03/committed-source-reconciliation.json`，将本节所在提交与测试输入绑定。

| 平台及运行标识 | 完整结果 | 快照与证据 |
| --- | --- | --- |
| macOS 15.7.2 ARM64；`macos-full-20261003T015207` | exit 0；Rust **1905 passed / 0 failed / 3 ignored**，包含 PTY 2 项；format、Clippy、Python 契约、gallery、安装、构建及 offline CLI smoke 通过 | `target/sqlite-demand-rebuild-2026-10-03/macos-full-20261003T015207/{result.json,verification-summary.json,source-before.json,source-after.json,verify-unix.log}` |
| Docker Linux 原生 ARM64；`20261002T175220Z-arm64-27300` | exit 0；Rust **1902 passed / 0 failed / 3 ignored**，包含 PTY 2 项；完整 Unix verification 同样通过 | snapshot SHA256 `b1e3d90af86695628ff2b01cd5991cb7776029b127ef2401dc275ad909b10b1c`；摘要 `target/sqlite-demand-rebuild-2026-10-03/linux-full/{result.json,source.json,verification-summary.json}`；完整日志 `/Volumes/File/codex-usage-monit-docker-build/runs/20261002T175220Z-arm64-27300/verify.log` |
| UTM Windows ARM64 guest，实际运行 x64 MSVC 程序；`4b92ec06118547f5a16c2069fbf96950` | `passed / scope=full / exit 0`；Rust **1858 passed / 0 failed / 4 ignored**，包含 ConPTY 2 项，portable launcher 真实代理回归通过；format、Clippy、Python、PowerShell 5.1/7 契约、构建及 offline CLI smoke 通过 | source.zip SHA256 `cf25479d6a5143f2a8a2238c4ca923655861255b0ea970f1d9118d318778e5a5`；`target/sqlite-demand-rebuild-2026-10-03/windows-full-x64/4b92ec06118547f5a16c2069fbf96950/{request.json,result.json,verify.log,host-invocation.json,interactive-context.json,task-cleanup.json,final-source-reconciliation.json}` |

完整命令：

```sh
# macOS wrapper 对 checkout 前后取证，内部执行 sh scripts/verify-unix.sh。
# Cargo 环境：CARGO_NET_OFFLINE=true，
# CARGO_TARGET_DIR=/Users/user/Workspace/codex-usage-monit/target/review-lock-storage，
# CARGO_BUILD_BUILD_DIR=/Users/user/Workspace/codex-usage-monit/target/review-lock-storage-build。
python3 -B target/sqlite-demand-rebuild-2026-10-03/verify_native.py

# Linux runner 在 Docker 中验证隔离快照。
sh scripts/test-linux-docker.sh

# Windows 使用现有 interactive wrapper，验证完整快照和原生 Windows/ConPTY 行为。
python3 -B target/orchestration-convergence-2026-10-02/verify_windows_interactive.py \
  --interactive-user 'WIN-MM0JRLGM2Q3\user' \
  --python-dir 'C:\Tools\codex-usage-monit\python-3.13.16-arm64' \
  --private-temp 'C:\Users\user\AppData\Local\Temp' \
  --toolchain-home 'C:\Users\user' \
  --target x86_64-pc-windows-msvc \
  --pwsh-path 'C:\Tools\codex-usage-monit\powershell-7.6.5-arm64\pwsh.exe' \
  --timeout 1800 \
  --output-dir target/sqlite-demand-rebuild-2026-10-03/windows-full-x64
```

Windows 身份为 `WIN-MM0JRLGM2Q3\user`、session 1，非 SYSTEM；私有 TEMP、交互上下文和清理结果均已取证，任务已删除、无残留进程。Unix 各执行 80 项 pipeline 契约（其中 6 项 Windows 专属用例按平台跳过）及 10 项 runner 契约。Windows Python 25 项通过，PowerShell verification、development launcher、permission repair 分别通过 78、17、10 项。三平台默认忽略项均包括 `prepare_synthetic_history_measurement_fixture`、`benchmark_real_codex_cache`、`synthetic_history_sample`；Windows 另忽略需要独立旧构建的 `different_build_running_portable_launcher_keeps_proxy_protocol`。这些跳过项不计入 passed。

此前本地失败均先诊断再修复：第一轮因重命名后的 import 排序止于 format；第二轮 Unix 的 quota exporter 测试提前写 SQL、未建立 ownership，修正夹具并补上生产配额读取的 Active/receipt/失库回归；Windows 则止于 Unix 专用辅助函数未加平台限定导致的 Clippy dead-code，修正 `cfg`。更早的 Darwin timeout 夹具在 stdin EOF 后立即退出，造成进程组清理竞态；改为同 PID 等待父进程终止，生产清理逻辑未改，20 次定向重复及 5 项相关回归通过。失败证据均保留在本轮 `target/sqlite-demand-rebuild-2026-10-03` 下，最终上表完整检查使用修复后同一批代码。

定向业务回归已覆盖 SQL 初始化与配额保留、quota/weekly、复制/分叉/conflict、来源策略与未完成删除、只读/损坏、CLI/TUI/recorder/remote；macOS 和 Windows GNU 交叉目标的 all-target Clippy 也通过，交叉检查仅为补充，不代替原生 Windows 结果。上述完整结果为本地证据；提交并推送后，通过 `python3 scripts/run-ci.py --local-results '…'` 执行一次 hosted 集成检查点并核对其 `headSha`。未新增性能承诺；真实用户历史或性能基准、真实 SSH、服务部署、断电实验及 Linux x64/musl 发布验证未执行。

## 8. 参考

- [此前提案](refactoring-proposal-2026-09-26.zh-CN.md)、[审核](refactoring-review-2026-09-26.zh-CN.md)、[执行与验证记录](refactoring-execution-plan-2026-09-26.zh-CN.md)。
- [SQLite WAL](https://www.sqlite.org/wal.html)、[SQLite 发布记录](https://www.sqlite.org/changes.html)、[SQLite 类型](https://www.sqlite.org/datatype3.html)、[SQLite 损坏规避与锁](https://www.sqlite.org/howtocorrupt.html)。候选依赖必须重新核对实际链接版本。
