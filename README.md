# Cascade

A native, lightweight video live-wallpaper engine for Windows 11.

Cascade plays your own video files **behind the desktop icons**, comes back by itself
after a reboot, and never touches your files.

No WebView2. No Electron. No bundled mpv/ffmpeg. One small executable.

> **Status (2026-11-23):** M0–M5, M5b and the M5-P4 surfaces S1–S4 are
> implemented. S4 (Monitors) is pending target-machine verification. The daemon
> plays H.264 behind the icons, is fully controllable over a named pipe, and
> every exit path restores the desktop. Remaining milestones are M6 (hardening
> and release) and M7 (compatibility verification). Internal design notes are
> local-only and are not distributed with this repository.

---

## What it is

- **Video only.** MP4/MKV/MOV with H.264 8-bit 4:2:0 ≤ 1080p60, decoded by Windows Media Foundation. Other codecs (HEVC, VP9, AV1, HDR, 10-bit) are rejected with a clear message — no silent fallback.
- **Windows 11 only.** One platform, one desktop-host path, no legacy branches.
- **Vendor-agnostic.** Generic D3D11 and Media Foundation APIs only — no vendor-specific decode path. Windows selects the adapter; behaviour across GPU vendors is not yet verified.
- **Any monitor size, any video resolution, any video FPS.** Swapchain is created per-monitor; `ID3D11VideoProcessor` handles scaling. Higher resolution/FPS costs more compute — expected, not a regression.
- **Runs behind the icons.** A child window of the desktop's wallpaper surface, not an overlay, not a second desktop, not a hook.
- **A tiny daemon plus an optional UI.** The wallpaper engine keeps running when the app window is closed.
- **Your library, not your files.** Adding a video stores its path. Nothing is copied, moved, or deleted.
- **Comes back after reboot.** Autostart that waits for the shell, a persisted selection, and a watchdog that repairs the desktop after a crash or an Explorer restart.
- **GPL-3.0-only.** No third-party GPL/LGPL code in the shipped binary.

## What it is not

| Not doing | Why |
|---|---|
| HTML / WebView2 / JavaScript wallpapers | A browser runtime is the thing we are avoiding |
| Bundling mpv or ffmpeg | Third-party licence obligations, size, and a second process to babysit |
| Built-in transcode / `convert` | Withdrawn: Cascade plays the file as-is; convert it externally if you need lighter playback |
| Automatic backend download / install | Deferred and out of v0.1.0: no product network call, no auto-installed ffmpeg |
| Audio | A wallpaper should be silent |
| Windows 10 | A second host code path for no benefit |
| Marketplace, accounts, cloud, telemetry | Local files only |
| Hooking or injecting into Explorer | We only create our own window |

---

## Usage

Everything runs through the daemon; the CLI is the primary interface.

```text
cascade run [--video <path>] [--detached]  # start the engine; --detached returns at once
cascade stop                               # quit through the repaint ladder
cascade status [--json]                    # running? playing what?
cascade pause | resume
cascade set <n> | <file> | --id <id>       # play a library row (n = the number from `library ls`)
cascade library ls | add <path> | rm <n> | prune
cascade settings get | set <key> <value>
cascade autostart on | off | status
cascade ui                                 # open the GUI: Library / Settings (alias --ui)
cascade doctor                             # no daemon needed
```

`cascade run --detached` returns as soon as the engine answers its pipe
(`detached: engine running as pid …`) — close the terminal right away, the
wallpaper keeps playing. The numbers printed by `library ls` are CLI-only
ordinals: `set <n>` and `rm <n>` resolve them against a fresh list at
execution time and send stable ids, so a renumbered list cannot corrupt
anything (`docs/design-m5.md` §4).

With `tray = true` (the default) the daemon also shows a notification-area
icon with a small menu — **Open Cascade / Pause / Quit**. Every entry is a
thin wrapper over the commands above (`ui`, `pause`/`resume`, `stop`);
`settings set tray false` hides the icon while the daemon keeps running.

`cascade install` also writes two per-user shortcuts — **Start Menu →
Programs\Cascade.lnk** and **Desktop\Cascade.lnk** — so Cascade is found by
Start search and can be opened from the Desktop. Both point at the installed
`cascade.exe ui` (the same GUI path as `cascade ui`). `cascade uninstall`
removes exactly those two files and nothing else.

