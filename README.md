# Cascade

A native, lightweight video live-wallpaper engine for Windows 11.

Cascade plays your own video files **behind the desktop icons**, comes back by itself
after a reboot, and never touches your files.

No WebView2. No Electron. No bundled mpv/ffmpeg. One small executable.

> **Status (2026-11-12):** M0–M4 measured complete (S7 evidence:
> `docs/design-m4.md` §8.7); **M5 in progress — P1 (CLI foundation) and
> P2 (GUI MVP) shipped and verified on target**: `run --detached`,
> ordinal `library ls` / `set <n>` / `library rm <n>`, `stop`
> (`P1-VERIFY: ALL PASS`, 38 checks), and the GUI (`cascade ui`): add /
> remove / set a video through the native file dialog, Start-engine when
> the daemon is down, window geometry persistence, and **0 MB** measured
> impact on the daemon while the window is open (`docs/design-m5.md`
> §14). The daemon plays H.264 behind the icons, is fully
> controllable over a named pipe, `cascade install` puts `cascade` on
> your PATH, and every exit path restores the desktop.

---

## What it is

- **Video only.** MP4/MKV/MOV with H.264 8-bit 4:2:0 ≤ 1080p60, decoded by Windows Media Foundation. Other codecs (HEVC, VP9, AV1, HDR, 10-bit) are rejected with a clear message — no silent fallback.
- **Windows 11 only.** One platform, one desktop-host path, no legacy branches.
- **Cross-GPU.** Developed on Intel UHD iGPU; D3D11 hardware selection adapts to dedicated GPU (NVIDIA/AMD) when present.
- **Any monitor size, any video resolution, any video FPS.** Swapchain is created per-monitor; `ID3D11VideoProcessor` handles scaling. Higher resolution/FPS costs more compute — expected, not a regression.
- **Runs behind the icons.** A child window of the desktop's wallpaper surface, not an overlay, not a second desktop, not a hook.
- **A tiny daemon plus an optional UI.** The wallpaper engine keeps running when the app window is closed.
- **Your library, not your files.** Adding a video stores its path. Nothing is copied, moved, or deleted.
- **Comes back after reboot.** Autostart that waits for the shell, a persisted selection, and a watchdog that repairs the desktop after a crash or an Explorer restart.
- **MIT OR Apache-2.0.** No GPL anywhere in the shipped binary.

## What it is not

| Not doing | Why |
|---|---|
| HTML / WebView2 / JavaScript wallpapers | A browser runtime is the thing we are avoiding |
| Bundling mpv or ffmpeg | GPL, size, and a second process to babysit |
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
cascade ui                                 # open the library window (alias --ui)
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
   `Quality` is opt-in with a warning. See [`docs/quality.md`](docs/quality.md).

## The safety promise

A wallpaper app must never eat the wallpaper. Cascade:

- **does not install itself as your wallpaper** — it draws over the desktop, leaving your
  wallpaper configuration untouched;
- restores the desktop on every exit path (tray, Ctrl+C, closing the terminal that
  launched it, logoff, panic);
- repairs a blank desktop on the next start after a crash or a Task Manager kill;
- reattaches within ~5 seconds if Explorer restarts;
- pauses when the desktop is not visible (fullscreen, lock, display off).

Full contract and test procedures: [`docs/reliability.md`](docs/reliability.md).

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
- HEVC / VP9 / AV1 / HDR / 10-bit are rejected in v1 with a clear message — only H.264 8-bit 4:2:0 ≤ 1080p60 is supported.
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

| File | Contents |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | system shape, crates, contracts, milestones |
| [`docs/reliability.md`](docs/reliability.md) | lifecycle, restore, autostart, watchdog, M1 gates |
| [`docs/quality.md`](docs/quality.md) | pipeline, profiles, what "no loss" means |
| [`docs/ipc.md`](docs/ipc.md) | daemon/client protocol |
| [`docs/spike-m0.md`](docs/spike-m0.md) | environment, commands, measurements, findings |
| [`AGENTS.md`](AGENTS.md) | development workflow and hard rules |

## Uninstall

```
cascade uninstall [--purge]
```

One command reverses every install step: it stops the engine, turns autostart
off, removes the `bin` shim directory from your user PATH, deletes the shim and
the installed copy (if the copy is the running image, a small helper in `%TEMP%`
finishes the delete right after it exits), and removes Cascade's own files from
`%LOCALAPPDATA%\Cascade`. `library.json` and `state.json` are kept unless you
pass `--purge`. Other files in that folder (your `measurements\` logs, for
example) are left alone.

Your videos are untouched — they were never copied or modified.

## License

MIT OR Apache-2.0, at your option. `LICENSE-MIT` and `LICENSE-APACHE` are added at the
first release.
