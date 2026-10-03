# Windows ConPTY 初始数据超时诊断（2026-10-03）

## 1. 已确认的现象

`real_tui_pty_handles_keyboard_mouse_search_resize_and_exit` 的失败发生在等待 fixture 数据出现的原有 8 秒门限，ConPTY 本身已成功创建并输出首帧。保留这个失败判定，再单独观察最长 30 秒，当前开发二进制在 **16.086 秒**出现所需数据，随后 `q` 正常退出。因此这是初始历史准备耗时超出门限，不能称为 ConPTY 创建失败或进程死锁；晚到的数据也不能把原测试改记为通过。

隔离源码副本的细分计时进一步定位到 **SQLite 访问路径反复进行 Windows 完整祖先目录检查**：一次 stage/load 有 39 次数据库 open、190 次数据库 validate，调用完整路径 reparse 检查 9,398 次；后者耗时约占这轮 stage/load 的86%。revision 前后核对和首次同步 GC 放大了这种固定成本。详细计数及其重叠边界见第5节。

首次 fixture 只有 1 个 rollout 文件、1,034 bytes、6 行、1 个 task、1 个 turn、1 个 call，生成 2 个待写历史 bucket、0 个 weekly point、0 个 digest。没有远端配置，offline 不采样账户。旧 SQLite CLI 同条件也在原 8 秒门限失败，且 TUI、history、rollout 代码未随安全 bootstrap 修改，因此当前证据不支持把它归因于新的 Windows 安装 bootstrap 调用。

## 2. 原样二进制的时间和进程证据

| 阶段 | 实测 | 含义 |
| --- | ---: | --- |
| 启动首帧（产品 startup trace） | 243.109 ms | 产品首次渲染完成，ConPTY 输出可用；并非单测 ConPTY 创建耗时 |
| 观察器消费到首帧 | 284.150 ms | 与产品首帧计时起点不同，不能混用 |
| rollout 开始（operation 起点后） | 2.569 s | 日志扫描前已有初始化固定开销；不是 activation 自身 duration |
| rollout scan | 5.274 ms | 完成扫描、解析和 materialize；非扫描卡住 |
| history stage | 193 µs | 内存 staging 开销很小 |
| history record | 5.291 s | 首次立即 flush，没有等待 5 秒 batching |
| history load | 7.210 s | 主要加载耗时远大于业务行数 |
| stage/load 合计 | 12.501 s | `writeFailed=false`、`queryFailed=false`，最终正常完成 |
| query 内 source families load | 238.922 ms | 已包含在下行 query 中，不能相加 |
| query 内业务 projection | 357.395 ms | 加载总耗时多数在其外围 |
| 本机 quota load | 54.986 ms | 0 条采样；本轮不涉及远端配额 |
| 初始数据可见 | 16.086 s | 超出原 8 秒门限，失败保留 |
| 发送 q 后退出 | 816.023 ms / exit 0 | 没有强杀 |

受控 PID 56184 在约 16.089 秒的累计 CPU 为 kernel **13.734 秒**、user **1.188 秒**，约等于单核 92.7% 的执行时间。Win32 I/O counters 为 read 458 次 / 986,857 bytes、write 143 次 / 132,509 bytes、other 840,954 次。`other` 是 Windows 的其他 I/O 类别，**不能直接当作 metadata 调用计数**；持续内核 CPU 和其他 I/O 支持大量系统调用的候选，反对把全部时间解释为静止锁等待。它们不能单独证明杀毒软件责任。

## 3. 静态调用链与诊断边界

`collect_initial_refresh_completion` 完成 deferred runtime、rollout 采集后调用 `stage_and_load_history_selected_with_mode`。进入 `history.stage_load` 时已经获取 TUI 的 history mutex。首次 incremental staging 的 `last_staged_flush_attempt=None`，立即尝试写入。本 fixture 的 cwd 是 `/work/codex-usage-monit`，在 Windows 上不是绝对路径，项目身份直接标记 Ambiguous；digest 的 `projectKeys=0`，后续 descriptor 为空，不会执行 Git evidence 的超时预算。这仅排除本 fixture 的 Git 探测开销。

