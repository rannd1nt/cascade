//! Media Foundation playback backend (design-m2 §2, design-p1-color §4.2).
//!
//! Default mode is the **frame-server**: an `IMFMediaEngine` created
//! *without* `MF_MEDIA_ENGINE_PLAYBACK_HWND`, bound to a dedicated
//! decode-side D3D11 device through `MF_MEDIA_ENGINE_DXGI_MANAGER`. Frames
//! leave the engine through `TransferVideoFrame` as raw NV12 on that device
//! and cross to the render device through a CPU staging bridge
//! (`design-p1-color.md` §4.5.2 — decode and 3D must not share one device,
//! `notes-p1-color-601.md` §11), then are converted + scaled by our own
//! D3D11 video processor (`E3bPipeline`) with the matrix/range from the
//! SPS/VUI parser — Media Foundation ignores H.264 VUI colour flags on
//! every layer reachable on build 26200.9711, so the YUV→RGB step must be
//! ours (`notes-p1-color-601.md`, `design-p1-color.md` §2).
//!
//! If frame-server initialisation or the frame pipeline fails repeatedly,
//! the backend retries once in **rendering** mode — the proven
//! `MF_MEDIA_ENGINE_PLAYBACK_HWND` variant-A path: video stays visible with
//! the known wrong colour on BT.601 content (documented in `README.md`,
//! reported as `engine` in `status`, `design-p1-color.md` §4.4).
//!
//! All engine interaction runs on a dedicated playback thread behind a
//! command channel (the `source_reader.rs` pattern): the main thread only
//! sends commands and reads published atomics — no COM on the main thread
//! (AGENTS §4, design-m2 §3.1). `attach_surface` binds only the swapchain;
//! the engine itself does not depend on the host HWND, so a re-attach never
//! recreates or reloads the engine (`design-p1-color.md` §4.5, U4).

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::colour::ColourTags;
use crate::source_reader::{
    create_d3d11, create_dxgi_device_manager, create_swapchain, query_frame_rate,
    sequence_header_tags, E3bPipeline,
};
use cascade_core::{
    BackendError, BackendResult, BackendStats, Fit, HostSurface, MediaBackend, PlaybackOptions,
    PresentationMode,
};
use windows::core::{implement, IUnknown, Interface, BSTR, PCWSTR};
use windows::Win32::Foundation::{HWND, RECT, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_MAP_WRITE, D3D11_TEXTURE2D_DESC, D3D11_USAGE,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGISwapChain1, DXGI_PRESENT};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MFMediaEngineClassFactory, IMFAttributes, IMFMediaEngine, IMFMediaEngineClassFactory,
    IMFMediaEngineEx, IMFMediaEngineNotify, IMFMediaEngineNotify_Impl, MFCreateAttributes,
    MFCreateSourceReaderFromURL, MFShutdown, MFStartup, MFSTARTUP_FULL, MF_MEDIA_ENGINE_CALLBACK,
    MF_MEDIA_ENGINE_DXGI_MANAGER, MF_MEDIA_ENGINE_PLAYBACK_HWND,
    MF_MEDIA_ENGINE_READY_HAVE_CURRENT_DATA, MF_MEDIA_ENGINE_STATISTIC,
    MF_MEDIA_ENGINE_STATISTIC_FRAMES_DROPPED, MF_MEDIA_ENGINE_STATISTIC_FRAMES_RENDERED,
    MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, IsWindow, PeekMessageW, PostQuitMessage, TranslateMessage, MSG, PM_REMOVE,
    WM_QUIT,
};

/// Best-effort probe line on stderr (mirrors `derr!` in `cascade-win`;
/// `cascade-media` may not depend on it — AGENTS §4). Never panics: these
/// probes run on detach/exit paths where a failed write during `CTRL_CLOSE`
/// must not skip the repaint ladder (AGENTS §3.1).
macro_rules! probe {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

/// How long the playback thread waits for the first decodable frame.
const LOAD_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval inside that wait.
const LOAD_POLL: Duration = Duration::from_millis(20);
/// Main-thread bound for a playback-thread acknowledgement (spawn ready,
/// reload, attach, detach): the engine's internal `LOAD_TIMEOUT` wait plus
/// two seconds of headroom for the thread's own setup (D3D11 device +
/// engine creation), so the readiness budget is unchanged from when the
/// engine ran on the main thread.
const ACK_TIMEOUT: Duration = Duration::from_secs(7);
/// No-new-frame / rendering-mode pump cadence (E3 probe used the same 2 ms).
const TICK_POLL: Duration = Duration::from_millis(2);
/// Detached or paused cadence: nothing to draw, stay cheap (design §4.5.1).
const IDLE_POLL: Duration = Duration::from_millis(5);
/// Refresh interval of the engine's own frame counters (rendering mode).
const STATS_POLL: Duration = Duration::from_secs(1);
/// Consecutive frame-pipeline failures before the one-way trip to rendering
/// mode (design-p1-color §4.4).
const FALLBACK_ERRORS: u32 = 3;
/// Frame rate handed to the video processor when the stream does not say.
const DEFAULT_FRAME_RATE: (u32, u32) = (60, 1);

/// Published `BackendStats::engine_mode` values (design §4.5.1 "status").
const MODE_NONE: u8 = 0;
const MODE_FRAME_SERVER: u8 = 1;
const MODE_RENDERING: u8 = 2;

/// RAII COM apartment for a Cascade thread (AGENTS §4: apartment per
/// thread, released before thread exit).
struct ComInit {
    /// `false` when COM was already initialized in a different apartment
    /// mode: COM is usable, but this thread must not uninitialize it.
    ours: bool,
}

impl ComInit {
    /// Initialize COM with the requested apartment mode. The daemon main
    /// thread runs STA (it owns windows); the playback thread runs STA too
    /// (design-m2 §3.3 — the MTA experiment failed, notes §11.1, so the
    /// freeze fix is the split decode/render device, §4.5.2).
    fn new(apartment: COINIT) -> BackendResult<Self> {
        // SAFETY: null reserved pointer is the documented value; Drop only
        // uninitializes when this call is the one that initialized COM.
        let hr = unsafe { CoInitializeEx(None, apartment) };
        if hr.is_ok() {
            // S_OK and S_FALSE both mean "initialized"; S_FALSE still needs
            // a balancing CoUninitialize.
            Ok(Self { ours: true })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Self { ours: false })
        } else {
            Err(BackendError::Mf(format!(
                "CoInitializeEx failed: 0x{:08X}",
                hr.0
            )))
        }
    }
}

impl Drop for ComInit {
    fn drop(&mut self) {
        if self.ours {
            // SAFETY: balances the successful CoInitializeEx in `new`.
            unsafe { CoUninitialize() };
        }
    }
}

/// RAII `MFStartup`/`MFShutdown` pair (design-m2 §3.2). The playback
/// thread constructs its own pair as well; MF refcounts the startup.
struct MfStartup;

impl MfStartup {
    fn new() -> BackendResult<Self> {
        // SAFETY: documented pairing; MFSTARTUP_FULL is required by the
        // media engine (design-m2 §3.2).
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .map_err(|err| BackendError::Mf(format!("MFStartup: {}", err.message())))?;
        Ok(Self)
    }
}

impl Drop for MfStartup {
    fn drop(&mut self) {
        // SAFETY: balances the successful MFStartup in `new`.
        let _ = unsafe { MFShutdown() };
    }
}

/// Required event sink (MSDN: `MF_MEDIA_ENGINE_CALLBACK` is required in
/// every mode; without it `CreateInstance` returns `MF_E_ATTRIBUTENOTFOUND`
/// — measured on build 26200.9711). Events are not consumed here: readiness
/// and errors are polled (design-m2 §3.1), so the sink only has to exist
/// and be safe to call from any thread.
#[implement(IMFMediaEngineNotify)]
struct EngineNotify;

impl IMFMediaEngineNotify_Impl for EngineNotify_Impl {
    fn EventNotify(&self, _event: u32, _param1: usize, _param2: u32) -> windows::core::Result<()> {
        Ok(())
    }
}

