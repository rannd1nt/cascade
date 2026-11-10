# Cascade

A native, lightweight video live-wallpaper engine for Windows 11.

Cascade plays your own video files **behind the desktop icons**, comes back by itself
after a reboot, and never touches your files.

No WebView2. No Electron. No bundled mpv/ffmpeg. One small executable.

> **Status (2026-11-09):** M0–M1 complete (all gates A–F measured, `docs/reliability.md`
> §11); M2 native player shipping — hardware decode + quality rows re-measured on the
> current build (`docs/benchmark-m2.md` §11); M3 shipped for session-lock only, the
> remaining pause collectors are designed in `docs/design-m3b.md`; **M4 (IPC + CLI +
> library + assessment + install) in progress — S0–S6 done, next S7**
> (`docs/design-m4.md` §8). The daemon plays H.264 behind the icons, is fully
> controllable over a named pipe, `cascade install` puts `cascade` on your PATH,
> and every exit path restores the desktop.

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
- **Redirected output through cmd/PowerShell comes out empty — exit codes still
  work.** When `cmd.exe` or Windows PowerShell starts `cascade ...` with its
  output redirected (`cascade --help > out.txt`, `cascade --help | findstr ...`),
  the GUI child's text lands on the parent's hidden console instead of the
  redirect and is lost (measured, Windows 11 build 26200.9711; every exit code
  arrives, e.g. the HEVC refusal's `2`). Printing on a real interactive console
  is unaffected. To capture text, start the exe directly with a redirect you
  own (`Start-Process -RedirectStandardOutput ...`), which is what the test
  harness does. Details in `docs/design-m4.md` §6.4.

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