当前最明显的固定成本是数据库边界与 revision 读取的叠加。`history_projection_revision` 为一致性连读两次，每次分别获取 initialization receipt、两个 privacy writer 的 local revision、GC revision、facts revision、source metadata。首次 projection 前后各执行一次，在没有远端的情况下也至少产生 **24 次独立顶层 SQLite read/open**，尚未计入写入、GC 和业务查询。每次 open、事务边界及嵌套 facade 又校验目录、数据库/WAL/SHM 的权限、reparse、文件身份和 hard link；Windows 路径校验会反复遍历祖先目录并打开 handle。

首次写入还同步检查 GC，包括 metadata、progress、retention clock 等操作，并持有 writer lease。SQLite busy timeout 为 250 ms；OS 文件锁是另一层机制，不能用这个 timeout 保证所有锁都有界，也不能在没有等待计时的情况下排除它。主 trace 只在 stage/load 完成时写入三个汇总耗时，原 8 秒失败日志不能细分这些阶段。

`Finalizing usage snapshot…` 是后台加载期间保留的阶段文案，不表示 rollout materialize 仍在执行。`startup.ready` 表示首帧已经渲染；`tui.initial_data_ready` 和 fixture 行可见才表示这项交互测试需要的初始数据准备完成。这三个状态应分开解释。

## 4. 源码、命令和证据绑定

原样观察基线为分支 `codex/sqlite-history-storage`、HEAD **`deec0256fe8d625cdbab580ec0f19f90c27eb35a`**，开始时工作树 clean。Windows 11 build 26200、AMD64 / `x86_64-pc-windows-msvc`，Rust 1.97.0、PowerShell 7.4.1。只用合成 fixture 和私有 state/config/cache，未读取正式历史、auth，未修改正式 recorder、sshd 或安全软件策略。

- 原开发 CLI：`D:/Workspace/codex-usage-monit/target/debug/codex-usage-monit.exe`。
- 冻结 CLI：`D:/Workspace/codex-usage-monit/target/conpty-startup-analysis-2026-10-03/observer/current-cli.exe`。
- 两者 SHA256：**`252c86e0e3ccae7c25f0ccb63c129bc2dc84c38fb93cc8737c57f6dc56ea6f94`**。
- `remote-agent info --sha256` exit 0，build ID **`1bc17f0a7efd0298f73451c4eed111bb2aad5a904a659fd16ff482f0f72ede71`**、schema 1、protocol 5、version 0.5.2。
- 原 `tests/tui_pty.rs` SHA256 **`c7cd42a85e9876ebbe767b704700849bc5cea94ca18bea36dbe50d240eab1edd`**，前后不变。
- 私有 TEMP：`C:/Users/Ghost/AppData/Local/Temp/sr-20261003-5257353c`；本轮受控 fixture `.tmpa8mLUK` 保留，进程已正常退出。

UTC 10:51:22–10:51:44，独立 observer 用现有 Cargo dependency rlib 编译，compile exit 0；新 observational test exit **101**，以保留原 8 秒失败状态。它没有替换原测试，也没有运行整个 suite。产品子进程命令为以下命令加观察器的四个私有日志路径：

```text
current-cli.exe --codex-home D:/Workspace/codex-usage-monit/tests/fixtures/codex-home/normal --days 3650 --offline --no-rollout-cache --log-level debug --trace-log <private operation.jsonl> --startup-log <private startup.jsonl> --log-file <private application.jsonl> --perf-log <private perf.jsonl>
observer.exe --exact observe_controlled_conpty_initial_data_8s_and_30s --nocapture --test-threads=1
```

完整实际 rustc/test 命令、时间、状态及文件哈希在 `D:/Workspace/codex-usage-monit/target/conpty-startup-analysis-2026-10-03/observer/execution.json`。产品 argv 和 state/config/cache 环境设置在相邻 `observer.rs`，本轮 private root 为 `C:/Users/Ghost/AppData/Local/Temp/sr-20261003-5257353c/.tmpa8mLUK`；日志分别位于这个 root 下的 `operation.jsonl`、`startup.jsonl`、`application.jsonl`、`perf.jsonl`，state/config/cache 为其同名子目录。相邻 `logs/observer.json` 保存按秒 CPU/I/O 采样和门限结果，`logs/{operation,startup,perf,application}.jsonl` 保存产品日志；`run-observer.py` 可审阅。只有受控路径及合成数据的诊断日志，没有复制数据库或 monitor 身份。