/// The `MediaBackend` implementation over a playback thread that owns the
/// `IMFMediaEngine` (design-p1-color §4.2). The main thread keeps only the
/// remembered source/options and the published atomics — `stats()` never
/// touches COM (design §4.5.1).
pub struct MediaEngineBackend {
    /// Remembered by `load`; consumed when the playback thread spawns or
    /// reloads.
    source: Option<PathBuf>,
    opts: PlaybackOptions,
    /// Desired play state on the main thread; mirrored into every engine
    /// the playback thread creates (and re-applied after a reattach, when
    /// a detach paused the engine).
    want_playing: bool,
    /// Command channel to the playback thread; `None` before the first
    /// `attach_surface`, which spawns it (design §4.5.1 "spawn").
    cmd_tx: Option<Sender<EngineCmd>>,
    thread: Option<JoinHandle<()>>,
    /// Published by the playback thread, read here only.
    frames: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    video_size: Arc<AtomicU64>,
    engine_mode: Arc<AtomicU8>,
    /// Last frame count behind the measured `fps` (M4 S3b): frames over the
    /// wall time between two `stats()` calls, which the render loop makes
    /// on its 1–2 s watchdog tick. `Cell` because `stats(&self)` takes
    /// `&self`; the backend lives on the daemon's main thread only
    /// (design-m2 §3.1), so no synchronisation is needed.
    fps_sample: Cell<Option<(u64, Instant)>>,
    // Declaration order = drop order: `Drop` stops the playback thread
    // first (it releases its engine before its own MFShutdown/COM), then
    // `_mf` runs MFShutdown, then `_com` runs CoUninitialize on this thread
    // (design-m2 §3.3).
    _mf: MfStartup,
    _com: ComInit,
}

impl MediaEngineBackend {
    /// Construct the COM + MF runtime on the calling (main) thread.
    /// `MFStartup` runs here and again on the playback thread
    /// (design-m2 §3.2; MF refcounts it).
    pub fn new() -> BackendResult<Self> {
        let com = ComInit::new(COINIT_APARTMENTTHREADED)?;
        let mf = MfStartup::new()?;
        Ok(Self {
            source: None,
            opts: PlaybackOptions::default(),
            want_playing: true,
            cmd_tx: None,
            thread: None,
            frames: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            video_size: Arc::new(AtomicU64::new(0)),
            engine_mode: Arc::new(AtomicU8::new(MODE_NONE)),
            fps_sample: Cell::new(None),
            _mf: mf,
            _com: com,
        })
    }

    /// Frames per second over the interval since the previous `stats()`
    /// call (M4 S3b). The arithmetic lives in [`fps_rate`]; this half only
    /// owns the sample cell (`stats(&self)` takes `&self`).
    fn measured_fps(&self, frames: u64) -> f32 {
        let now = Instant::now();
        let (rate, sample) = fps_rate(self.fps_sample.get(), frames, now);
        self.fps_sample.set(Some(sample));
        rate
    }

    /// Ask the playback thread to stop and wait for it (design §4.5.1
    /// teardown): the thread releases its engine before its own
    /// MFShutdown/CoUninitialize, so the join is the ordering barrier.
    fn stop_thread(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(EngineCmd::Stop);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Bounded wait for a playback-thread acknowledgement. The bound is
/// [`ACK_TIMEOUT`] (see there for the headroom reasoning).
fn wait_ack(rx: &Receiver<Result<(), String>>, what: &str) -> BackendResult<()> {
    match rx.recv_timeout(ACK_TIMEOUT) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(reason)) => Err(BackendError::Mf(format!("{what}: {reason}"))),
        Err(RecvTimeoutError::Timeout) => Err(BackendError::Mf(format!(
            "{what}: playback thread did not answer within {} ms",
            ACK_TIMEOUT.as_millis()
        ))),
        Err(RecvTimeoutError::Disconnected) => {
            Err(BackendError::Mf(format!("{what}: playback thread exited")))
        }
    }
}

impl Drop for MediaEngineBackend {
    fn drop(&mut self) {
        // Stop the playback thread first: it releases its engine, then its
        // own MFShutdown/CoUninitialize. `_mf` and `_com` drop afterwards
        // in declaration order (design-m2 §3.3).
        self.stop_thread();
    }
}

impl MediaBackend for MediaEngineBackend {
    fn load(&mut self, source: &Path, opts: &PlaybackOptions) -> BackendResult<()> {
        // Validate before touching playback: a missing file degrades to the
        // colour background and never blanks the desktop (AGENTS §3.1).
        std::fs::metadata(source).map_err(|err| BackendError::Open {
            path: source.display().to_string(),
            reason: err.to_string(),
        })?;
        self.source = Some(source.to_path_buf());
        self.opts = opts.clone();
        self.want_playing = true;
        // Running thread: ack'd reload (design §4.5.1). No thread yet: the
        // source is only remembered — `attach_surface` spawns and loads it
        // (today's contract).
        let (ack_tx, ack_rx) = mpsc::channel();
        let cmd = EngineCmd::Reload {
            source: source.to_path_buf(),
            opts: opts.clone(),
            ack: ack_tx,
        };
        let sent = self.cmd_tx.as_ref().map(|tx| tx.send(cmd));
        match sent {
            Some(Ok(())) => wait_ack(&ack_rx, "reload"),
            Some(Err(_)) => {
                // The thread died (panic or early error): the remembered
                // source respawns it on the next `attach_surface`.
                self.stop_thread();
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn play(&mut self) -> BackendResult<()> {
        self.want_playing = true;
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(EngineCmd::Play);
        }
        Ok(())
    }

    fn pause(&mut self) -> BackendResult<()> {
        self.want_playing = false;
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(EngineCmd::Pause);
        }
        Ok(())
    }

    fn set_looping(&mut self, on: bool) -> BackendResult<()> {
        self.opts.looping = on;
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(EngineCmd::SetLooping(on));
        }
        Ok(())
    }

    fn stats(&self) -> BackendStats {
        // Atomics only — no COM on the main thread (design §4.5.1).
        // Hardware decode stays `None`: the engine cannot observe it and
        // the probe is slice 4, so `status` never prints a guess.
        let frames = self.frames.load(Ordering::Acquire);
        let packed = self.video_size.load(Ordering::Acquire);
        BackendStats {
            hardware_decode: None,
            decoded_frames: frames,
            dropped_frames: self.dropped.load(Ordering::Acquire),
            video_size: ((packed >> 32) as u32, packed as u32),
            fps: self.measured_fps(frames),
            engine_mode: match self.engine_mode.load(Ordering::Acquire) {
                MODE_FRAME_SERVER => Some(String::from("frame-server")),
                MODE_RENDERING => Some(String::from("rendering")),
                _ => None,
            },
        }
    }

    fn attach_surface(&mut self, surface: HostSurface) -> BackendResult<()> {
        let (width, height) = (surface.rect.w as u32, surface.rect.h as u32);
        // Running thread: rebind the surface only. A frame-server engine
        // survives the reattach untouched — this is the U4 bet
        // (design §4.5); a rendering engine is rebuilt on the new HWND.
        if self.thread.is_some() {
            let (ack_tx, ack_rx) = mpsc::channel();
            let cmd = EngineCmd::Attach {
                hwnd: surface.hwnd,
                width,
                height,
                ack: ack_tx,
            };
            let sent = self.cmd_tx.as_ref().map(|tx| tx.send(cmd));
            match sent {
                Some(Ok(())) => return wait_ack(&ack_rx, "attach"),
                // Dead thread: fall through and spawn a fresh one.
                Some(Err(_)) | None => self.stop_thread(),
            }
        }
        // First attach (or respawn): the source is required — the
        // `attach before load` error is kept (design §4.5.1).
        let source = self
            .source
            .clone()
            .ok_or_else(|| BackendError::Mf("attach_surface called before load".into()))?;
        let (ack_tx, ack_rx) = mpsc::channel();
        let config = ServerConfig {
            source,
            opts: self.opts.clone(),
            want_playing: self.want_playing,
            hwnd: surface.hwnd,
            width,
            height,
            ready: ack_tx,
            frames: Arc::clone(&self.frames),
            dropped: Arc::clone(&self.dropped),
            video_size: Arc::clone(&self.video_size),
            engine_mode: Arc::clone(&self.engine_mode),
        };
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let thread = thread::spawn(move || playback_thread(config, cmd_rx));
        self.cmd_tx = Some(cmd_tx);
        self.thread = Some(thread);
        // Spawn handshake: `Ok` once an engine is ready to draw (either
        // mode), `Err` when neither could start — attach keeps today's
        // synchronous error contract (design §4.5.1 "spawn").
        wait_ack(&ack_rx, "attach")
    }

