# Snifrig cross-platform design (macOS first, Linux later)

Status: design only, no code. Written 2026-10-07.

## 1. What the Windows build does today

- One process (`snifrigd`), no children, idle priority, no GPU use. Reads `NtQuerySystemInformation` directly (raw-dylib ntdll) instead of spawning tools.
- Signals: kernel pool by tag with growth per hour (`Toke`, `SeAt`, `FMfn`, `WCsc`), top 25 processes by handle count and by private memory, commit charge, free RAM, paged/non-paged pool, process/token creation rate from kernel allocation counters, free disk.
- Alerts: growth rules need 5+ min of history, re-alert gap 2 h (`REALERT_SECS`), each alert says what grows, how fast, likely cause. History in a 1440-sample ring (24 h at 60 s).
- Self-budget guard, checked every cycle, exits on breach: working set over 48 MB, thread/handle growth, private growth over 8 MB, CPU over 1%. Measured: ~6 MB WS, 74 handles, ~0.01% CPU.
- Persistence: HKCU `...\CurrentVersion\Run` via `snifrig install` / `uninstall`.
- Tray (`snifrig-tray`, separate process) reads `status.json` only: green / red / gray. Left-click flyout shows CPU, RAM, GPU, VRAM with top 3 consumers each, via `snifrig --snapshot` using per-process GPU counters, collected only on open.
- `snifrig pause [MIN]` pauses fixing (default 60 min), monitoring continues (`mode.json`).
- `snifrig webhook URL|off` POSTs alert JSON (opt-in, e.g. n8n) via WinHTTP, 5 s timeouts.
- Data: `%LOCALAPPDATA%\snifrig` with `snifrig.jsonl` (1 MB cap), `alerts.jsonl` (256 KB cap), `status.json`, `webhook.txt`. Release profile: opt-level "s", LTO, panic=abort, strip. Only dependency: `windows-sys`.
- Note: README says "no network"; true except the opt-in webhook.

## 2. Architecture for porting

Keep the Windows file layout and JSON schemas identical so n8n consumers and the license-statement code do not fork.

- Split `lib.rs` (50 KB) into portable core (ring buffers, growth math, alert rules, jsonl/status/webhook payload, budget guard logic, license, pause) and a `probe` trait with `os/windows.rs`, `os/macos.rs`, `os/linux.rs` behind `cfg(target_os)`.
- Probe output is a neutral struct: `Sys { mem_total, mem_avail, commit_like, pressure, swap_used }`, `Vec<Proc { pid, name, rss, private, fds, threads, start }>`, optional `Gpu`.
- Webhook POST: Windows uses WinHTTP. Off Windows, prefer a tiny HTTP/1.1 client over `std::net::TcpStream`. For https see risk R3.

## 3. macOS probes

All are in-process C calls; no `ps`, `vm_stat` or `top` spawning.

| Signal | API | Notes |
|---|---|---|
| RAM, swap | `host_statistics64(mach_host_self(), HOST_VM_INFO64, ...)` giving `vm_statistics64` (free, active, inactive, wire, compressor_page_count, purgeable); `sysctlbyname("hw.memsize")`, `sysctlbyname("vm.swapusage")` | Compressor pages are the closest analogue of commit growth. |
| Memory pressure | `sysctlbyname("vm.memory_pressure")` (1 normal, 2 warn, 4 critical) and `kern.memorystatus_vm_pressure_level` | Readable unprivileged. Replaces "commit near limit". |
| Process list | `proc_listpids(PROC_ALL_PIDS)` or `sysctl KERN_PROC_ALL` | Cheap; ~600 pids. |
| Per-process memory | `proc_pid_rusage(pid, RUSAGE_INFO_V4)` field `ri_phys_footprint` (matches Activity Monitor "Memory"), plus `ri_resident_size` | Use footprint as "private memory". |
| Per-process fds (handle analogue) | `proc_pidinfo(pid, PROC_PIDLISTFDS, ...)` count; or `PROC_PIDTASKINFO` | Only own-user processes without root; root-owned return EPERM. Report coverage honestly. |
| Process churn | diff of pid + `pbi_start_tvsec` from `PROC_PIDTBSDINFO` between samples | Short-lived processes between samples are missed; no kernel counter like Windows. Mitigate with a 10 s fast sample only while an alert is open. |
| Disk | `statfs` / `getattrlist` volume available capacity | Use important-usage capacity, not raw free. |
| GPU util / VRAM | IOKit: match `IOAccelerator` services, read `PerformanceStatistics` dict: `Device Utilization %`, `In use system memory`, `Alloc system memory` | Apple Silicon has unified memory, so "VRAM" is system memory allocated to the GPU; label it that way. Works unprivileged (same data `ioreg -r -c IOAccelerator` shows). |
| GPU per-process | None public. Per-client breakdown exists as `IOAccelerator` child `AGXDeviceUserClient` objects with `IOUserClientCreator` pid string, but memory per client is not exposed reliably. | Flyout shows system GPU only on macOS; top-3 consumers by CPU/RSS instead. |
| Power, GPU freq | `powermetrics` needs root. `IOReport` (private) gives the same unprivileged (used by macmon). | Out of scope for v1; private API risk. |