本次是失败定位，不是修复后的回归。未改生产源码、原测试门限或安全检查，未运行新全平台检查、CI、push、tag 或正式部署。原 Windows 完整批次的失败仍保留；此前更早的成功测试不能替代本轮结果。

## 5. 隔离计时版定位结果

从 deec0256 的 tracked `git archive` 创建源码副本，仅加少量 content-free 子 span，以及 RAII + relaxed atomic 的累计计数。保留原业务分支、所有安全校验、SQL、锁和测试门限；没有改生产 checkout 的 Rust 源码。它使用独立 Cargo target，不覆盖原样 CLI，只复用非产品 dependency cache。构建 exit 0 / 56.47秒，`remote-agent info --sha256` exit 0。

- 源码 archive：`target/conpty-startup-analysis-2026-10-03/instrumented/source-deec025.zip`，SHA256 **`55a4f99d9dbc4a0e365d9a8cad2421209218ea122aee214ca68a54872af1c2b5`**。
- 诊断 patch：同目录 `diagnostic.patch`，SHA256 **`985e159261d9aa0c0deb24cf0e9ba68e2fb9db6b2fc3fed9876f51db8428b79a`**；六个文件为 lib、tui、history_runtime、source_history/database、source_identity 及新 diagnostic_timing。
- 实际 binary：`D:/Workspace/codex-usage-monit/target/conpty-startup-analysis-2026-10-03/instrumented/cargo-target/debug/codex-usage-monit.exe`。
- SHA256 **`4c0cc545abac9215959686a87a71d3c639640e5f30dcd7870b822bfca476c248`**，build ID **`8075d7909b931801f0837fa9eb77ecbc0845541834e1b5bf63b2c2d3d938b705`**，同为 version0.5.2 / protocol5 / x86_64-pc-windows-msvc。

同 fixture 的第二轮 observer 于 UTC10:58:27–10:58:42执行，compile0 / test101，子进程 PID8712 正常 q 退出0，没有强杀；私有 root 为 `C:/Users/Ghost/AppData/Local/Temp/sr-20261003-5257353c/.tmpo5WNFg`。产品首帧205.299ms、observer消费首帧242.155ms，数据在10.448秒出现，仍超过8秒。运行时延与原样的16.086秒不同，可能受环境及缓存影响；**这一差异不是修复收益**。两轮 other I/O 840,954 / 840,845 次接近，业务及校验路径保持相同。

| 诊断 span | 实测 | 边界 |
| --- | ---: | --- |
| history.stage_load | 7.705024 s | 整体 |
| write | 3.244065 s | 含下面 lease、manifest、authority、local write、GC |
| writer lease 获取 | 43.633 ms | 含权限和身份校验；并非单测 File::lock 的等待时间，但长锁等待不是本轮主因 |
| runtime manifest | 117.304 ms | 写入前 manifest / receipt 验证 |
| runtime authority | 101.250 ms | 授权及 writer 创建 |
| local write | 1.315319 s | 源锁、revision 预留、记录准备和发布 |
| GC 检查 | 1.343129 s | 首次同步检查，不是扫描 rollout |
| cache and load | 4.460422 s | 含下方 revision 和 query |
| revision before | 1.598582 s | 双读一致性检查 |
| query | 922.738 ms | 外层完整 runtime query，范围大于原 trace 的 `history.v2.query` |
| revision after | 1.741968 s | 双读一致性检查 |
| recorder health | 109 µs | 无 recorder 延迟 |

下面是 stage/load 起止之间的**进程级累计**，函数内 RAII 记录，包括嵌套工作，因此各行 **inclusive 且可重叠，不应相加**。本轮只有受控 TUI；这些数字也不应推广为所有真实历史的精确调用次数。

| 边界 | 调用次数 | 累计耗时 | 解释 |
| --- | ---: | ---: | --- |
| HistoryDatabase::open | 39 | 2.920131 s | 含 open 内的校验 |
| OpenedDatabase::validate | 190 | 5.157483 s | 含目录、文件、身份及 side-file 校验 |
| transact 的 operation closure | 67 | 1.985649 s | 含 nested facade、其校验及 SQL，不是纯 SQL 时间 |
| COMMIT / RELEASE SAVEPOINT | 45 | 43.034 ms | 含只读事务和 savepoint release，不等于45次刷盘；没有显示秒级 durable commit 停顿 |
| reject_windows_reparse_components | **9,398** | **6.624327 s** | 每次从叶子逐个 `path.ancestors()` 查询至盘根；次数是完整路径检查次数，不是各祖先的 metadata 次数 |
| validate_windows_private_handle | 4,698 | 340.373 ms | ACL / owner 验证本身远小于反复路径遍历 |
| windows_current_user_sid | 4,700 | 23.815 ms | 已包含于 ACL 等上层，不重复加总 |