    fn detach_surface(&mut self) -> BackendResult<()> {
        let (ack_tx, ack_rx) = mpsc::channel();
        let sent = self
            .cmd_tx
            .as_ref()
            .map(|tx| tx.send(EngineCmd::Detach { ack: ack_tx }));
        match sent {
            // Acked after the swapchain is released (and the frame-server
            // engine paused), so the caller may destroy the HWND right
            // after — the watchdog/display paths do (design §4.5.1).
            Some(Ok(())) => wait_ack(&ack_rx, "detach"),
            Some(Err(_)) => {
                self.stop_thread();
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn shutdown(self: Box<Self>) {
        // `Drop` stops the playback thread (engine release), then
        // MFShutdown, then CoUninitialize — in that order (design-m2 §3.3).
        drop(self);
    }
}

/// Command sent from the daemon main thread to the playback thread.
/// `Play`/`Pause`/`SetLooping` are fire-and-forget (the `source_reader`
/// pattern); the ack'd variants exist so the caller may destroy or rebuild
/// the HWND right after the answer (design §4.5.1).
enum EngineCmd {
    Attach {
        hwnd: isize,
        width: u32,
        height: u32,
        ack: Sender<Result<(), String>>,
    },
    Detach {
        ack: Sender<Result<(), String>>,
    },
    Reload {
        source: PathBuf,
        opts: PlaybackOptions,
        ack: Sender<Result<(), String>>,
    },
    Play,
    Pause,
    SetLooping(bool),
    Stop,
}

/// Configuration handed to the playback thread at spawn.
struct ServerConfig {
    source: PathBuf,
    opts: PlaybackOptions,
    want_playing: bool,
    hwnd: isize,
    width: u32,
    height: u32,
    /// Spawn handshake: `Ok` once an engine is ready to draw, `Err` when
    /// neither mode could start.
    ready: Sender<Result<(), String>>,
    frames: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    video_size: Arc<AtomicU64>,
    engine_mode: Arc<AtomicU8>,
}

/// Playback thread entry point. Everything COM/D3D11/MF happens here —
/// no interface crosses a thread boundary (AGENTS §4).
fn playback_thread(config: ServerConfig, cmd_rx: Receiver<EngineCmd>) {
    // COM first, then MF; declared in this order so the reverse-order
    // scope exit runs MFShutdown before CoUninitialize (design-m2 §3.3).
    // STA as designed: the MTA experiment (notes §11.1) reproduced
    // byte-identical freeze stacks, so the apartment is not the variable —
    // the fix under test is the split decode/render device (§4.5.2).
    let com = match ComInit::new(COINIT_APARTMENTTHREADED) {
        Ok(com) => com,
        Err(err) => {
            let _ = config.ready.send(Err(err.to_string()));
            return;
        }
    };
    let mf = match MfStartup::new() {
        Ok(mf) => mf,
        Err(err) => {
            let _ = config.ready.send(Err(err.to_string()));
            return;
        }
    };
    let ready = config.ready.clone();
    match Server::new(config) {
        Ok(mut server) => {
            let _ = ready.send(Ok(()));
            server.run(&cmd_rx);
            // Engine release (Server::drop) happens here, before this
            // thread's MFShutdown/CoUninitialize below.
            drop(server);
        }
        Err(reason) => {
            let _ = ready.send(Err(reason));
        }
    }
    drop(mf);
    drop(com);
}

/// Active presentation mode of the playback thread (design §4.4).
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// NV12 `TransferVideoFrame` + our video processor + our swapchain.
    FrameServer,
    /// The proven `MF_MEDIA_ENGINE_PLAYBACK_HWND` path (wrong colour on
    /// BT.601 content by design — the documented fallback).
    Rendering,
}

/// Frame-server frame path across the two D3D11 devices (design §4.5.2):
/// transfer destination + CPU-read staging on the decode device, CPU-write
/// staging + video-processor input on the render device. One frame crosses
/// as: `TransferVideoFrame` → GPU copy → `Map(READ)` (barrier) → plane
/// `memcpy` → `Unmap` → GPU copy into `dst`.
struct Nv12Bridge {
    /// NV12 destination of `TransferVideoFrame` (decode device). Bind
    /// flags mirror the E3 probe: RENDER_TARGET first (the engine's blit
    /// wants it), plain 0 as the measured fallback.
    src: ID3D11Texture2D,
    /// `IUnknown` view of `src` (the API takes `IUnknown`).
    src_view: IUnknown,
    /// CPU-read staging on the decode device: `Map(READ)` blocks in the
    /// kernel until the GPU copy completes — the frame barrier, no spin.
    staging_read: ID3D11Texture2D,
    /// CPU-write staging on the render device.
    staging_write: ID3D11Texture2D,
    /// NV12 input of the video processor (render device).
    dst: ID3D11Texture2D,
}

/// One engine plus its GPU objects, owned by the playback thread.
struct Server {
    source: PathBuf,
    opts: PlaybackOptions,
    /// Desired play state; applied to every engine this server creates and
    /// re-applied after a reattach (a detach paused the engine).
    want_playing: bool,
    mode: Mode,
    /// The frame-server engine, or the rendering engine while attached;
    /// `None` in rendering mode while detached (the HWND is a creation
    /// attribute there — today's semantics).
    engine: Option<IMFMediaEngine>,
    /// Playback HWND of the *rendering* engine; `0` for a frame-server
    /// engine, which has none (this is also the `shutdown_or_release`
    /// switch that selects the release path — U5).
    engine_hwnd: isize,
    /// Colour tags from the SPS/VUI and the stream frame rate: read from a
    /// bare source reader *before* the engine is built (design §4.2).
    colour: ColourTags,
    frame_rate: (u32, u32),
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Host surface bound right now; `None` while detached.
    hwnd: Option<isize>,
    /// Last bound surface size (kept across a detach so a reload can
    /// rebuild the pipeline before the next attach).
    size: (u32, u32),
    swapchain: Option<IDXGISwapChain1>,
    /// Render-target view of the back buffer: the black clear outside the
    /// video's destination rect (letterbox bars, `quality.md` §4.8).
    rtv: Option<ID3D11RenderTargetView>,
    pipeline: Option<E3bPipeline>,
    /// Decode-side device + context (design §4.5.2): MF's DXGI manager,
    /// the transfer destination and the read staging live here, so decode
    /// and 3D never share one device (the freeze, notes §11). Created
    /// lazily on the first frame-server start; idle in rendering mode.
    mf_device: Option<ID3D11Device>,
    mf_context: Option<ID3D11DeviceContext>,
    /// The two-device frame bridge (design §4.5.2); `None` while no
    /// frame-server engine exists.
    bridge: Option<Nv12Bridge>,
    /// Native video size, mirrored into `video_size_pub`.
    video_size: (u32, u32),
    /// Consecutive frame-pipeline failures; ≥ [`FALLBACK_ERRORS`] trips
    /// the one-way fallback to rendering mode (design §4.4).
    errors: u32,
    /// Last `FRAMES_RENDERED` poll (rendering-mode stats, ~1 s).
    stat_tick: Instant,
    frames: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    video_size_pub: Arc<AtomicU64>,
    engine_mode_pub: Arc<AtomicU8>,
}

/// Route to start for **one load** (`design-engine-mode.md` §4), in order:
/// the dev A/B evidence knob `CASCADE_ENGINE_RENDER=1` (precedent
/// `CASCADE_GATE=B`) forces rendering regardless of the setting, then the
/// user's `engine_mode` decides — `efficient` renders directly, `accurate`
/// takes the frame-server with its own one-way fallback. Pure so the rule is
/// unit-testable without a GPU; the route is decided here once and never
/// changes on a running engine (AGENTS §3.15).
fn start_mode(requested: PresentationMode, forced_by_env: bool) -> Mode {
    if forced_by_env || requested == PresentationMode::Efficient {
        Mode::Rendering
    } else {
        Mode::FrameServer
    }
}

impl Server {
    /// Build the device and start an engine on `config.hwnd`, preferring
    /// frame-server mode and falling back to rendering (design §4.4). Any
    /// failure here fails the spawn handshake, i.e. `attach_surface`.
    fn new(config: ServerConfig) -> Result<Self, String> {
        let (device, context) = create_d3d11(true).map_err(|err| err.to_string())?;
        let mut server = Self {
            source: config.source,
            opts: config.opts,
            want_playing: config.want_playing,
            mode: Mode::FrameServer,
            engine: None,
            engine_hwnd: 0,
            colour: ColourTags::default(),
            frame_rate: DEFAULT_FRAME_RATE,
            device,
            context,
            hwnd: Some(config.hwnd),
            size: (config.width, config.height),
            swapchain: None,
            rtv: None,
            pipeline: None,
            mf_device: None,
            mf_context: None,
            bridge: None,
            video_size: (0, 0),
            errors: 0,
            stat_tick: Instant::now(),
            frames: config.frames,
            dropped: config.dropped,
            video_size_pub: config.video_size,
            engine_mode_pub: config.engine_mode,
        };
        // Mode selection, once per load (`start_mode`, design-engine-mode
        // §4): the A/B knob or an `efficient` request starts rendering
        // directly; `accurate` (default) falls through to the frame-server
        // with its documented one-way fallback to rendering.
        let forced = std::env::var("CASCADE_ENGINE_RENDER").is_ok_and(|value| value == "1");
        if start_mode(server.opts.engine_mode, forced) == Mode::Rendering {
            server.mode = Mode::Rendering;
            server
                .start_rendering(config.hwnd)
                .map_err(|err| format!("rendering mode: {err}"))?;
            server.publish_mode();
            return Ok(server);
        }
        match server.start_frame_server(config.hwnd, config.width, config.height) {
            Ok(()) => {
                server.publish_mode();
                Ok(server)
            }
            Err(first) => {
                server.release_frame_server();
                server.mode = Mode::Rendering;
                match server.start_rendering(config.hwnd) {
                    Ok(()) => {
                        probe!("[p1] frame-server start failed ({first}); rendering mode instead");
                        server.publish_mode();
                        Ok(server)
                    }
                    Err(second) => Err(format!("frame-server: {first}; rendering: {second}")),
                }
            }
        }
    }

    fn publish_mode(&self) {
        let value = match self.mode {
            Mode::FrameServer => MODE_FRAME_SERVER,
            Mode::Rendering => MODE_RENDERING,
        };
        self.engine_mode_pub.store(value, Ordering::Release);
    }

    /// Frame-server start: colour tags + frame rate from a bare source
    /// reader **before** the engine exists (design §4.2), then the engine,
    /// then the GPU surface.
    fn start_frame_server(&mut self, hwnd: isize, width: u32, height: u32) -> Result<(), String> {
        self.start_frame_server_engine()?;
        self.bind_surface(hwnd, width, height)
    }

    /// Engine half of [`Self::start_frame_server`] — also the rebuild path
    /// for a reload or a reattach after a failed reload (no surface).
    fn start_frame_server_engine(&mut self) -> Result<(), String> {
        // Bare reader (no D3D manager): the bitstream is the only
        // trustworthy matrix source (notes D1). It is dropped before the
        // engine opens the file again, so no handle is held across.
        let (colour, frame_rate) = stream_info(&self.source)?;
        self.colour = colour;
        self.frame_rate = frame_rate;
        probe!(
            "[p1] stream info: matrix={} range={} (VUI matrix_coefficients={})",
            colour.matrix.as_str(),
            colour.range.as_str(),
            colour
                .matrix_coefficients
                .map(|c| c.to_string())
                .unwrap_or_else(|| "<absent>".to_string())
        );
        // Decode-side device (design §4.5.2): MF's engine, the transfer
        // texture and the read staging live on their own device, so the
        // driver never runs decode and 3D work on one device (the freeze,
        // notes §11). Created lazily; a failure here is a frame-server
        // init failure and falls back to rendering like any other (§4.4).
        // The device stays alive for the Server's lifetime (idle devices
        // do no GPU work) and is reused across reloads.
        if self.mf_device.is_none() {
            let (mf_device, mf_context) = create_d3d11(true).map_err(|err| err.to_string())?;
            self.mf_device = Some(mf_device);
            self.mf_context = Some(mf_context);
        }
        let mf_device = self
            .mf_device
            .as_ref()
            .ok_or_else(|| String::from("no decode device"))?;
        let engine = create_frame_server_engine(mf_device)?;
        if let Err(reason) = run_source(&engine, &self.source, self.opts.looping, self.want_playing)
        {
            // Frame-server engine: no HWND → the release path (U5).
            shutdown_or_release(engine, 0);
            return Err(reason);
        }
        self.engine = Some(engine);
        self.engine_hwnd = 0;
        self.publish_video_size()?;
        self.create_bridge()?;
        self.dropped.store(0, Ordering::Release);
        Ok(())
    }

    /// Rendering start (fallback and rendering reload): the proven
    /// variant-A engine bound to `hwnd`, muted before any source can make
    /// a sound (design-m2 §5).
    fn start_rendering(&mut self, hwnd: isize) -> Result<(), String> {
        let engine = create_rendering_engine(hwnd)?;
        if let Err(reason) = run_source(&engine, &self.source, self.opts.looping, self.want_playing)
        {
            shutdown_or_release(engine, hwnd);
            return Err(reason);
        }
        self.engine = Some(engine);
        self.engine_hwnd = hwnd;
        self.hwnd = Some(hwnd);
        self.publish_video_size()?;
        Ok(())
    }

    /// Publish the native video size (both modes) and remember it.
    fn publish_video_size(&mut self) -> Result<(), String> {
        let engine = self.engine.as_ref().ok_or("no engine")?;
        let (mut width, mut height) = (0u32, 0u32);
        // SAFETY: live engine; out-pointers are locals that outlive the call.
        unsafe { engine.GetNativeVideoSize(Some(&mut width), Some(&mut height)) }
            .map_err(|err| format!("GetNativeVideoSize: {}", err.message()))?;
        if width == 0 || height == 0 {
            return Err(String::from("zero video size"));
        }
        self.video_size = (width, height);
        self.video_size_pub.store(
            ((width as u64) << 32) | u64::from(height),
            Ordering::Release,
        );
        Ok(())
    }

    /// Build the four textures of the two-device frame bridge (design
    /// §4.5.2): NV12 transfer destination + CPU-read staging on the decode
    /// device, CPU-write staging + NV12 video-processor input on the
    /// render device. NV12 bind flags mirror the E3 probe: RENDER_TARGET
    /// first (the video processor input view wants it), plain 0 as the
    /// measured fallback.
    fn create_bridge(&mut self) -> Result<(), String> {
        let (width, height) = self.video_size;
        let mf_device = self
            .mf_device
            .as_ref()
            .ok_or_else(|| String::from("no decode device"))?;
        let rt = D3D11_BIND_RENDER_TARGET.0 as u32;
        let src = match nv12_texture(mf_device, width, height, D3D11_USAGE_DEFAULT, rt, 0) {
            Ok(texture) => texture,
            Err(first) => {
                probe!("[p1] NV12 src bind=RT failed ({first}); retrying bind=0");
                nv12_texture(mf_device, width, height, D3D11_USAGE_DEFAULT, 0, 0)?
            }
        };
        let dst = match nv12_texture(&self.device, width, height, D3D11_USAGE_DEFAULT, rt, 0) {
            Ok(texture) => texture,
            Err(first) => {
                probe!("[p1] NV12 dst bind=RT failed ({first}); retrying bind=0");
                nv12_texture(&self.device, width, height, D3D11_USAGE_DEFAULT, 0, 0)?
            }
        };
        let staging_read = nv12_texture(
            mf_device,
            width,
            height,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
        )?;
        let staging_write = nv12_texture(
            &self.device,
            width,
            height,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_WRITE.0 as u32,
        )?;
        // SAFETY: a texture is any COM object; QueryInterface cannot fail
        // for IUnknown.
        let src_view: IUnknown = src
            .cast()
            .map_err(|err| format!("NV12 as IUnknown: {}", err.message()))?;
        self.bridge = Some(Nv12Bridge {
            src,
            src_view,
            staging_read,
            staging_write,
            dst,
        });
        Ok(())
    }

    /// Bind (or rebind) the host surface: swapchain + video processor for
    /// `hwnd` at `width`×`height`. Releases any previous surface first
    /// (reattach = new swapchain only — U4).
    fn bind_surface(&mut self, hwnd: isize, width: u32, height: u32) -> Result<(), String> {
        self.release_surface();
        let swapchain = create_swapchain(
            &self.device,
            hwnd,
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
        )
        .map_err(|err| err.to_string())?;
        // SAFETY: the swapchain owns its buffer; it dies with the chain.
        let back_buffer: ID3D11Texture2D = unsafe { swapchain.GetBuffer(0) }
            .map_err(|err| format!("GetBuffer: {}", err.message()))?;
        // SAFETY: live device and texture; `None` = default view
        // description; the out-param starts as None.
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        unsafe {
            self.device
                .CreateRenderTargetView(&back_buffer, None, Some(&mut rtv))
        }
        .map_err(|err| format!("CreateRenderTargetView: {}", err.message()))?;
        let rtv = rtv.ok_or_else(|| String::from("no render target view"))?;
        let mut pipeline = E3bPipeline::create(
            &self.device,
            &self.context,
            &back_buffer,
            self.frame_rate,
            (width, height),
            self.colour,
        )
        .map_err(|reason| format!("video processor: {reason}"))?;
        pipeline.set_fit(self.opts.fit);
        self.swapchain = Some(swapchain);
        self.rtv = Some(rtv);
        self.pipeline = Some(pipeline);
        self.hwnd = Some(hwnd);
        self.size = (width, height);
        Ok(())
    }

    /// Release the surface objects; the engine and the remembered HWND are
    /// untouched (a detach clears `hwnd` itself).
    fn release_surface(&mut self) {
        self.pipeline = None;
        self.rtv = None;
        self.swapchain = None;
    }

    /// Tear the frame-server side down completely: engine (release path —
    /// no HWND, U5), surface, frame bridge. Used by the fallback and by
    /// `Drop`.
    fn release_frame_server(&mut self) {
        if let Some(engine) = self.engine.take() {
            shutdown_or_release(engine, 0);
        }
        self.engine_hwnd = 0;
        self.release_surface();
        self.bridge = None;
    }

    /// One-way trip to rendering mode (design §4.4): release the
    /// frame-server side, then start the proven engine on the current HWND
    /// — or defer to the next attach when detached.
    fn fallback_to_rendering(&mut self, reason: &str) {
        probe!("[p1] frame-server -> rendering fallback: {reason}");
        self.release_frame_server();
        self.mode = Mode::Rendering;
        self.errors = 0;
        if let Some(hwnd) = self.hwnd {
            if let Err(err) = self.start_rendering(hwnd) {
                probe!("[p1] rendering mode could not start: {err}");
                self.engine = None;
            }
        }
        self.dropped.store(0, Ordering::Release);
        self.publish_mode();
    }
}

impl Server {
    /// The playback thread's main loop: drain commands first (an ack must
    /// not wait behind a frame), then run one tick of the active mode.
    fn run(&mut self, cmd_rx: &Receiver<EngineCmd>) {
        probe!(
            "[p1] run enter: mode={} hwnd={:?} want_playing={}",
            match self.mode {
                Mode::FrameServer => "frame-server",
                Mode::Rendering => "rendering",
            },
            self.hwnd,
            self.want_playing
        );
        loop {
            let mut stop = false;
            loop {
                match cmd_rx.try_recv() {
                    Ok(cmd) => {
                        if self.handle(cmd) {
                            stop = true;
                            break;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    // The backend dropped without a Stop (process
                    // teardown): exit the same way.
                    Err(mpsc::TryRecvError::Disconnected) => {
                        stop = true;
                        break;
                    }
                }
            }
            if stop {
                break;
            }
            match self.mode {
                Mode::FrameServer => self.tick_frame_server(),
                Mode::Rendering => self.tick_rendering(),
            }
        }
        // `status` stops reporting a mode the moment the thread is gone.
        self.engine_mode_pub.store(MODE_NONE, Ordering::Release);
    }

    /// Apply one command. Returns `true` when the thread must exit.
    fn handle(&mut self, cmd: EngineCmd) -> bool {
        match cmd {
            EngineCmd::Stop => true,
            EngineCmd::Play => {
                self.want_playing = true;
                if let Some(engine) = &self.engine {
                    // SAFETY: live engine on its own thread.
                    if let Err(err) = unsafe { engine.Play() } {
                        probe!("[p1] Play failed: {}", err.message());
                    }
                }
                false
            }
            EngineCmd::Pause => {
                self.want_playing = false;
                if let Some(engine) = &self.engine {
                    // SAFETY: live engine on its own thread.
                    if let Err(err) = unsafe { engine.Pause() } {
                        probe!("[p1] Pause failed: {}", err.message());
                    }
                }
                false
            }
            EngineCmd::SetLooping(on) => {
                self.opts.looping = on;
                if let Some(engine) = &self.engine {
                    // SAFETY: live engine on its own thread.
                    if let Err(err) = unsafe { engine.SetLoop(on) } {
                        probe!("[p1] SetLoop failed: {}", err.message());
                    }
                }
                false
            }
            EngineCmd::Attach {
                hwnd,
                width,
                height,
                ack,
            } => {
                let result = self.on_attach(hwnd, width, height);
                let _ = ack.send(result);
                false
            }
            EngineCmd::Detach { ack } => {
                let result = self.on_detach();
                let _ = ack.send(result);
                false
            }
            EngineCmd::Reload { source, opts, ack } => {
                let result = self.on_reload(source, opts);
                if result.is_err() && matches!(self.mode, Mode::FrameServer) {
                    // Reload failure = frame-server init failure (§4.4):
                    // one-way trip to rendering; the file may simply be
                    // broken, in which case rendering fails too and the
                    // desktop keeps the last frame (never blank).
                    self.fallback_to_rendering("reload failed");
                }
                let _ = ack.send(result);
                false
            }
        }
    }

    /// Rebind the host surface (design §4.5.1): a frame-server engine is
    /// untouched — only the swapchain is rebuilt (the U4 reattach bet) —
    /// while a rendering engine dies with its window and is rebuilt.
    fn on_attach(&mut self, hwnd: isize, width: u32, height: u32) -> Result<(), String> {
        self.size = (width, height);
        match self.mode {
            Mode::FrameServer => {
                if self.engine.is_none() {
                    // Rebuild attempt after a failed reattach restart.
                    self.start_frame_server_engine()?;
                }
                self.bind_surface(hwnd, width, height)?;
                let engine = self
                    .engine
                    .as_ref()
                    .ok_or_else(|| String::from("no engine"))?;
                // A detach paused the engine; re-apply the desired state.
                // SAFETY: live engine on this thread.
                if self.want_playing {
                    unsafe { engine.Play() }.map_err(|err| format!("Play: {}", err.message()))?;
                } else {
                    unsafe { engine.Pause() }.map_err(|err| format!("Pause: {}", err.message()))?;
                }
                Ok(())
            }
            Mode::Rendering => {
                if self.engine.is_some() && self.engine_hwnd == hwnd {
                    return Ok(());
                }
                if let Some(old) = self.engine.take() {
                    shutdown_or_release(old, self.engine_hwnd);
                    self.engine_hwnd = 0;
                }
                self.start_rendering(hwnd)
            }
        }
    }

    /// Release the surface; keep the engine (design §4.5.1).
    fn on_detach(&mut self) -> Result<(), String> {
        match self.mode {
            Mode::FrameServer => {
                // Stop decoding while nothing is drawn, but keep the engine
                // and its source alive: reattach = swapchain only (U4).
                // The file stays open across a detach — open item measured
                // during the `docs/reliability.md` §12 re-run.
                if let Some(engine) = &self.engine {
                    // SAFETY: live engine on this thread.
                    if let Err(err) = unsafe { engine.Pause() } {
                        probe!("[p1] Pause on detach failed: {}", err.message());
                    }
                }
                self.release_surface();
                self.hwnd = None;
                Ok(())
            }
            Mode::Rendering => {
                // Variant-A parity: the rendering engine dies with its
                // window (its `Shutdown()` needs a live HWND — R2/R3) and
                // releases the file with it.
                if let Some(engine) = self.engine.take() {
                    shutdown_or_release(engine, self.engine_hwnd);
                }
                self.engine_hwnd = 0;
                self.hwnd = None;
                Ok(())
            }
        }
    }

    /// Swap the source (design §4.5.1 reload row).
    fn on_reload(&mut self, source: PathBuf, opts: PlaybackOptions) -> Result<(), String> {
        self.source = source;
        self.opts = opts;
        match self.mode {
            Mode::Rendering => {
                // Release first (frees the file), then rebuild on the
                // current window — today's `start_on` semantics. While
                // detached only the source is remembered; an engine-less
                // *attached* backend retries here (an earlier start failed
                // with a bad file, a good one now loads).
                if let Some(old) = self.engine.take() {
                    shutdown_or_release(old, self.engine_hwnd);
                    self.engine_hwnd = 0;
                }
                if let Some(hwnd) = self.hwnd {
                    self.start_rendering(hwnd)?;
                }
                Ok(())
            }
            Mode::FrameServer => {
                // Release the old engine first — that is what frees the
                // file (design §4.5.1) — then rebuild from the bare-reader
                // colour tags (§4.2).
                if let Some(old) = self.engine.take() {
                    shutdown_or_release(old, 0);
                    self.engine_hwnd = 0;
                }
                self.bridge = None;
                self.start_frame_server_engine()?;
                if let Some(hwnd) = self.hwnd {
                    // Colour/frame rate may have changed: rebuild the
                    // pipeline on the remembered surface.
                    let (width, height) = self.size;
                    self.bind_surface(hwnd, width, height)?;
                }
                self.errors = 0;
                Ok(())
            }
        }
    }

    /// Frame-server tick (design §4.3): `OnVideoStreamTick` per vblank,
    /// S_OK → transfer NV12 → blit → `Present(1, 0)`; S_FALSE → drain and
    /// wait; failures count toward the rendering fallback.
    fn tick_frame_server(&mut self) {
        // Detached or paused: nothing to draw, stay cheap (IDLE_POLL).
        if self.hwnd.is_none() || !self.want_playing {
            pump_messages();
            std::thread::sleep(IDLE_POLL);
            return;
        }
        // Microsoft docs (`OnVideoStreamTick`): call once per vertical
        // blank; S_OK = new frame ready → `TransferVideoFrame`. The safe
        // wrapper cannot tell S_OK from S_FALSE (both are success
        // HRESULTs and `HRESULT::ok` accepts both), so the raw HRESULT is
        // read from the vtable (the E3 probe's exact call).
        let hr = self.engine.as_ref().map(|engine| {
            let mut pts: i64 = 0;
            // SAFETY: live engine on this thread; the call fills `pts`.
            unsafe {
                let vtable = Interface::vtable(engine);
                (vtable.OnVideoStreamTick)(Interface::as_raw(engine), &mut pts).0
            }
        });
        let Some(hr) = hr else {
            // Frame-server mode without an engine (a failed rebuild):
            // idle until the next attach/reload retries.
            pump_messages();
            std::thread::sleep(IDLE_POLL);
            return;
        };
        if hr < 0 {
            self.note_error(format!("OnVideoStreamTick: 0x{hr:08X}"));
            return;
        }
        if hr != 0 {
            // S_FALSE: no new frame this vblank — drain and wait.
            pump_messages();
            std::thread::sleep(TICK_POLL);
            return;
        }
        match self.present_frame() {
            Ok(()) => {
                self.frames.fetch_add(1, Ordering::AcqRel);
                self.errors = 0;
            }
            Err(reason) => self.note_error(reason),
        }
    }

    /// One frame: transfer the decoded NV12 on the decode device, bridge
    /// it across to the render device through CPU staging (design §4.5.2),
    /// then video processor → back buffer → present (§4.3). Any error is a
    /// fallback candidate.
    fn present_frame(&mut self) -> Result<(), String> {
        let (width, height) = self.video_size;
        let rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        let bridge = self
            .bridge
            .as_ref()
            .ok_or_else(|| String::from("no NV12 bridge"))?;
        let mf_context = self
            .mf_context
            .as_ref()
            .ok_or_else(|| String::from("no decode context"))?;
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| String::from("no engine"))?;
        // SAFETY: engine and destination are live on this thread; the full
        // frame rect is the destination (native size — MSDN
        // TransferVideoFrame; E3-3 proved the NV12 bytes are the raw
        // decode, so the matrix is still ours to apply below).
        unsafe { engine.TransferVideoFrame(&bridge.src_view, None, &rect, None) }
            .map_err(|err| format!("TransferVideoFrame: {}", err.message()))?;
        // Decode-device half of the bridge (design §4.5.2): GPU-copy the
        // transferred frame into CPU-read staging, then map it. `Map(READ)`
        // blocks in the kernel until that copy completes — this is the
        // frame barrier (no polling, no `DO_NOT_WAIT`, no busy-wait).
        // SAFETY: live decode context and textures on this thread.
        unsafe { mf_context.CopyResource(&bridge.staging_read, &bridge.src) };
        let mut src_map = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging has CPU read access; the out-param is filled on
        // success.
        unsafe {
            mf_context.Map(
                &bridge.staging_read,
                0,
                D3D11_MAP_READ,
                0,
                Some(&mut src_map),
            )
        }
        .map_err(|err| format!("Map read staging: {}", err.message()))?;
        // Render-device half: map the CPU-write staging, copy both NV12
        // planes across, unmap both sides, then GPU-copy into the video
        // processor's input texture.
        let mut dst_map = D3D11_MAPPED_SUBRESOURCE::default();
        let mapped = unsafe {
            self.context.Map(
                &bridge.staging_write,
                0,
                D3D11_MAP_WRITE,
                0,
                Some(&mut dst_map),
            )
        };
        if let Err(err) = mapped {
            // SAFETY: the read mapping above is still live and paired.
            unsafe { mf_context.Unmap(&bridge.staging_read, 0) };
            return Err(format!("Map write staging: {}", err.message()));
        }
        // `pData` is a plain struct field; both pointers stay valid until
        // the Unmaps below.
        let (src_base, dst_base) = (src_map.pData.cast::<u8>(), dst_map.pData.cast::<u8>());
        if src_base.is_null() || dst_base.is_null() {
            // SAFETY: both mappings are live and paired.
            unsafe {
                self.context.Unmap(&bridge.staging_write, 0);
                mf_context.Unmap(&bridge.staging_read, 0);
            }
            return Err("mapped staging pData is null".into());
        }
        copy_nv12_planes(
            src_base,
            src_map.RowPitch,
            dst_base,
            dst_map.RowPitch,
            width,
            height,
        );
        // SAFETY: both mappings are live; each Unmap pairs its Map.
        unsafe {
            self.context.Unmap(&bridge.staging_write, 0);
            mf_context.Unmap(&bridge.staging_read, 0);
        }
        // SAFETY: live render context and textures on this thread.
        unsafe {
            self.context
                .CopyResource(&bridge.dst, &bridge.staging_write)
        };
        // Letterbox bars are the black clear outside the destination rect
        // (opaque window, quality.md §4.8). Only Contain can leave any
        // uncovered area; Cover/Stretch fill the surface.
        if self.opts.fit == Fit::Contain {
            if let Some(rtv) = &self.rtv {
                // SAFETY: live view; the constant is opaque black.
                unsafe {
                    self.context
                        .ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 0.0])
                };
            }
        }
        let pipeline = self
            .pipeline
            .as_mut()
            .ok_or_else(|| String::from("no video pipeline"))?;
        pipeline
            .blit_texture(&bridge.dst)
            .map_err(|err| err.why())?;
        let swapchain = self
            .swapchain
            .as_ref()
            .ok_or_else(|| String::from("no swapchain"))?;
        // Present with vsync: this is the frame pacing (same as Tier 2).
        // SAFETY: the swapchain is alive for this call.
        let hr = unsafe { swapchain.Present(1, DXGI_PRESENT(0)) };
        if hr.is_err() {
            return Err(format!("Present {hr:?}"));
        }
        Ok(())
    }

    /// Count a frame-pipeline failure: log the first few and then every
    /// ~2 s, and trip the fallback after [`FALLBACK_ERRORS`] in a row
    /// (design §4.4).
    fn note_error(&mut self, reason: String) {
        self.errors = self.errors.saturating_add(1);
        if self.errors <= FALLBACK_ERRORS || self.errors.is_multiple_of(120) {
            probe!("[p1] frame-server error x{}: {reason}", self.errors);
        }
        if self.errors >= FALLBACK_ERRORS {
            self.fallback_to_rendering(&reason);
        }
    }

    /// Rendering tick: the engine presents into its own HWND; this thread
    /// only drains the STA queue (its callbacks) and refreshes the
    /// published counters about once a second (design §4.4).
    fn tick_rendering(&mut self) {
        pump_messages();
        if self.engine.is_none() {
            std::thread::sleep(IDLE_POLL);
            return;
        }
        if self.stat_tick.elapsed() >= STATS_POLL {
            self.stat_tick = Instant::now();
            let counters = self.engine.as_ref().and_then(|engine| {
                let ex = engine.cast::<IMFMediaEngineEx>().ok()?;
                Some((
                    statistic(&ex, MF_MEDIA_ENGINE_STATISTIC_FRAMES_RENDERED).unwrap_or(0.0) as u64,
                    statistic(&ex, MF_MEDIA_ENGINE_STATISTIC_FRAMES_DROPPED).unwrap_or(0.0) as u64,
                ))
            });
            if let Some((rendered, dropped)) = counters {
                self.frames.store(rendered, Ordering::Release);
                self.dropped.store(dropped, Ordering::Release);
            }
        }
        std::thread::sleep(TICK_POLL);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Rendering engines take their real HWND (`Shutdown()` when the
        // window is alive, plain release when it is dead — variant-A
        // parity); frame-server engines pass 0 → always the release path
        // (the E3-proven teardown, U5). Then the surface objects go with
        // the swapchain they belong to.
        if let Some(engine) = self.engine.take() {
            shutdown_or_release(engine, self.engine_hwnd);
        }
        self.release_surface();
        self.bridge = None;
    }
}

/// Colour tags + frame rate for `source` from a **bare** source reader:
/// no D3D manager, no output negotiation — container attributes only.
/// The reader (and the file handle it holds) dies inside this call, so the
/// engine can open the same file right after (design §4.2).
fn stream_info(source: &Path) -> Result<(ColourTags, (u32, u32)), String> {
    let url = file_url(source);
    let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is nul-terminated and outlives the call; MF does not
    // keep the pointer.
    let reader = unsafe { MFCreateSourceReaderFromURL(PCWSTR::from_raw(wide.as_ptr()), None) }
        .map_err(|err| format!("open for stream info: {}", err.message()))?;
    let colour = sequence_header_tags(&reader);
    let frame_rate = query_frame_rate(&reader).unwrap_or(DEFAULT_FRAME_RATE);
    Ok((colour, frame_rate))
}

/// One NV12 texture with explicit usage/bind/cpu-access flags — the E3
/// probe's `tex()` pattern (`probe.rs`), parameterised over the device so
/// both sides of the bridge can use it (design §4.5.2).
fn nv12_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    usage: D3D11_USAGE,
    bind: u32,
    access: u32,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: access,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: valid descriptor; out-param starts as None.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|err| format!("CreateTexture2D NV12: {}", err.message()))?;
    texture.ok_or_else(|| String::from("no NV12 texture"))
}