## 4. Linux probes

| Signal | Source | Notes |
|---|---|---|
| RAM, commit | `/proc/meminfo`: MemAvailable, Committed_AS, CommitLimit, SwapFree, Slab, SUnreclaim, KernelStack, PageTables, Shmem | Committed_AS vs CommitLimit is the direct commit-charge equivalent. |
| Pressure | `/proc/pressure/{memory,cpu,io}` PSI, `some`/`full` avg10/60/300 | Readable unprivileged; needs kernel 4.20+ and PSI enabled. Best leak-adjacent early warning. |
| Per-process | `/proc/[pid]/status` (VmRSS, RssAnon, VmSwap, Threads), `/proc/[pid]/smaps_rollup` (Pss, Private_Dirty) | RssAnon is the "private" analogue; smaps_rollup is costlier, use only for top N. |
| fds | `/proc/[pid]/fd` entry count, `/proc/sys/fs/file-nr` | Other users' fd dirs need root or `CAP_SYS_PTRACE`; document. |
| Kernel slab (pool-tag analogue) | `/proc/slabinfo` is root only (0400). Unprivileged: `/proc/meminfo` Slab, SReclaimable, SUnreclaim | Option: optional root helper (systemd system unit or `setcap`) for per-cache growth; default off. |
| Churn | `/proc/stat` field `processes` (forks since boot), `/proc/loadavg` | Direct analogue of Windows creation counters; includes short-lived processes. |
| Disk | `statvfs` | |
| GPU NVIDIA | NVML via `dlopen("libnvidia-ml.so.1")`: `nvmlDeviceGetUtilizationRates`, `nvmlDeviceGetMemoryInfo`, `nvmlDeviceGetComputeRunningProcesses` (per-process VRAM) | Dlopen avoids a hard link and any dependency when no NVIDIA. `nvidia-smi` spawning is a fallback only, violates no-children rule. |
| GPU AMD | `/sys/class/drm/card*/device/{gpu_busy_percent,mem_info_vram_used,mem_info_vram_total}` | Unprivileged. Per-process via `/proc/[pid]/fdinfo/*` keys `drm-engine-gfx`, `drm-memory-vram` (kernel 5.19+). Intel i915/xe expose the same fdinfo keys. |

## 5. Persistence and tray

macOS:
- LaunchAgent plist at `~/Library/LaunchAgents/group.fungibility.snifrig.plist` (label placeholder, pick final reverse-DNS): `ProgramArguments`, `RunAtLoad` true, `KeepAlive` with `SuccessfulExit false` (so a budget-guard exit does not respawn-loop; add `ThrottleInterval` 60), `ProcessType` Background, `LowPriorityIO` true, `Nice` 10. Load with `launchctl bootstrap gui/$UID <plist>`, remove with `bootout`. `snifrig install` writes the plist and calls `launchctl` once (install-time only).
- Menu bar item without Dock icon: `LSUIElement` true in the helper's `Info.plist` (or `setActivationPolicy(.accessory)`). The tray runs as a separate bundle `SnifrigTray.app` inside the daemon's folder, matching the Windows split.
- Tray options: `tray-icon` (tauri) pulls `muda`, `objc2` family and an event-loop need (main thread); roughly 1 to 2 MB added and an AppKit run loop. Direct `objc2` + `objc2-app-kit` (NSStatusItem, NSMenu, NSImage template icon) is lighter and matches our need (icon plus ~6 menu items, poll `status.json` with a 5 s NSTimer). Recommendation: direct `objc2-app-kit` with only the needed feature flags. Fallback: `tray-icon` if objc2 menu code exceeds ~300 lines.
- Flyout: NSPopover is overkill; v1 uses an NSMenu with disabled text rows filled from `--snapshot`.