6.624327 / 7.705024 ≈86%。结合原样进程的高 kernel CPU、极小输入和同量级 other I/O，主瓶颈已定位为反复的路径安全校验与元数据系统调用，数据库细粒度 facade、revision 双读及 GC 又将其放大。不是业务记录聚合或读取大量日志耗尽预算，也没有证据表明只是等锁。没有测量文件系统过滤驱动各自耗时，**不能据此认定杀毒软件是根因**；没有关闭防护作对照，也没有再运行旧 EncodedCommand 测试。

原样及诊断观察都保留独立 frozen CLI、harness、execution.json 和日志。新证据目录为 `D:/Workspace/codex-usage-monit/target/conpty-startup-analysis-2026-10-03/instrumented-observer`，其 `diagnostic-summary.json` 记录全部阶段和累计字段、instrumentation/patch/manifest SHA。私有 fixture root 及 CPU/I/O 原始采样在 `logs/observer.json`。完整 build 命令在 `instrumented/binary.json`，实际复用观察命令为：

```text
D:/Dev_Kits/Python/Python312/python.exe -B target/conpty-startup-analysis-2026-10-03/observer/run-observer.py --binary D:/Workspace/codex-usage-monit/target/conpty-startup-analysis-2026-10-03/instrumented/cargo-target/debug/codex-usage-monit.exe --out target/conpty-startup-analysis-2026-10-03/instrumented-observer --identity-json D:/Workspace/codex-usage-monit/target/conpty-startup-analysis-2026-10-03/instrumented/binary.json
```

## 6. 后续修复方向与验收要求

首先应减少相同请求内反复打开数据库和遍历相同祖先路径。例如把一个 revision 读取所需的多类数据放进一个短 SQLite read snapshot，合并同一事务内重复的校验工作；仍需保留 ownership/receipt 的前后 fencing、路径替换检测、ACL、文件身份、hard-link、WAL/SHM 校验语义，不能长期缓存信任结果或关闭防护来获得速度。首次 GC 的固定成本和初始 projection cache 的 revision 探测也是适合继续优化的相邻调用者。

启动阶段文案应区分 rollout materialize 和历史写入/加载，以免完成扫描后仍显示 Finalizing 而误导定位。修复后需在同一受控 fixture、原8秒门限、相同隔离配置下验证，再补 Windows native ConPTY、路径替换/ACL/锁回归和相关历史回归；可以保留有界 readiness 握手，但不把单纯调大 timeout 作为根因修复。以上是修复前诊断；用户随后要求修复，实际实现和回归见第7节。

## 7. 修复与原门限回归

### 7.1 最小实现和安全边界

`history_application::history_projection_revision` 在原有未初始化 gate 后，用一个短 SQLite read snapshot 承载两次完整 revision 探测。receipt、local/other privacy、GC、facts、source policy/remote active 引用共享一致 SQL 视图；ownership 和 project mapping 仍分别双读，检测这些外部文件变化。每个 nested facade 的安全校验保持，读快照在返回调用者前结束，不覆盖采集、SSH、usage query 或写入。首次加载前后两次 revision 探测的顶层 open 从24次降为2次。

Windows 已打开数据库和 WAL/SHM 的校验使用 `validate_windows_bound_file`：先验证当前及已打开对象的 regular/no-reparse metadata，再保留 `ensure_opened_file_matches_path` 对 guard 和重新打开的当前路径各自完整的祖先前后遍历、owner/ACL 与 file ID 核对，最后通过同一 guard 核对 `nNumberOfLinks==1`。移除的是之前对这个已绑定对象重复打开路径并执行的完整校验；没有缩短祖先遍历范围或缓存权限信任。SQLite 打开前原始 `validate_file`、主库/WAL 的真实 SQLite HANDLE 比对、WAL/SHM deny-delete pins、guard 释放顺序和临时 `-journal` 校验均保留。Unix 数据库校验路径未改。