/// Copy one NV12 frame between two mapped staging textures: the Y plane
/// first, then interleaved UV at `RowPitch * Height` — the E3 readback
/// layout (`probe.rs`), reversed. Both planes store `width` bytes per row
/// (one UV pair per luma pixel column); the UV plane has half the rows.
fn copy_nv12_planes(
    src: *const u8,
    src_pitch: u32,
    dst: *mut u8,
    dst_pitch: u32,
    width: u32,
    height: u32,
) {
    // SAFETY: both pointers are live mapped regions the caller holds
    // (Map succeeded, Unmap not yet); D3D11 guarantees RowPitch ≥ the
    // row byte count on both sides and both textures are `width × height`
    // NV12, so every row touched here stays inside its mapping.
    unsafe {
        copy_nv12_plane(src, src_pitch, dst, dst_pitch, width, height);
        copy_nv12_plane(
            src.add(src_pitch as usize * height as usize),
            src_pitch,
            dst.add(dst_pitch as usize * height as usize),
            dst_pitch,
            width,
            height / 2,
        );
    }
}

/// Row-by-row copy of one NV12 plane (same row bytes on both sides).
fn copy_nv12_plane(
    src: *const u8,
    src_pitch: u32,
    dst: *mut u8,
    dst_pitch: u32,
    row_bytes: u32,
    rows: u32,
) {
    for row in 0..rows {
        // SAFETY: as `copy_nv12_planes`: the caller's mappings outlive
        // this call and row `row` of each plane is inside its mapping
        // (pitch ≥ row_bytes, `rows` bounded by the texture height).
        unsafe {
            std::ptr::copy_nonoverlapping(
                src.add(src_pitch as usize * row as usize),
                dst.add(dst_pitch as usize * row as usize),
                row_bytes as usize,
            );
        }
    }
}

