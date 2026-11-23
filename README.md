<div align="center">
  <img src="img/cascade.png" alt="Cascade Icon and Brand" width="20%">

  <h1>Cascade</h1>
  <p><strong>A native, lightweight video live-wallpaper engine for Windows 11.</strong></p>
</div>


Cascade plays your own video files **behind the desktop icons**, comes back by itself
after a reboot, and never touches your files.

Your wallpaper setting stays yours: Cascade draws over the desktop instead of replacing
it, and restores the desktop when it exits or crashes.

## What you need

- Windows 11.
- A video Cascade can play: H.264, 8-bit 4:2:0, up to 1080p60 (MP4/MKV/MOV).
  Other formats are refused with a clear message; Cascade does not convert them.
- A graphics card that supports D3D11. Windows picks the adapter; Cascade does not
  install codecs, backends, or any other software.

## Install and run

Download `cascade.exe`, then from PowerShell:

```powershell
.\cascade.exe install
.\cascade.exe run
```

`install` adds a Start Menu shortcut, a Desktop shortcut, and per-user autostart — no
admin rights needed. `uninstall` reverses those steps.

If you just want to try it without installing:

```powershell
.\cascade.exe run --video "C:\Videos\clip.mp4"
```

The wallpaper keeps playing when you close the terminal.

## Commands

Everything is controllable from the command line; the tray menu and the GUI use the
same commands.

```text
cascade run [--video <path>] [--detached]  # start playback; --detached returns at once
cascade stop                               # quit and restore the desktop
cascade status [--json]                    # is it running, and what is playing?
cascade pause | resume
cascade set <n> | <file> | --id <id>       # play a library entry
cascade library ls | add <path> | rm <n> | prune
cascade settings get | set <key> <value>
cascade autostart on | off | status
cascade ui                                 # open the Library / Settings / Monitors window
cascade doctor                             # check the machine; no daemon needed
cascade restore                            # repaint the desktop if something went blank
```

`cascade library add <path>` only stores the path. Your video is never copied, moved,
renamed, or deleted. `prune` removes library entries whose files are gone.

With `tray = true` (the default) the daemon shows a small notification-area icon with
**Open Cascade / Pause / Quit**. `settings set tray false` hides it.

## The safety promise

- **Your wallpaper setting is never changed** by Cascade.
- The desktop is restored on every exit path: tray, Ctrl+C, closing the terminal,
  logoff, and crashes.
- A blank desktop left by a hard kill is repaired on the next start.
- Playback pauses when the desktop is not visible (fullscreen, lock, display off).
- Cascade never prevents sleep, shutdown, or logoff.

## Settings worth knowing

```powershell
cascade settings set volume 50          # video's own audio, 0–100 (default 0 = muted)
cascade settings set engine_mode efficient   # cheaper, but wrong colour on some videos
```

The default `accurate` mode keeps the video's colour flags but costs noticeably more
CPU and memory. `efficient` is the cheaper route with a known colour defect on BT.601
content. The wallpaper never touches the system master volume.

## Known limitations

- **Only H.264 8-bit 4:2:0 up to 1080p60 is supported.** HEVC, VP9, AV1, 10-bit and HDR
  are refused with a clear message. Convert such files yourself before adding them.
- **No built-in transcoding or optional backends.** Cascade plays the file as-is and
  makes no network calls.
- **Hardware coverage is generic, not exhaustively verified.** Cascade uses D3D11
  feature level 11 and lets Windows choose the adapter. Intel, AMD and NVIDIA iGPUs and
  dGPUs, mixed-DPI and mixed-refresh monitor setups, and machines with no audio device
  have not been verified outside the development machine.
- **Unsigned builds may worry antivirus software.** A program that starts at logon,
  has no console window, and stays resident resembles persistence malware;
  Microsoft Defender flagged the development build as
  `Behavior:Win32/Persistence.A!ml`. Installing outside Downloads/Temp helps. Code
  signing is not done yet, so some AV products may prompt on first run.
- **Scaling is not pixel-identical to the source.** A 1080p video on a 768p screen is
  resampled; Cascade uses a quality filter, not the most expensive one. Looping has a
  visible seam.
- **PowerShell 5.1 can paint over interactive output** with the next prompt. Use
  `cascade status | Out-Host` or `cmd /c cascade status` if text disappears. Plain
  `cascade status > file.txt` in PowerShell 5.1 also loses the text; use a pipeline,
  `cmd /c cascade ... > file.txt`, or `Start-Process -RedirectStandardOutput`.
- An external drive that is unplugged makes its library entry "missing"; the entry is
  pruned. The video files themselves are untouched.

## Uninstall

```powershell
cascade uninstall [--purge]
```

This stops the engine, turns autostart off, removes the two shortcuts and the PATH shim,
and deletes Cascade's installed copy and its own files under `%LOCALAPPDATA%\Cascade`.
`library.json` and `state.json` are kept unless you pass `--purge`. Your videos are
never touched.

## License

GPL-3.0-only. See [`LICENSE`](LICENSE). No third-party GPL/LGPL code is compiled into
the shipped binary; other dependencies keep their own licences and are checked before
release.
