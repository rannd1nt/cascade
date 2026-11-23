# AGENTS.md

Instructions for anyone (human or agent) working on Cascade.

**Read order:** this file → `docs/architecture.md` → `docs/reliability.md` →
`docs/quality.md` → `docs/ipc.md` → `docs/spike-m0.md`.

**Internal design notes are local-only and are not distributed with this
repository.** They live in a companion directory registered in OpenCode as the
`cascade-docs` reference. Every `docs/...` path in this file is relative to that
reference root, not to this checkout; a fresh clone is expected to have none of
them. Keep this file as the single committed entry point.

If a rule here conflicts with a design doc, this file wins on *process*, and
`docs/architecture.md` wins on *design*. If code conflicts with docs, the code is
probably wrong — or the docs must be updated in the same commit.

---

## 1. What this project is

A native, single-executable video live-wallpaper engine for Windows 11. Plays the user's
own video files behind the desktop icons using Media Foundation + D3D11. No WebView2, no
Electron, no bundled mpv/ffmpeg, no third-party GPL/LGPL code in the shipped binary
(Cascade itself is GPL-3.0-only; see `LICENSE`). The default backend is Media Foundation
only: unusual codecs are the user's trade-off — convert them externally, install an OS
codec extension, or (later, if re-scoped) install an optional backend. Nothing is
downloaded or installed by Cascade in v0.1.0.

Current state: **M0–M5, M5b and M5-P4 S1–S4 are implemented; S4 (Monitors) is
pending the user's target-machine verification. M6 (hardening/release) and M7
(compatibility verification) remain.**

The app has three roles in one binary:

```
cascade.exe               daemon + tray (runs from logon to logoff)
cascade.exe --ui          GUI client (only while the window is open)
cascade.exe <command>     CLI client (one-shot)
```

Two standing rules:

- **CLI first.** Every user-facing action must be reachable from the CLI;
  the tray and the GUI are thin layers over the same daemon paths. A
  feature that exists only in the GUI does not exist.
- **The product targets the general Windows 11 population.** This dev
  machine (build 26200, 1366×768, Intel UHD) is only the development and
  test object — never design, tune, or patch for it specifically: no
  hardcoded paths, monitors, resolutions, DPI, or hardware assumptions.
  Behaviour must hold on a typical Windows 11 machine (per-monitor DPI,
  100–200 % scaling, multi-monitor, varied GPUs), and every undocumented
  Windows behaviour gets its recorded build number + fallback (§9).
- **Assume hardware variety this machine cannot show.** The installed
  base includes Intel **and** AMD CPUs; integrated GPUs (Intel UHD /
  Iris Xe, AMD Radeon iGPU) and dedicated GPUs (NVIDIA GTX/RTX, AMD
  Radeon RX, plus laptop iGPU+dGPU switchable setups); SATA SSD, NVMe
  and spinning HDDs; 100–200 % DPI scaling; mixed refresh-rate multi-
  monitor layouts; and machines with no audio device at all. Every
  design and fix must reason about those variants explicitly —
  capability probes, documented fallbacks, honest capability reporting
  — never assume this box's Intel UHD path. Full compatibility
  verification is impossible here: it is **the final milestone**, run
  after everything else is done — possibly only after the first beta
  release ships (see `docs/architecture.md` §12). What was *not* tested
  must be stated, not hidden (§9, README limitations).

Keep answers and reasoning in the language the user writes in (Indonesian is fine), but
**all code, comments, commit messages, docs, identifiers, and CLI output are English.**

---

## 2. Scope discipline

In scope for v1: video playback, desktop host, library (paths only), small GUI, tray,
pause rules, autostart, reliability, the video track's own volume (0–100, default 0 —
`docs/design-volume.md`).

Do **not** build any of this unless the user explicitly re-scopes:

- HTML/WebView2/JS wallpapers, web content, shaders, particles, audio (only the
  video track's volume above is in scope; no audio features beyond it).
- Windows 10 support, cross-platform abstractions, mobile.
- Marketplace, accounts, cloud sync, telemetry, auto-update.
- Plugin systems, scripting, custom codec packs, optional user-installed backends
  (ffmpeg or other binaries), product-side backend download/auto-install. **DEFERRED**:
  out of v0.1.0, never bundled, and needing an explicit re-scope before any code.
- Playlist rotation, tags, collections, a video preview player, themes (v1.1 at the
  earliest).
- A custom file browser. Use the native `IFileOpenDialog`.

The GUI is in scope, but only the four surfaces in `docs/architecture.md` §9. If a UI
idea does not fit one of them, it is a v1.1 idea.

v0.1.0 status of the codec work (`docs/design-m5-p4-codec-conversion.md` §12.6):
built-in `convert`, copy/replace/retranscode, conversion overlays and the Compatibility
page are **WITHDRAWN** — not deferred plans. OS-extension display, optional
user-installed backends, backend download/auto-install and the Settings
codec-prerequisite block are **DEFERRED**. The Monitors surface (S4) was implemented
2026-11-22 and is pending the user's target-machine verification; it was held, not
cancelled.

---

## 3. Hard constraints (never violate)

1. **The desktop must never be left blank.** Every exit path restores/repaints. A hard
   kill must be repaired on the next start. This is the product's core promise.
2. **Cascade never owns the wallpaper setting.** Do not call `SetWallpaper` /
   `SPI_SETDESKWALLPAPER` to install itself. Only the last-resort repaint may call SPI
   with a path, under the conditions in `docs/reliability.md` §3.
3. **Never delete, move, or modify user media.** "Remove" edits the library list only.
4. **The daemon must not depend on the UI.** Separate processes; the daemon's memory and
   behaviour are unchanged when the UI is open or closed. The daemon path must not
   initialize egui/wgpu.
5. **No third-party GPL/LGPL code in the shipped binary.** Cascade itself is
   GPL-3.0-only (`LICENSE`). Dev-only tooling is acceptable behind a non-default feature,
   excluded from release artifacts. Check licences before coding; an optional external
   backend is never bundled or linked in v0.1.0.
6. **No mpv/ffmpeg binaries committed or bundled.** mpv is a local dev harness only.
7. **No injection or hooking.** No global `SetWindowsHookEx`, no remote threads, no DLL
   injection, no patching Explorer.
8. **No elevation.** Manifest stays `asInvoker`.
9. **No network calls** in the product.
10. **One distributable binary.** The release is one `cascade.exe` plus OS DLLs.
11. **No async runtime** in `cascade-core` / `cascade-win`. Threads + channels.
12. **Don't touch other apps' data.** Only `%APPDATA%\Cascade` and
    `%LOCALAPPDATA%\Cascade` are ours.
13. **Never prevent sleep, shutdown, or logoff.** Return TRUE from `WM_QUERYENDSESSION`
    unconditionally. No `SetThreadExecutionState(ES_DISPLAY_REQUIRED)`.
14. **The shipped binary stem must never be 6 or 8 characters.** On build
    26200.9711 Windows allocates ≈250 MB (stem 8) / ≈550 MB (stem 6) into *this*
    PE's address space for those filename classes — measured, deterministic,
    survives reboot; the mechanism is external and unidentified
    (`docs/spike-m0.md` §10.4). Any rename re-measures first with a
    path-verified harness. `cascade` (7, the shipped name) is a cold class
    (`docs/spike-m0.md` §10.5–§10.8).
15. **Never run a second load pipeline in a live daemon.** Every engine load
    retains ≈50 MB of private bytes until process exit — measured, mechanism
    identified as *the load itself* (`docs/design-m4.md` §8.3). Therefore any
    toggle, setting, or event that needs the engine re-created must: persist
    plain state → answer the client → exit ladder → respawn. Never an
    in-process reload/rebuild to apply a change. A same-path set is a no-op
    (no load, no restart); only a fresh process may load. Check this rule
    before designing *any* new setting that affects the engine, the host, or
    the media boundary.
16. **`on_battery=stop` is a full stop, not a coloured pause.** On a confirmed
    battery source with `on_battery=stop`, the daemon must run its exit ladder
    (desktop restored to the *real* wallpaper — never a teal host window) and
    hand over to the listener: a fresh process with **zero compute** — no
    engine, no host windows, no timers in steady state — that serves the tray
    and the pipe honestly and hands back to a full daemon when AC returns
    (`docs/design-m5b.md` §4). Never an in-process unload that keeps ≈90 MB
    retained (§3.15), never an in-process rebuild on AC return. A manual
    `cascade run` overrides the rule for that session; the override ends at
    the next confirmed AC cycle.
17. **The CLI has two routes: the pipe, or direct file access — never a
    watcher, never an auto-start.** `settings`/`library` commands talk to
    whichever of our processes answers the pipe; direct file access is allowed
    only after a positive, double-probed "no Cascade process of ours is
    running" verdict. Never add a file watcher on `config.toml`/`library.json`,
    never start the daemon from a read command, and never write a file behind
    a possibly-live daemon (`docs/design-m5b.md` §3).

A future product-side backend downloader/auto-installer remains **DEFERRED** and would
have to amend §§2, 3.8, 3.9, 3.10 and 3.12 explicitly. It is not a hidden exception to
those rules and is not part of v0.1.0 (`docs/design-m5-p4-codec-conversion.md` §12.5).

---

## 4. Architecture rules

- Dependency direction (enforced by review, later by CI):
  - `cascade-win` must not depend on `cascade-media`.
  - `cascade-media` must not depend on `cascade-win`.
  - `cascade-ui` must not depend on `cascade-win` (no desktop-hack code in the UI).
  - IPC types live in `cascade-core`.
- `HostSurface { hwnd, monitor, rect, dpi }` is the only win → media boundary.
- `RuleEngine` is pure: events in, `DesiredPlayback` out. No Win32 inside.
- One host window per monitor. Do not "simplify" to one window spanning the desktop.
- COM: apartment-initialized per thread, RAII wrappers, released before thread exit.
- Every `unsafe` block needs a `// SAFETY:` comment. Prefer confining `unsafe` to
  `cascade-win` and `cascade-media`.
- No `unwrap()`/`expect()` in library code. `anyhow` only in the binary; libraries use
  `thiserror`.
- Prefer the `windows` crate over hand-written `extern "system"`. Link the Microsoft docs
  page in a comment when a signature must be hand-written.
- The render loop never waits on the pipe. The pipe thread never touches HWNDs.

---

## 5. Environment

- Development shell: **WSL (Ubuntu 22.04)**. The repository lives at
  `\\wsl.localhost\Ubuntu-22.04\home\rannd1nt\Projects\Cascade`.
- Rust: stable toolchain in WSL. **`~/.cargo/bin` is not on `PATH` in non-interactive
  shells** — use `export PATH="$HOME/.cargo/bin:$PATH"` or the full path.
- Windows has **no** Rust, no VS Build Tools, no Windows SDK, and only ~14 GB free on C:.
  Do not propose installing the Windows toolchain; cross-compile instead.
- Target: `x86_64-pc-windows-msvc` via `cargo-xwin` (same target CI uses).
- Test machine: Windows 11 25H2 build 26200, Intel UHD Graphics, 1366×768 @ 60 Hz.
  Performance claims are only valid on this machine (or a stated equivalent).

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo install cargo-xwin --locked      # once
rustup target add x86_64-pc-windows-msvc
```

If `cargo-xwin` hits a dependency that needs `lib.exe`, install `clang lld` via apt
first. Only if that fails, discuss the Windows toolchain with the user — do not install
it silently.

---

## 6. Commands

```bash
cargo build --workspace
cargo build --release --target x86_64-pc-windows-msvc
cargo xwin build --release --target x86_64-pc-windows-msvc
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Running the app (from Windows PowerShell, not WSL, for realistic window behaviour):

```powershell
.\cascade.exe doctor
.\cascade.exe run
.\cascade.exe status
.\cascade.exe restore
```

Dev harness (manual, never in tests/CI):

```powershell
mpv --wid=0 --loop-file=inf --no-audio --aid=no `
    --vo=gpu --gpu-context=d3d11 --hwdec=d3d11va --panscan=1.0 `
    --no-config --no-osc --osd-level=0 --profile=fast `
    --demuxer-max-bytes=8MiB --demuxer-readahead-secs=1 `
    "C:\Users\Zahraan Dzakii\Videos\Furina Midnight Maedow.mp4"
```

**Warning:** this leaves the desktop blank when killed with Ctrl+C. Restore afterwards
with `docs/spike-m0.md` §4.1. Never launch it unattended.

---

## 7. Testing rules

- Unit-test everything pure: config, library model, fit/cover math, `RuleEngine`,
  monitor rect math, IPC encode/decode, snapshot comparison.
- Integration tests that touch `WorkerW`, the wallpaper, or a real monitor are
  `#[ignore]` by default and must snapshot + restore around themselves even on panic.
- Tests must never leave persistent user state modified: no autostart left on, no marker
  left behind, no wallpaper changed.
- **The reliability matrix is part of the definition of done.** Any change to the host,
  lifecycle, IPC, or autostart requires re-running the scenarios in
  `docs/reliability.md` §12 on the target machine.
- Do not sleep-and-hope. Poll conditions with a timeout.
- CI cannot validate the desktop host. Say so instead of pretending it can.

---

## 8. Performance and quality work

- Numbers must be **measured**, never estimated, with date, machine, build, and command.
- Use per-engine GPU counters, not Task Manager's headline GPU percentage.
- Report RAM as working set **and** private bytes.
- Append measurements to `docs/spike-*.md` / `docs/benchmark-*.md`. Never overwrite old
  numbers. Corrections get their own section that says it is a correction.
- **Quality regressions are bugs.** The list of what counts is in `docs/quality.md` §8.
  A quality change needs before/after evidence, not "it looked fine".
- Never silently change quality, FPS, or profile based on power state. Profiles are
  explicit and visible.
- Required isolation check: daemon RSS with the UI closed == with the UI open.

---

## 9. Documentation rules

- `README.md`: what/why/how-to-run, honest limitations, no marketing.
- `docs/architecture.md`: system design source of truth.
- `docs/reliability.md`: lifecycle contract and scenarios. If a scenario changes, this
  file changes in the same commit.
- `docs/quality.md`: pipeline and profiles.
- `docs/ipc.md`: protocol. Any command change updates this file and bumps the version if
  the change is breaking.
- `docs/spike-*.md`: measurements, append-only.
- Record the Windows build number for every undocumented behaviour observed, and what the
  fallback is.
- Prefer tables, ASCII diagrams, and explicit numbers over prose.

---

## 10. Change workflow

1. **State the milestone and DoD** before writing code. If it does not map to a
   milestone, say so and confirm scope.
2. **Design note first** if the change touches: host strategy, wallpaper lifecycle, media
   boundary, IPC, or the library model.
3. **Smallest vertical slice.** Host + solid colour before video; one monitor before
   many; pause before library.
4. Build, lint, test.
5. Manual verification on the target machine for anything visual, Win32, or lifecycle
   related.
6. Update docs.
7. Commit in logical units.
8. **Stop at gates.** M1-A…F and the M2 three-day gate are real stops. Report the result
   and ask for a decision instead of continuing silently.

If blocked for more than a day on undocumented Win32 behaviour: write down what was tried
in `docs/notes-*.md`, implement the documented fallback, and move on. Do not burn a week
inside a COM rabbit hole.

---

## 11. Commits

Conventional Commits, scoped:

```
feat(win): attach host window to Progman's WorkerW
fix(win): repaint desktop after Ctrl+C exit
feat(win): retry shell discovery before attaching
feat(media): probe hardware H.264 decoder before load
feat(ipc): add library commands
feat(ui): library grid with lazy thumbnails
docs(quality): record 80-90% cost of quality mode
chore(repo): add cargo workspace skeleton
```

Rules:

- One logical change per commit. No formatting noise mixed with behavior.
- `main` stays buildable.
- No binaries, no `target/`, no `mpv.exe`, no test videos, no large screenshots, no
  secrets.
- Do not rewrite published history.

---

## 12. Definition of Done

- [ ] `cargo build --workspace` and the `x86_64-pc-windows-msvc` release build succeed.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- [ ] `cargo fmt --all -- --check` is clean.
- [ ] New logic has unit tests; Win32 paths have an `#[ignore]`d test or a written manual
      procedure.
- [ ] Manual verification on the target machine when visual/Win32/lifecycle related.
- [ ] Reliability scenarios re-run if the change touches host, lifecycle, IPC, or
      autostart (`docs/reliability.md` §12).
- [ ] Docs updated; measurements recorded for perf/quality work.
- [ ] No new dependency without a license check and a stated reason.
- [ ] No violation of §3.
- [ ] Daemon RSS unchanged when the UI is closed (isolation).

---

## 13. Prohibited shortcuts

| Shortcut | Why not |
|---|---|
| "Just use mpv" in release | GPL, size, second process, defeats the project |
| Skip the repaint path to move faster | It already destroyed a wallpaper in M0 |
| Install the wallpaper setting to "make it simpler" | We would own user config and could corrupt it |
| Run the UI inside the daemon | A UI crash kills the wallpaper |
| Delete missing files from disk automatically | Never touch user media |
| Add a file watcher or relocate flow | Explicitly out of scope; prune instead |
| Claim performance without measuring | The budget table becomes fiction |
| Silently drop quality/FPS on battery | Users notice and lose trust |
| Add `unsafe` to avoid a type wrapper | Unsafe must be justified |
| Poll at 60 Hz for state | Use events + a 1–2 s watchdog |
| Fixed `sleep(3)` before attaching | Use shell discovery with backoff |
| One window for all monitors | Breaks DPI and per-monitor pause |
| `process::exit()` in an exit path | Skips the repaint ladder |
| Catch-all `unwrap()` | Panics in a background daemon are user-visible |
| Copy an undocumented Win32 hack with no fallback | Windows changes; ship two paths |
| Apply an engine-affecting setting by reloading in-process | ≈50 MB retained per load (§3.15, `design-m4.md` §8.3) — persist, answer, exit ladder, respawn |
| Hide a limitation in the README | Users find it, then file a bug |
| Reformat unrelated files in a feature commit | Makes review and bisect useless |

---

## 14. Open decisions

Tracked in `docs/architecture.md` §14. Do not silently decide them in code; if an
implementation forces one, record the choice in the architecture doc and mention it.