/// The frame-server engine — the exact E3-1 setup (design §4.2): no
/// `MF_MEDIA_ENGINE_PLAYBACK_HWND`, the DXGI manager bound to `device`,
/// the mandatory callback sink, muted before a source can make a sound
/// (design-m2 §5).
fn create_frame_server_engine(device: &ID3D11Device) -> Result<IMFMediaEngine, String> {
    let manager = create_dxgi_device_manager(device)?;
    let attrs = new_engine_attrs()?;
    // Frame-server mode: PLAYBACK_HWND is deliberately NOT set (MSDN
    // `CreateInstance`: rendering mode requires PLAYBACK_HWND or
    // PLAYBACK_VISUAL; without either the engine is frame-server).
    // SAFETY: documented attribute; SetUnknown AddRefs the manager.
    unsafe { attrs.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager) }
        .map_err(|err| format!("SetUnknown DXGI_MANAGER: {}", err.message()))?;
    let engine = instantiate_engine(&attrs)?;
    // SAFETY: created above; release path (no HWND) on failure.
    mute_engine(engine, 0)
}

/// The rendering engine (the proven variant-A path, design §4.4).
fn create_rendering_engine(hwnd: isize) -> Result<IMFMediaEngine, String> {
    let attrs = new_engine_attrs()?;
    // SAFETY: the HWND travels as a UINT64 (Microsoft docs:
    // MF_MEDIA_ENGINE_PLAYBACK_HWND).
    unsafe { attrs.SetUINT64(&MF_MEDIA_ENGINE_PLAYBACK_HWND, hwnd as u64) }
        .map_err(|err| format!("SetUINT64 PLAYBACK_HWND: {}", err.message()))?;
    let engine = instantiate_engine(&attrs)?;
    mute_engine(engine, hwnd)
}

