# Dependency advisory disposition

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
dependencies on pushes, pull requests and a weekly schedule; it does not ignore
advisories. These are point-in-time results, not a claim about future advisories.
