# 配置锁并发测试稳定性修复与验证

## 问题与范围

合并复核针对 `4256ad79fbdfa5a17253a81ba4a0fb8b1c1c838f` 运行了
[CI 37133993216](https://github.com/ghostroller/codex-usage-monit/actions/runs/37133993216)。
macOS、Linux、依赖审计通过；Windows lib 为1749通过、1失败、1忽略。
唯一失败是 `current_host_guard_linearizes_local_phase_against_config_updates`：
共享配置锁释放后，等待完整配置更新结果的1秒期限超时。后续target未完成，不能将该CI称为完整Windows通过。

失败路径不访问SQLite。配置更新包含私有目录/ACL/文件身份校验、排他锁、CAS、
`sync_all`、Windows原子替换及发布后校验；产品没有完整操作必须1秒完成的契约。
原日志未定位具体耗时阶段，不能据此声称产品死锁或确定归因于ACL、磁盘或调度。
未改源码的定向诊断在macOS重复10次、UTM Windows执行x64目标1次均通过。

本次仅修改 `src/remote_sync.rs` 的 `#[cfg(test)]` 模块。生产代码前缀与4256ad7逐字节一致，
没有修改SQLite、同步协议、文件锁实现或ConPTY测试门限。

## 测试改动

- 配置共享锁在测试主线程的 `with_current_host` callback中持有。
  在启动更新线程之前，通过真实 `try_lock_exclusive_for_test` 要求排他锁返回 `Ok(None)`；
  I/O错误仍使测试失败。这一步直接验证锁保护存在。
- 去掉100ms“没有结果”的判据。更新线程的启动通知只证明线程已启动，不能证明其已经等待OS锁。
  callback返回即释放共享锁，不再依赖另一线程等待测试发送release消息。
- 更新完成后检查revision精确增长1、host修改为 `changed-alias`，并重新读取配置要求与更新结果一致。
- 相邻disable/remove及detached-source-purge测试统一使用30秒worker watchdog。
  mapping锁仍保持到配置修改成功；过时页面仍必须拒绝发布。
  新增排他try-lock探针确认mapping准备不会占住配置锁，并移除50ms调度睡眠。
  purge仍须在明确持有配置锁期间返回 `Busy`，移除250ms运行耗时断言。

30秒是测试诊断的watchdog，不是产品性能承诺，也不能取消任意阻塞的OS I/O。
mapping的准备通知仍在 `prepare_page` 前发送，不构成“已进入mapping锁等待”的严格握手；
purge主线程的try-lock调用本身也不受channel watchdog覆盖。这些fixture边界保留在审查记录中。

## 本地验证与源码绑定

测试基于4256ad7加唯一dirty源码 `src/remote_sync.rs`。
该文件SHA256为 `2fe14e08703f0b170d6bbae99e4e3e785eeddd00d7997a1d04a9805bb87d1c79`。
以下focused filter也匹配 `automatic_remote_sync::tests`，实际为remote_sync25项及automatic_remote_sync32项。
其他target的0项结果不算额外覆盖，focused没有执行ConPTY、CLI smoke或完整套件。

| 检查 | 环境 | 结果与证据 |
| --- | --- | --- |
| macOS focused | 原生ARM64，Rust1.97.0 | 57通过、0失败；另通过fmt及Clippy全部targets；`target/config-lock-test-fix-2026-10-04/mac-results.json` |
| Linux focused | Docker aarch64，Rust1.97.0 | 57通过、0失败；snapshot `0a3eea72a4ec3c362267b677489877f1c8744694092273a136287095803e8571`；`target/config-lock-test-fix-2026-10-04/linux/verification-summary.json` |
| Windows focused | UTM ARM64宿主执行x64 MSVC目标，SYSTEM，Rust1.97.0，pwsh7.6.5 | 57通过、0失败；run `0a39a4061b62445c8b49eaa486854d7a`；archive `21869c296d7c29ab48fcb701ee6ddd448fc3538b6fc36bc51373a28939780f22` |
| Windows完整检查 | 同一UTM ARM64宿主/x64目标，已登录user的管理员测试进程 | **1878通过、0失败、4忽略**，13个target；run `c378beb3ab404e1a820c254e6225dcba`，与focused相同archive；exit0 |

macOS完整命令及Cargo目录/UTC/sourceStable在上述JSON中：
`sh scripts/verify-unix.sh --filter remote_sync::tests`、
`cargo clippy --locked --all-targets -- -D warnings`、`cargo fmt --all -- --check`。
Linux为 `sh scripts/test-linux-docker.sh --filter remote_sync::tests`，复用现有镜像及toolchain，
日志在 `/Volumes/File/codex-usage-monit-docker-build/runs/20261003T162224Z-arm64-41213/verify.log`。
Windows使用标准UTM runner，完整argv及guest原始日志在本轮windows目录记录。

Windows完整检查通过fmt、Clippy全部targets、Python25项合约、全部Rust targets、原两项ConPTY、
`running_portable_launcher_with_different_bytes_passes_real_proxy_contract` 及离线CLI smoke。
UTC时间为2026-10-03 16:24:59.9056818至16:33:45.8222821；实际执行账户为
`WIN-MM0JRLGM2Q3\user`，非SYSTEM、administrator=true、session1。
本机标准Guest Agent以SYSTEM执行，缺少正常用户的Python/TEMP上下文，因此完整检查使用已有的
`target/orchestration-convergence-2026-10-02/verify_windows_interactive.py` 包装原runner：
以既有InteractiveToken启动唯一临时测试任务，为该进程选择已安装Python3.13.16和用户私有TEMP。
原runner的源码打包、archive/result/runId匹配、测试入口及日志校验保留；无filter、focused或Skip。
不改用户/系统PATH、全局执行策略、正式服务或真实应用状态。
`interactive-context.json` 记录实际身份与GetTempPath；`task-cleanup.json` 确认临时任务已注销，
processTreeStopped=true、remainingProcessIds为空、lastTaskResult=0。

提交前逐字节核对Windows完整检查archive和Linux已测试workspace的全部209份非Markdown tracked文件，
均与当前checkout一致，没有缺失或差异，记录为 `snapshot-binding.json`。
随后新增的本文仅为验证记录；Mac/Linux没有重跑不受本次测试代码影响的生产全量，
其既有完整证据不冒称本次新全量。新的托管检查将在包含测试修复及本文的已推送提交上运行，
本地结果不能替代其最终状态。

在独立源码副本中故意让 `with_current_host` 在callback之前释放共享锁，
新测试在“guarded local phase must hold its shared config fence”断言失败，exit101；
这证明保留了对锁保护缺失的确定性检测。正式源码未施加此修改。
该副本与主checkout曾共享本任务Cargo构建目录，后续一次运行复用了故意破坏锁的产物；
其失败日志保留为 `mac-green-shared-cache.log`，不作为正式源码失败或通过证据。
清理本任务crate产物后重新编译正式checkout，focused、Clippy、fmt全部通过，源码前后稳定。

本轮记录位于Git忽略的 `target/config-lock-test-fix-2026-10-04`；原始日志不会随分支自动同步。
此前生产代码全量和实机同步证据继续按[原测试报告](conpty-startup-timeout-analysis-2026-10-03.zh-CN.md#73-完整本地检查提交与覆盖边界)
及[实机验收报告](sqlite-real-machine-sync-validation-2026-10-03.zh-CN.md)各自的提交/构建绑定使用。