/// Attributes object with the mandatory event sink set (required in every
/// mode — MSDN `MF_MEDIA_ENGINE_CALLBACK`; measured `MF_E_ATTRIBUTENOTFOUND`
/// without it on build 26200.9711).
fn new_engine_attrs() -> Result<IMFAttributes, String> {
    let mut attrs: Option<IMFAttributes> = None;
    // SAFETY: valid out-pointer; size 0 is documented for "no initial
    // attributes".
    unsafe { MFCreateAttributes(&mut attrs, 0) }
        .map_err(|err| format!("MFCreateAttributes: {}", err.message()))?;
    let attrs = attrs.ok_or_else(|| String::from("MFCreateAttributes returned no object"))?;
    let notify: IMFMediaEngineNotify = EngineNotify.into();
    // SAFETY: the sink is mandatory in every mode; the engine AddRefs it.
    unsafe { attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify) }
        .map_err(|err| format!("SetUnknown CALLBACK: {}", err.message()))?;
    Ok(attrs)
}

/// Shared `CreateInstance` half of both engine factories.
fn instantiate_engine(attrs: &IMFAttributes) -> Result<IMFMediaEngine, String> {
    // SAFETY: documented in-proc class factory; null outer interface.
    let factory: IMFMediaEngineClassFactory =
        unsafe { CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER) }
            .map_err(|err| format!("class factory: {}", err.message()))?;
    // SAFETY: dwflags 0 is the only documented value for CreateInstance.
    unsafe { factory.CreateInstance(0, attrs) }
        .map_err(|err| format!("CreateInstance: {}", err.message()))
}