没有跳过初始 GC、降低 synchronous=FULL、增加旧历史兼容、恢复 A 方案或改变原 ConPTY 测试。生产 `src/tui.rs`、`src/source_identity.rs`、原 `tests/tui_pty.rs` 和 fixture 均未改。新增三项 revision 状态回归，以及 Windows 两项 bound-file 和一项真实 DACL 回归。

### 7.2 确定性回归和真实 ConPTY

| 场景 | 实际结果 | 证据与意义 |
| --- | --- | --- |
| 仅恢复旧 revision 函数，保留新并发 hook/回归及相同 DB 实现 | 编译0、exact test101、0pass/1fail；UTC12:59:59–13:02:04 | `red-revision/test.log`。其他线程提交后旧 probe 返回None，命中“a concurrent SQL commit must not split the revision snapshot”；不是编译或基础设施失败 |
| 新 revision 实现 | 3pass / exit0；6.34s | `projection-focused.log`。并发 SQL 提交时取得完整旧视图，下次 probe 见新revision及exclude策略；外部mapping变化仍返回None；未初始化/丢库不创建或重建；结束后正常query/write验证read snapshot释放 |
| Windows database 相关回归 | 11pass / exit0；1.15s | `database-focused.log`。包含真实 DB/WAL/SHM DACL 扩大为Everyone Read后的nested读拒绝、ACL恢复、rollback、旧提交保留；三种对象hardlink拒绝/rollback；不能用传入metadata批准另一个当前path；existing sideguard、跨进程锁和schema拒绝均通过 |
| 原样 `tests/tui_pty.rs` 两项真实 ConPTY | 2pass / exit0；9.35s是两项整体运行时间 | `conpty-original.log`。初始fixture、键盘、鼠标、搜索、resize、退出及remote setup错误显示都通过原各自门限；没有改8秒初始数据门限 |
| 独立观察修复后的非仪器 CLI，原8秒条件 | 首次即test0，ready6.411187s；q0 / 488.092ms，无kill | `observer/execution.json`、`observer/summary.json`。产品首帧210.370ms；history stage/load4.023904s = record2.497377s + load1.526068s；v2query150.334ms；writeFailed/queryFailed=false |

观察 PID53332，私有 root `C:/Users/Ghost/AppData/Local/Temp/sr-20261003-5257353c/.tmpLtoYFc`。kernel5.640625s、user484.375ms；other I/O471,911次，相比修复前的840,954次约减少44%。本次是同fixture的有界复现与回归，尚非正式历史或不同硬件的性能基准；原样baseline的16.086秒与修复版6.411秒均各自记录，不把仪器版10.448秒混作旧生产基线。offline账户缺失的snapshotPartial仍为true，历史写入/查询没有失败，它与测试超时不是同一状态。

所有本节日志位于 `D:/Workspace/codex-usage-monit/target/conpty-startup-fix-2026-10-03`。observer CLI冻结为 `bin/codex-usage-monit.exe`，SHA256 **`2f4c4af29a85f45af2224312efca63e3e08d3045c8520daa136495611a54d4f6`**，build ID **`f9bf593a9e8b33eece0dc099fc9cb5f8b2dd80db1ebe7b09f04198ac606f9451`**；version0.5.2/schema1/protocol5/x86_64-pc-windows-msvc，`remote-agent info --sha256` exit0。测试基线HEAD仍为deec0256，dirty生产源码只有 history_application、source_history/database 及新增 database/windows_security_tests；归一化构建源码身份和可复现逐文件快照分别记录，不以未跟踪的新测试缺失的普通git diff充当完整快照。

逐文件映射的 sourceHash **`6a1f4dd99d51957039daaf353c2a3af2a4314fd50c51aecdd21c888595731e72`**，manifest文件自身SHA256 **`cc15512c91d8f61ba9f9d8802caa835d212b13f674c8a3701d9776fe10947d90`**；`binary-identity.json`、`frozen-source-manifest.json`、`frozen-source/` 保存实际源码、dirty patch与完整 build命令。observer的pre/post sourceHash、原test、fixture、冻结CLI均稳定。其 execution SHA256 **`65daa5487344795cb21b5e18c7f022fe47a3cee476b48731335ef453ff532739`**；trace/perf/startup/app日志hash在summary.json。红灯专用恢复diff、source manifest及testbinary独立保存，不进入生产。