Linux:
- `~/.config/systemd/user/snifrig.service`: `Type=simple`, `Restart=on-failure`, `RestartSec=60`, `Nice=10`, `IOSchedulingClass=idle`, `MemoryMax=64M`, `WantedBy=default.target`. `snifrig install` runs `systemctl --user enable --now`. Headless servers need `loginctl enable-linger` for it to survive logout; document, do not do it silently. Fallback for non-systemd: XDG autostart `.desktop`.
- Tray: StatusNotifierItem over D-Bus (`org.kde.StatusNotifierItem` + `com.canonical.dbusmenu`). KDE, XFCE, Cinnamon, MATE, Budgie support it. Stock GNOME does not; users need the AppIndicator/KStatusNotifierItem Support extension. Without a host, degrade to desktop notification plus `snifrig status` CLI.
- Crates: `ksni` (pure-Rust SNI, zbus/async, no GTK) beats `tray-icon`'s AppIndicator path which requires GTK3 and libayatana. Both are heavy relative to the daemon, so keep the tray a separate optional binary and the daemon dependency-free.

## 6. Dependencies (keep binary small, idle near zero)

- Daemon, macOS and Linux: `libc` only (declares `host_statistics64`, `proc_pidinfo`, `proc_pid_rusage`, `sysctlbyname`, `statvfs`). It is declaration-only, zero runtime cost, mirrors our `windows-sys` choice. Hand-declare the few Mach/IOKit prototypes missing from libc via `extern "C"` and link `-framework IOKit -framework CoreFoundation`.
- Avoid `sysinfo` (broad, allocates per refresh, large), `libproc` crate (thin but drags extra deps and its own allocations; we need ~4 calls), `tokio`/`reqwest` (MBs, threads), `serde` (existing code hand-rolls JSON; keep).
- GUI: `objc2`, `objc2-foundation`, `objc2-app-kit` with minimal features (macOS tray only). Linux tray: `ksni`. Neither linked into the daemon.
- Profile: keep opt-level "s", LTO, panic=abort, strip. Expect daemon ~300 to 600 KB; target working set under 8 MB. On Apple Silicon use QoS `QOS_CLASS_BACKGROUND` via `pthread_set_qos_class_self_np` as the idle-priority equivalent; Linux `setpriority` + `ioprio_set` idle.
- Budget guard: keep 48 MB / 1% CPU ceilings; RSS read from `proc_pid_rusage(self)` / `/proc/self/status`.

## 7. GitHub Actions for macOS

- Runner: `macos-latest` is arm64 and now maps to macOS 15 (migration announced Jul 2025). The Intel `macos-13` image was retired in Dec 2025; an Intel label (`macos-15-intel`) may exist but should be verified before use. Avoid dependence: cross-compile x86_64 on the arm64 runner.
- Matrix: one job on `macos-latest` with `rustup target add aarch64-apple-darwin x86_64-apple-darwin`; build both with `cargo build --release --target <t>`; `lipo -create -output snifrig target/*/release/snifrig`; same for `snifrigd` and the tray binary. x86_64 binary is test-run only on Rosetta (`arch -x86_64`), if installed on runner.
- Package: `SnifrigTray.app` (Info.plist with `LSUIElement`, `CFBundleIdentifier`) plus CLI in a `.zip` or `.pkg`; release via `gh release`.
- Signing and notarization needs: paid Apple Developer Program membership; a "Developer ID Application" certificate exported as .p12; hardened runtime (`codesign --force --options runtime --timestamp`); `xcrun notarytool submit --wait`; `xcrun stapler staple` (apps and dmg/pkg; bare CLI binaries cannot be stapled, ship them in a zip/dmg or accept online check). "Developer ID Installer" cert only if shipping a .pkg.
- Secrets (names only, set by Josh, never invent values): `APPLE_DEVELOPER_ID_CERT_P12_BASE64`, `APPLE_DEVELOPER_ID_CERT_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_TEAM_ID`, `APPLE_NOTARY_KEY_ID`, `APPLE_NOTARY_ISSUER_ID`, `APPLE_NOTARY_API_KEY_P8_BASE64` (App Store Connect API key; alternative is Apple ID + app-specific password, `APPLE_ID` and `APPLE_APP_SPECIFIC_PASSWORD`). Also `KEYCHAIN_PASSWORD` for the temporary CI keychain.
- Unsigned fallback: ad-hoc `codesign -s -` works on Apple Silicon but users hit Gatekeeper; fine for dev builds only. Linux CI: `ubuntu-latest` with `x86_64-unknown-linux-musl` and `aarch64` via cross; static musl suits a dlopen NVML plan only if glibc is used, so ship gnu build for GPU support (see R4).