/// Mute before any source exists (design-m2 §5). `hwnd` is the rendering
/// window (or 0 for a frame-server engine) and selects the teardown path
/// in [`shutdown_or_release`] if the mute fails.
fn mute_engine(engine: IMFMediaEngine, hwnd: isize) -> Result<IMFMediaEngine, String> {
    match unsafe { engine.SetMuted(true) } {
        Ok(()) => Ok(engine),
        Err(err) => {
            shutdown_or_release(engine, hwnd);
            Err(format!("SetMuted: {}", err.message()))
        }
    }
}

/// Load `source` into an engine: SetSource → Load → bounded readiness
/// wait → SetLoop → Play. Runs on the playback thread (its own pump keeps
/// that thread responsive during the wait).
fn run_source(
    engine: &IMFMediaEngine,
    source: &Path,
    looping: bool,
    play: bool,
) -> Result<(), String> {
    let url = file_url(source);
    // SAFETY: engine is live; the BSTR outlives the call.
    unsafe { engine.SetSource(&BSTR::from(url.as_str())) }
        .map_err(|err| format!("SetSource: {}", err.message()))?;
    // SAFETY: live engine.
    unsafe { engine.Load() }.map_err(|err| format!("Load: {}", err.message()))?;
    wait_ready(engine)?;
    // SAFETY: live engine.
    unsafe { engine.SetLoop(looping) }.map_err(|err| format!("SetLoop: {}", err.message()))?;
    if play {
        // SAFETY: live engine.
        unsafe { engine.Play() }.map_err(|err| format!("Play: {}", err.message()))?;
    }
    Ok(())
}

/// Bounded wait for the first decodable frame. Readiness is polled through
/// `GetReadyState` instead of consumed from the sink: one code path, and
/// the nested pump keeps this thread's queue drained (design-m2 §3.1).
fn wait_ready(engine: &IMFMediaEngine) -> Result<(), String> {
    let deadline = Instant::now() + LOAD_TIMEOUT;
    loop {
        // SAFETY: live engine; no out-parameters.
        let ready = unsafe { engine.GetReadyState() };
        if ready as i32 >= MF_MEDIA_ENGINE_READY_HAVE_CURRENT_DATA.0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(timeout_reason(engine, ready));
        }
        pump_messages();
        std::thread::sleep(LOAD_POLL);
    }
}

fn timeout_reason(engine: &IMFMediaEngine, ready: u16) -> String {
    // SAFETY: live engine; the error object is only read when present.
    match unsafe { engine.GetError() } {
        Ok(error) => {
            // SAFETY: scalar getter on the error object.
            let code = unsafe { error.GetErrorCode() };
            if code == 0 {
                format!(
                    "no data after {} ms (ready state {ready})",
                    LOAD_TIMEOUT.as_millis()
                )
            } else {
                format!("engine error 0x{code:04X} at ready state {ready}")
            }
        }
        Err(_) => format!(
            "no data after {} ms (ready state {ready})",
            LOAD_TIMEOUT.as_millis()
        ),
    }
}

