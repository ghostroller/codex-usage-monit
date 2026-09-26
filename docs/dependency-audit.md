# Dependency advisory disposition

## 2026-09-26 local checkpoint

Audited the lockfile at source commit `62f6a3ab2a787759867379071ffd5fa1d00b4ba6`
using cargo-audit 0.22.2 on native Windows x86_64. RustSec database commit
`e2111519ba6d14a5da59a7b2e5c8083ae8a37c01` was last updated at
`2026-09-25T19:51:57+02:00` and contained 1,271 advisories. The complete lockfile
contained 247 dependencies; there were **zero vulnerabilities and zero advisory
warnings**, with no ignored advisories or target filters. Yanked-package checking
was enabled. No project dependency or lockfile changes were needed.

The lockfile SHA-256 was
`3d6e81337754deabd7eb5c25a440501dd7e414ba3a1b4d9b07362d26711bc298`.
The missing checker was installed into the ignored workspace directory with
`cargo install cargo-audit --version 0.22.2 --locked --root target/tools/refactoring-audit`;
it was not installed globally. The audit command was:

```powershell
target/tools/refactoring-audit/bin/cargo-audit.exe audit --deny warnings --db target/verification/refactoring-next-20260926/n3/advisory-db --json
```

The JSON report and execution record are in
`target/verification/refactoring-next-20260926/n3/audit.log` and `audit.json`.
The earlier offline installation attempt failed because the tool was not cached;
the subsequent fixed-version installation and network-backed audit succeeded.
This local audit did not trigger hosted CI and is not a new runtime test pass or
a guarantee against future advisories. The historical dependency changes below
remain unchanged.

## 2026-09-08 dependency changes

Checked on 2026-09-08 against RustSec database commit
`faedffd5118c1835e13cca3babb6059afb1eb8d0` using cargo-audit 0.22.2.
The review started at repository commit `8104cfc`; its supplied report used
`6efcede3ae80975ac1e4d055ae9f36539da2d1eb`.

| Locked dependency before | Advisory | Disposition |
| --- | --- | --- |
| lru 0.12.5 via ratatui 0.29.0 | [RUSTSEC-2026-0002](https://rustsec.org/advisories/RUSTSEC-2026-0002.html), unsound mutable iterator | Upgrade to lru 0.18.4 via ratatui 0.30.2 / ratatui-core 0.1.2; patched since 0.16.3. |
| lru 0.12.5 | [RUSTSEC-2026-0253](https://rustsec.org/advisories/RUSTSEC-2026-0253.html), panic safety in pop | Same upgrade; patched since 0.18.2. |
| paste 1.0.15 via ratatui | [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436.html), unmaintained | Removed by the Ratatui upgrade. This was a maintenance warning, not a demonstrated application vulnerability. |
| quick-xml 0.39.4 | [RUSTSEC-2026-0194](https://rustsec.org/advisories/RUSTSEC-2026-0194.html), quadratic duplicate-attribute checks | Upgrade to patched 0.41.0. Found by the complete audit, absent from the supplied review. |
| quick-xml 0.39.4 | [RUSTSEC-2026-0195](https://rustsec.org/advisories/RUSTSEC-2026-0195.html), namespace allocation | Same upgrade. The application uses Reader rather than NsReader; this does not change the decision to remove the affected release. |

The old Ratatui layout cache stores `(Rect, Layout)` keys and uses cache lookup
and insertion. This inspection does not establish an application path to the
affected mutable iterator or a key destructor that panics. No exploitability
claim is made; the affected dependency is removed regardless.

Crossterm is upgraded to 0.29 so the application and Ratatui backend share one
version. Ratatui enables only the existing Crossterm, layout cache and underline
color capabilities. The PTY fixture now answers cursor-position queries issued
by terminal initialization. The XML API replacement uses the same implicit XML
1.0 mode as the deprecated method.

Validation: locked all-target compilation and the complete macOS test suite,
including semantic TUI rendering and real PTY interaction, passed. The final
lockfile passed `cargo audit --deny warnings` with zero vulnerabilities and
zero advisory warnings. The dependency audit workflow checks all locked target
dependencies at manually requested CI checkpoints, before version-tag releases,
and on a weekly schedule. It can also be dispatched independently; ordinary
pushes and pull requests do not start it. See [testing policy](testing.md).
The workflow does not ignore advisories. These are point-in-time results, not a
claim about future advisories.