`cascade install` also registers Cascade in **Settings → Apps →
Installed apps** (a per-user entry, no admin needed), showing the
version, publisher and installed size. The tile's **More options →
Uninstall** runs the same uninstall as the CLI, without opening a
terminal. Windows 11 may still show a **Modify** entry in that menu;
the supported removal path is **Uninstall**.

---

## Measured baseline (M0, mpv harness)

Target machine: Windows 11 25H2 build 26200.9711, Intel UHD Graphics (driver
30.0.101.1960), 1366×768 @ 60 Hz, one monitor.

| Configuration | CPU | RAM (working set) | GPU 3D | GPU VideoDecode |
|---|---|---|---|---|
| `--vo=gpu --profile=fast` (**Balanced** reference) | 0.6–2.2% | 106–115 MB | 18–27% | 19–22% |
| `--vo=gpu-next` + Lanczos / linear light | — | — | **80–90%** | — |
| paused / exited | ~0% | — | 0–7% | 0% |

These are mpv numbers, not Cascade's. Two conclusions shaped the design:

1. Decode is essentially free; **composition is the cost**, and pausing is the only real
   battery lever.
2. High-quality scaling is not worth it on this GPU. **Balanced is the default**;
   `Quality` is opt-in with a warning. See the internal quality notes.

## The safety promise

A wallpaper app must never eat the wallpaper. Cascade:

- **does not install itself as your wallpaper** — it draws over the desktop, leaving your
  wallpaper configuration untouched;
- restores the desktop on every exit path (tray, Ctrl+C, closing the terminal that
  launched it, logoff, panic);
- repairs a blank desktop on the next start after a crash or a Task Manager kill;
- reattaches within ~5 seconds if Explorer restarts;
- pauses when the desktop is not visible (fullscreen, lock, display off).

Full contract and test procedures live in the internal reliability notes.

## Known limitations

- **Antivirus false positives.** An unsigned `cascade.exe` that writes a Run-key
  autostart entry, starts with no console window of its own (GUI subsystem), and stays
  resident matches the behavioural
  signature of persistence malware — Microsoft Defender flagged it as
  `Behavior:Win32/Persistence.A!ml` during development. Two things reduce this: install
  outside temp/download folders, and **code signing** (not yet done; it costs money and
  needs a certificate). Until a release is signed, some AV products may prompt on first
  run. This is stated here rather than hidden — see `docs/reliability.md` §11.
- On Windows 11 24H2+ the desktop is a "raised desktop" where **GDI painting never
  appears**; Cascade must present through DXGI. The GDI path only works on the classic
  layout and is kept as a fallback. If DXGI device creation fails on the raised desktop,
  the host is invisible (reported loudly at attach, never silently).
- Scaling a 1080p video to a 768p panel is inherent; we use a good filter, not the most
  expensive one. "No re-encoding, no bit-depth loss, correct colour" is the promise —
  not "pixel-identical to 1080p".