/// Nested pump so a load wait never starves this thread's messages: the
/// engine's callbacks are posted to the thread that created it.
fn pump_messages() {
    let mut msg = MSG::default();
    loop {
        // SAFETY: standard Win32 message pump; `msg` is a valid
        // out-parameter and `None` means "thread messages".
        let has_message = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool();
        if !has_message {
            break;
        }
        if msg.message == WM_QUIT {
            // SAFETY: hand the quit back to the outer loop.
            unsafe { PostQuitMessage(msg.wParam.0 as i32) };
            break;
        }
        // SAFETY: `msg` was filled by PeekMessageW.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Shut the engine down — or release it without `Shutdown()` when its
/// playback HWND no longer exists (or when it has none at all: a
/// frame-server engine always takes that branch — U5).
///
/// Measured on build 26200.9711 (`docs/notes-s7-shellrestart.md` R2/R3):
/// `IMFMediaEngine::Shutdown()` blocks **forever** once the
/// `MF_MEDIA_ENGINE_PLAYBACK_HWND` window is dead — the shell can destroy
/// that window before any of our detach paths runs, and the STA render
/// thread cannot pump messages from inside the blocking call. A plain
/// `Release` returns promptly and leaves no file lock behind (R4: probes
/// `m3`/`m4`, reattach reloaded the same file, all gates green).
/// Every `Shutdown()` call site goes through this helper.
fn shutdown_or_release(engine: IMFMediaEngine, hwnd: isize) {
    // SAFETY: pure validity check on a raw handle (Microsoft docs:
    // IsWindow tolerates stale handles).
    let alive = unsafe { IsWindow(Some(HWND(hwnd as _))) }.as_bool();
    probe!("[wd] m1: MediaEngine::Shutdown enter (hwnd={hwnd}, IsWindow={alive})");
    if alive {
        // SAFETY: created by us on this thread; releases the playback
        // window first (design-m2 §3.3).
        let _ = unsafe { engine.Shutdown() };
        probe!("[wd] m2: MediaEngine::Shutdown returned");
    } else {
        probe!("[wd] m3: hwnd dead - skipping Shutdown, releasing engine");
        drop(engine);
        probe!("[wd] m4: engine Release returned");
    }
}

/// The measured `fps` behind `BackendStats::fps` (M4 S3b): frames over the
/// wall time between two samples. In frame-server mode the counter is our
/// transfer count; in rendering mode it is the engine's own
/// `FRAMES_RENDERED`, refreshed by the playback thread (~1 s).
///
/// Returns the rate plus the sample to store for the next call. A `0.0`
/// rate means "not measurable yet" and `status` omits the field rather
/// than printing a fake zero: the first sample, a counter that restarted
/// because the engine was recreated, or an interval shorter than 500 ms
/// (two refreshes close together — the anchor is kept so the window grows).
fn fps_rate(previous: Option<(u64, Instant)>, frames: u64, now: Instant) -> (f32, (u64, Instant)) {
    const MEASURE_WINDOW: Duration = Duration::from_millis(500);
    let Some((previous_frames, anchor)) = previous else {
        return (0.0, (frames, now));
    };
    if frames < previous_frames {
        // The engine was recreated (`attach_surface` reloads from 0):
        // restart instead of dividing a negative delta.
        return (0.0, (frames, now));
    }
    let elapsed = now.duration_since(anchor);
    if elapsed < MEASURE_WINDOW {
        return (0.0, (previous_frames, anchor));
    }
    let rate = (frames - previous_frames) as f64 / elapsed.as_secs_f64();
    (rate as f32, (frames, now))
}

/// Read one engine statistic; the PROPVARIANT type depends on the counter.
fn statistic(ex: &IMFMediaEngineEx, id: MF_MEDIA_ENGINE_STATISTIC) -> Option<f64> {
    // SAFETY: GetStatistics initializes the PROPVARIANT; only the scalar
    // members used by numeric counters are read, each behind the variant
    // type it reports. Scalar variants hold no allocation, so no
    // PropVariantClear is needed.
    unsafe {
        let pv = ex.GetStatistics(id).ok()?;
        match pv.Anonymous.Anonymous.vt.0 {
            // VT_I4, VT_R8, VT_UI4, VT_UI8, VT_I8.
            3 => Some(pv.Anonymous.Anonymous.Anonymous.lVal as f64),
            5 => Some(pv.Anonymous.Anonymous.Anonymous.dblVal),
            19 => Some(pv.Anonymous.Anonymous.Anonymous.ulVal as f64),
            20 => Some(pv.Anonymous.Anonymous.Anonymous.uhVal as f64),
            21 => Some(pv.Anonymous.Anonymous.Anonymous.hVal as f64),
            _ => None,
        }
    }
}

/// `Path` → `file:///` URL for `SetSource`, percent-encoded (MF wants a
/// URL; the spaces in a typical user path must not survive).
fn file_url(path: &Path) -> String {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut raw = absolute.to_string_lossy().replace('\\', "/");
    if let Some(rest) = raw.strip_prefix("//?/UNC/") {
        raw = format!("//{rest}");
    } else if let Some(rest) = raw.strip_prefix("//?/") {
        raw = rest.to_string();
    }
    // `raw` is now "C:/..." or "//server/share/...".
    let mut url = if raw.starts_with("//") {
        String::from("file:")
    } else {
        String::from("file:///")
    };
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'/'
            | b':'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'=' => url.push(byte as char),
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_percent_encodes_spaces() {
        // canonicalize fails for a path that does not exist; the fallback
        // must still produce a valid URL.
        assert_eq!(
            file_url(Path::new(r"C:\videos\my clip.mp4")),
            "file:///C:/videos/my%20clip.mp4"
        );
    }

    #[test]
    fn file_url_keeps_plain_paths() {
        assert_eq!(
            file_url(Path::new(r"C:\videos\clip.mp4")),
            "file:///C:/videos/clip.mp4"
        );
    }

    #[test]
    fn start_mode_follows_env_then_the_requested_route() {
        // `design-engine-mode.md` §4 order: the dev A/B knob first, then
        // the user's setting; `accurate` (the default) is the frame-server.
        assert_eq!(
            start_mode(PresentationMode::Accurate, false),
            Mode::FrameServer
        );
        assert_eq!(
            start_mode(PresentationMode::Efficient, false),
            Mode::Rendering
        );
        assert_eq!(
            start_mode(PresentationMode::Accurate, true),
            Mode::Rendering
        );
        assert_eq!(
            start_mode(PresentationMode::Efficient, true),
            Mode::Rendering
        );
    }

    #[test]
    fn fps_rate_needs_two_samples_before_it_reports() {
        let t0 = Instant::now();
        let (rate, sample) = fps_rate(None, 0, t0);
        assert_eq!(rate, 0.0, "one sample is not a rate");
        assert_eq!(sample, (0, t0));
    }

    #[test]
    fn fps_rate_divides_frames_by_the_window() {
        let t0 = Instant::now();
        let (rate, sample) = fps_rate(Some((100, t0)), 220, t0 + Duration::from_secs(2));
        assert!((rate - 60.0).abs() < 0.001, "expected 60, got {rate}");
        assert_eq!(sample, (220, t0 + Duration::from_secs(2)));
    }

    #[test]
    fn fps_rate_restarts_when_the_counter_restarts() {
        // `attach_surface` can rebuild the engine, so the next sample may
        // be smaller than the previous one.
        let t0 = Instant::now();
        let (rate, sample) = fps_rate(Some((600, t0)), 12, t0 + Duration::from_secs(2));
        assert_eq!(rate, 0.0, "a negative delta must never become a rate");
        assert_eq!(sample, (12, t0 + Duration::from_secs(2)));
    }

    #[test]
    fn fps_rate_keeps_the_anchor_when_the_window_is_too_short() {
        let t0 = Instant::now();
        let (rate, sample) = fps_rate(Some((100, t0)), 110, t0 + Duration::from_millis(200));
        assert_eq!(rate, 0.0, "a 200 ms window is too short to report");
        assert_eq!(
            sample,
            (100, t0),
            "the anchor must survive so the window grows instead of restarting"
        );
        // The next call divides over the full window from the original
        // anchor: 60 frames in 2 s = 30 fps. Had the anchor restarted at
        // the short call it would read 50 in 1.8 s ≈ 27.8 fps.
        let (rate, _) = fps_rate(Some(sample), 160, t0 + Duration::from_secs(2));
        assert!((rate - 30.0).abs() < 0.001, "expected 30, got {rate}");
    }
}