本地实际命令使用 PowerShell7.4.1、原私有TEMP、同一 Cargo target/build目录；observer测当前源码的冻结开发CLI，未运行旧编码探针：

```powershell
& scripts/windows/verify.ps1 -RepositoryPath D:/Workspace/codex-usage-monit -CargoTargetDir D:/Workspace/codex-usage-monit/target -CargoBuildDir D:/Workspace/codex-usage-monit/target -TestTempDir C:/Users/Ghost/AppData/Local/Temp/sr-20261003-5257353c -TestFilter projection_revision -SkipClippy -SkipSmoke
& scripts/windows/verify.ps1 -RepositoryPath D:/Workspace/codex-usage-monit -CargoTargetDir D:/Workspace/codex-usage-monit/target -CargoBuildDir D:/Workspace/codex-usage-monit/target -TestTempDir C:/Users/Ghost/AppData/Local/Temp/sr-20261003-5257353c -TestFilter source_history::database -SkipFormat -SkipClippy -SkipSmoke
# 以下原测试artifact来自本轮源码编译；TEMP/TMP固定到上述私有目录：
& target/debug/deps/tui_pty-5e0dfb58ea480a49.exe --nocapture --test-threads=1
& D:/Dev_Kits/Python/Python312/python.exe -B target/conpty-startup-fix-2026-10-03/runner.py --binary D:/Workspace/codex-usage-monit/target/conpty-startup-fix-2026-10-03/bin/codex-usage-monit.exe --out target/conpty-startup-fix-2026-10-03/observer --identity-json D:/Workspace/codex-usage-monit/target/conpty-startup-fix-2026-10-03/binary-identity.json
```

### 7.3 完整本地检查、提交与覆盖边界

修复提交为 **`2ac404617a60bbcd25e4908dba69f81113d139f9`**（`fix(history): reduce SQLite validation overhead during TUI startup`）。实际测试时是deec0256加第7.1节声明的三份dirty源码；提交只保存这三份已测试文件。逐文件源码身份保持第7.2节的sourceHash，三平台归一化build ID均为 **`f9bf593a9e8b33eece0dc099fc9cb5f8b2dd80db1ebe7b09f04198ac606f9451`**，不能将更早CI或验收文档第13节的失败批次替代本次结果。`commit-binding.json`逐一核对所有构建输入的Git blob与已测试checkout，并按build.rs规则计算同一build ID；这项只读绑定没有重跑或改动测试源码。

| 完整检查 | 平台、架构和工具链 | Rust实际计数 | 时间与完成状态 |
| --- | --- | --- | --- |
| 原生Windows `scripts/windows/verify.ps1` | Windows11 build26200 / AMD64；Rust1.97.0、PowerShell7.4.1 | **1878pass / 0fail / 4ignore**，13个target结果 | exit0；UTC日志创建13:05:24.497至最后写入13:18:50.536，这里是日志时间边界，不冒充独立进程计时 |
| 原生Mac `sh scripts/verify-unix.sh` | macOS15.7.2 / ARM64；Rust1.97.0 | **1923pass / 0fail / 3ignore** | exit0；UTC13:08:49.609807–13:11:38.976729，169.36s |
| Mac上的本地Docker `sh scripts/test-linux-docker.sh` | Linux / aarch64；Rust1.97.0，复用已验证的缓存toolchain | **1920pass / 0fail / 3ignore** | exit0；UTC13:08:49.609845–13:12:03.047307，193.43s |

三者均完成fmt、Clippy `--all-targets -D warnings`、所有Rust targets及CLI smoke；Windows包含原两项ConPTY，Mac/Linux包含原Unix PTY。所有新增revision回归、相关projection cache和平台对应的数据库权限/路径替换/锁回归通过。Windows四组Python contracts为13+4+2+6=25项，全通过，包含PowerShell5.1/7安装合同和Windows verification/dev/permission脚本检查；Mac/Linux各自的Python发现为80项（74pass/6skip），另有macOS runner contracts10pass。Rust ignored及Python skip如实保留，详情见原日志。没有再执行旧frozen-v4编码探针。

Windows完整实际命令为：

```powershell
& scripts/windows/verify.ps1 -RepositoryPath D:/Workspace/codex-usage-monit -CargoTargetDir D:/Workspace/codex-usage-monit/target -CargoBuildDir D:/Workspace/codex-usage-monit/target -TestTempDir C:/Users/Ghost/AppData/Local/Temp/sr-20261003-5257353c
```