## 8. Alert rule mapping

| Windows rule | macOS | Linux |
|---|---|---|
| Pool tag growth (`Toke`, `SeAt`, ...) | No equivalent. Replace: compressor pages growth, wired memory growth, `vm.memory_pressure` level | No tags without root. Replace: SUnreclaim growth, Slab growth, optional root `slabinfo` per-cache |
| Paged/non-paged pool | wired + compressor | Slab + KernelStack + PageTables |
| Commit charge near limit | memory pressure warn/critical + swap used growth | Committed_AS / CommitLimit, PSI memory `some`/`full` |
| Free RAM low | free + inactive + purgeable | MemAvailable |
| Handle count per process | fd count (own-user only) | fd count (own-user without root) |
| Private memory growth | `ri_phys_footprint` growth | RssAnon / Pss growth |
| Process/token creation rate | pid-diff only, misses short-lived | `/proc/stat processes` delta, exact |
| Free disk | direct | direct |
| GPU/VRAM flyout | system GPU + unified memory only | NVML / amdgpu / drm fdinfo per-process |
| Self-budget, pause, webhook, status.json | direct | direct |

Alert wording tables (the "usually caused by" text, `lib.rs` line ~166) must be rewritten per OS; the Windows tag text is not reusable.

## 9. Phased plan

1. Core split and `probe` trait, Windows regression (jsonl/status schema unchanged): M.
2. macOS daemon probes (mem, pressure, procs, fds, disk) and alert mapping: M.
3. macOS CI, lipo universal binary, ad-hoc signed artifacts: S.
4. macOS LaunchAgent install/uninstall, pause, webhook over plain sockets: S.
5. macOS menu bar app (objc2), GPU via IOAccelerator: M.
6. Developer ID signing, notarization, stapling, release workflow: M (calendar time dominated by enrollment).
7. Linux daemon (procfs, PSI, statvfs) and systemd user unit: M.
8. Linux GPU (NVML dlopen, amdgpu sysfs, fdinfo): M.
9. Linux tray (ksni) with GNOME fallback; optional root slab helper: L.
10. TLS webhook (https) decision and implementation across OSes: M.

## 10. Risks

- R1: No pool-tag analogue; the product's headline feature does not port. Mitigation: re-position cross-platform builds around footprint/fd/pressure growth and say so in the README.
- R2: Apple signing and notarization is gated on Josh's paid Developer ID account; also fd and some rusage calls are blocked for other users' processes without root or entitlements. Unsigned builds will be blocked by Gatekeeper.
- R3: Webhook https needs TLS. WinHTTP and NSURLSession gave it free; Linux and a socket-only build do not. Rustls costs ~1 MB; plain http only is limiting. Decide early.
- R4: GPU per-process data is unavailable on Apple Silicon and depends on driver/kernel versions on Linux; IOReport and powermetrics-level data rely on private APIs or root. Static musl blocks NVML dlopen.
- R5: Process churn is sampled, not counted, on macOS, so spawn-storm alerts will undercount.

## Sources

- Linux PSI: https://docs.kernel.org/accounting/psi.html
- tauri tray-icon crate (platform, main-thread, Linux deps): https://github.com/tauri-apps/tray-icon
- Apple notarization overview: https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution
- GitHub macos-latest migration to macOS 15: https://github.blog/changelog/2025-07-11-upcoming-changes-to-macos-hosted-runners-macos-latest-migration-and-xcode-support-policy-updates/
- macOS 13 runner closing: https://github.blog/changelog/2025-09-19-github-actions-macos-13-runner-image-is-closing-down/
- macmon (sudoless Apple Silicon metrics via IOReport): https://github.com/vladkens/macmon
- libproc crate (wrapper over libproc.h): https://docs.rs/libproc
- GNOME legacy tray extension: https://www.omgubuntu.co.uk/2024/08/gnome-official-status-icons-extension
- StatusNotifierItem spec: https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/ (fetch blocked, cited from search results)
- Local: X:\snifrig\README.md, X:\snifrig\src\lib.rs, X:\snifrig\Cargo.toml
- Not verified online, from author knowledge: exact Mach/libproc struct fields, IOAccelerator PerformanceStatistics keys, nvml function names, amdgpu sysfs names, launchd keys, systemd directives.