- HDR is detected and reported, not tone-mapped (v1).
- **Hardware coverage is generic, not exhaustively verified.** D3D11 requests feature level 11 and lets Windows pick the adapter — no adapter pinning, no lower-feature-level or software fallback. There is no `WM_DPICHANGED` handler, and `hwdec` is omitted unless a hardware MFT explicitly registers H.264. Intel/AMD/NVIDIA iGPU and dGPU, mixed-DPI, mixed-refresh and no-audio machines are untested outside the development box (`docs/design-m7-p0-honesty.md` §8).
- HEVC / VP9 / AV1 / HDR / 10-bit are rejected in v1 with a clear message — only H.264 8-bit 4:2:0 ≤ 1080p60 is supported. Cascade does not convert them; convert externally yourself. OS-extension and optional-backend handling is deferred and not in v0.1.0.
- A clip looping has a visible seam; crossfade is not in v1.
- **Frame-server colour fix costs CPU and RAM.** The default engine mode presents
  through a CPU staging bridge between two D3D11 devices (needed so MF honours the
  video's colour flags). Measured on the target machine 2026-11-10: CPU 33.5% avg
  (vs 9–10% for the rendering route) and ~225 MB private bytes
  (`docs/rnd-media-engine-bottleneck.md` §6). You choose the trade with
  `cascade settings set engine_mode accurate|efficient` (default `accurate`):
  `efficient` runs the cheaper rendering route but shows the wrong colour on BT.601
  content — the pre-fix behaviour, stated here rather than hidden. Changing the mode
  restarts the daemon while a video plays. Optimisation of the accurate route is
  planned; independently, if the frame-server ever fails ≥3 times in a row it falls
  back to rendering mode, which keeps the wallpaper but re-introduces the colour
  defect (`docs/design-engine-mode.md`).
- **Audio is off by default.** The video's own audio track plays only when you set a
  level: `cascade settings set volume 50` (0–100, `0` = muted — the default; the
  change applies immediately, no restart). The wallpaper never touches the system
  master volume, and there is no audio beyond the video track itself
  (`docs/design-volume.md`).
- An external drive that is unplugged counts as "missing"; the entry is pruned from the
  library (no relocate flow, by design).
- The desktop-host API is undocumented and has changed before. Two strategies plus a
  watchdog exist for that reason.
- **In Windows PowerShell 5.1, interactive output can be overwritten by the next
  prompt.** `cascade.exe` must be a GUI-subsystem binary — a console build flashes a
  Terminal window and steals focus on every autostart (measured, `docs/spike-m0.md`
  "M1-F corrections") — and PowerShell 5.1 launches GUI-subsystem apps *without
  waiting*: the `PS C:\...>` prompt returns first, and PSReadLine repaints its prompt
  lines over `cascade`'s output on the next Enter or Ctrl+C. This is a PowerShell
  behaviour (PowerShell/PowerShell#25875, PowerShell/PSReadLine#366), not something
  the app can fix. Workarounds: `cascade status | Out-Host` (a pipeline forces
  PowerShell to wait) or `cmd /c cascade status`. After `cascade install` a generated
  `cascade.cmd` shim makes plain `cascade ...` calls synchronous; the measurements are
  in `docs/design-m4.md` §6.4.
- **Redirected output: cmd and pipelines work; PowerShell `> file` does not.**
  Measured on Windows 11 build 26200.9711: a successful `AttachConsole`
  *replaces* already-valid standard handles, so before the 2026-11-12 stdio
  fix any redirect through `cmd` came out empty (`cascade --help > out.txt`
  → 0 bytes; `docs/design-m4.md` §6.4). After the fix (`console.rs` checks
  the handles **before** attaching) cmd file and pipe redirects deliver the
  text — 0 → 22 bytes, `docs/design-m5.md` §14. Still broken: Windows
  PowerShell 5.1's `> file` to a GUI-subsystem child loses the text and the
  child panics with `os error 232` (PS closes the pipe early — a PowerShell
  behaviour, not ours). To capture text from PowerShell use a pipeline
  (`cascade status | Out-String`), `cmd /c cascade … > out.txt`, or
  `Start-Process -RedirectStandardOutput …`. Exit codes always arrive either
  way (e.g. the HEVC refusal's `2`), and printing on a real interactive
  console is unaffected.

## Build (from WSL, no Windows toolchain required)

```bash
cargo install cargo-xwin --locked
rustup target add x86_64-pc-windows-msvc
cargo xwin build --release --target x86_64-pc-windows-msvc
```

Run the resulting `.exe` from Windows. Release artifacts are built by GitHub Actions on
`v*` tags and published as prereleases (`v0.1.0-alpha.1`, `-beta.1`, …).

## Documentation

`AGENTS.md` in this repository is the development workflow and hard rules.

Internal design, measurement and spike notes are **local-only companion files**;
they are not part of this repository and are not distributed with a release.
They cover the system shape (`architecture.md`), lifecycle (`reliability.md`),
pipeline and profiles (`quality.md`), wire protocol (`ipc.md`) and measured
findings (`spike-m0.md`). `AGENTS.md` records the read order for contributors
who have them.

## Uninstall

```
cascade uninstall [--purge]
```

One command reverses every install step: it stops the engine, turns autostart
off, deletes the Start Menu and Desktop `Cascade.lnk` shortcuts (exactly those
two names — any other `.lnk`, even one named similarly, is left alone), removes
the `bin` shim directory from your user PATH, deletes the shim and
the installed copy (if the copy is the running image, a small helper in `%TEMP%`
finishes the delete right after it exits), and removes Cascade's own files from
`%LOCALAPPDATA%\Cascade`. `library.json` and `state.json` are kept unless you
pass `--purge`. Other files in that folder (your `measurements\` logs, for
example) are left alone.

Your videos are untouched — they were never copied or modified.

## License

GPL-3.0-only. See [`LICENSE`](LICENSE). Cascade itself is GPL-3.0-only; no third-party
GPL/LGPL dependency is compiled into the shipped binary. Third-party dependencies keep
their own licences and are checked before release.