完整entry没有Skip参数。CLI smoke针对原离线fixture，验证非空tasks、JSON和显式partial标记，按现有合同接受native0/2；该单条native退出码没有独立保存，已保存的是完整verification exit0和全部断言通过。这与SSH aggregate complete或facts完成没有关系。完整日志 `target/conpty-startup-fix-2026-10-03/windows-full.log`，SHA256及13个target计数在同目录 `validation-summary.json`。

Mac/Linux经现有真实SSH `local-mac`传输**本轮源码与合成测试日志**，在 `/Users/user/conpty-startup-fix-2026-10-03/source` 的私有副本运行，formal checkout只读。两次command的完整argv/env/UTC/exit/日志索引在 `D:/Workspace/codex-usage-monit/target/fix/platforms/source-summary.json`；Mac固定私有TMPDIR、CODEX_HOME/state/config/cache，Linux runner进一步使用自身isolated workspace和容器临时目录。源archive SHA256 **`c351cfebf8f7dd90bee5301ecac1a667864eacd2a30f75f7bf396134afab944b`**，origin为deec0256+声明的dirty，私有存储snapshot HEAD `c3f4df6f3d5c11554bb95295113d8727838361fb`仅供runner记录，不是分支实现提交。Docker实际snapshot SHA256 **`236b6c5829fe638d221cd3c018baec70bd13b55f54bc8cf2a6e6b56d866401d3`**；image ID **`sha256:72db5382ffef4a5dbf89a125ee07bf0073404cee718902ed4ef0d6da99b0261a`**。源码前后稳定。

完整检查完成后的实际开发二进制如下，均核对文件SHA256与 `remote-agent info --sha256`，info exit0、version0.5.2/info schemaVersion1/protocol5（info的schemaVersion不表示数据库schema；生产库仍为schema2）。Windows本次metadata命令独立设置空私有CODEX_HOME及state/config/cache，绝不读取正式auth。完整检查可能重链接同一源码，因此下面Windows字节SHA与第7.2节冻结observer CLI不同，build ID相同，两者证据各自绑定。

| 平台 | 二进制绝对路径 | SHA256 |
| --- | --- | --- |
| Windows x86_64-pc-windows-msvc | `D:/Workspace/codex-usage-monit/target/debug/codex-usage-monit.exe` | `df781ee83d54d0f26d88283a3fe13e385dda675012bee832e731c5d31e28992f` |
| Mac aarch64-apple-darwin | `/Users/user/sqlite-review-fixes-2026-10-03/cargo-target/debug/codex-usage-monit` | `b8e50e752b2a0f26161f0f42c6a06f6fe3ca418c5c868b3894be1640d9a43012` |
| Linux aarch64-unknown-linux-gnu | `/Volumes/File/codex-usage-monit-docker-build/target-linux-arm64/debug/codex-usage-monit` | `a0cd9933a1e73143fc3f5290cbff3d857982045b98d48410b6d0bd7031518dec` |

Mac原logs位于 `/Users/user/conpty-startup-fix-2026-10-03/logs`；Linux runner result位于 `/Users/user/conpty-startup-fix-2026-10-03/docker-build/runs/20261003T130850Z-arm64-24232/result.json`。15份合成日志已返回 `target/fix/platforms/logs`，逐文件SHA在 `evidence-files-manifest.json`。Windows总记录在 `target/conpty-startup-fix-2026-10-03/validation-summary.json`，完整二进制info与独立观察分别保存，不覆盖失败证据。最终只读审查确认短SQL快照不跨usage query/写入/SSH，Windows helper保持实时权限及对象验证。

上述target为本机Git忽略目录，证据不随分支自动同步；跨机接手需另行保存或复制这些记录并按SHA核对，不能凭另一端同名目录认定日志存在。

本次已修复并通过原ConPTY初始数据门限。未覆盖Linux AMD64、正式大历史/硬件矩阵性能基准，未为该build重跑非空SQLite双向SSH用量、quota、facts/proof或续页恢复；那些实机证据继续绑定验收文档各自原build。本轮没有停止或更新正式服务、auth、sshd、安全软件策略；未跑新CI、push、tag或正式部署。本机 `.agent/environment.local.md` 要求不主动触发CI，故采用上述完整本地检查收尾。
