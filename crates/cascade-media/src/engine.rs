//! Media Foundation playback backend (design-m2 §2, gate variants A/B).
//!
//! One `IMFMediaEngine` per daemon, created on the daemon main thread
//! (design-m2 §3.1). `MF_MEDIA_ENGINE_PLAYBACK_HWND` is a *creation*
//! attribute, so the engine object only exists once `attach_surface` knows
//! the host HWND; `load` validates the file and remembers the source until
//! then. Variants A and B share this code — the difference is whether
//! `cascade-win` releases its swap chain first (design-m2 §4).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cascade_core::{
    BackendError, BackendResult, BackendStats, HostSurface, MediaBackend, PlaybackOptions,
};
use windows::core::{implement, Interface, BSTR};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{
    CLSID_MFMediaEngineClassFactory, IMFAttributes, IMFMediaEngine, IMFMediaEngineClassFactory,
    IMFMediaEngineEx, IMFMediaEngineNotify, IMFMediaEngineNotify_Impl, MFCreateAttributes,
    MFShutdown, MFStartup, MFSTARTUP_FULL, MF_MEDIA_ENGINE_CALLBACK, MF_MEDIA_ENGINE_PLAYBACK_HWND,
    MF_MEDIA_ENGINE_READY_HAVE_CURRENT_DATA, MF_MEDIA_ENGINE_STATISTIC,
    MF_MEDIA_ENGINE_STATISTIC_FRAMES_DROPPED, MF_MEDIA_ENGINE_STATISTIC_FRAMES_PER_SECOND,
    MF_MEDIA_ENGINE_STATISTIC_FRAMES_RENDERED, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, PostQuitMessage, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

/// How long `attach_surface` waits for the first decodable frame.
const LOAD_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval inside that wait.
const LOAD_POLL: Duration = Duration::from_millis(20);

/// RAII COM apartment for the daemon main thread (AGENTS §4: apartment per
/// thread, released before thread exit).
struct ComInit {
    /// `false` when COM was already initialized in a different apartment
    /// mode: COM is usable, but this thread must not uninitialize it.
    ours: bool,
}

impl ComInit {
    fn new() -> BackendResult<Self> {
        // SAFETY: null reserved pointer is the documented value; Drop only
        // uninitializes when this call is the one that initialized COM.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
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

/// RAII `MFStartup`/`MFShutdown` pair (design-m2 §3.2).
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

/// The `MediaBackend` implementation on `IMFMediaEngine` (design-m2 §2).
pub struct MediaEngineBackend {
    /// `None` until `attach_surface`: the playback HWND is a creation
    /// attribute (design-m2 §4), so the engine object cannot exist earlier.
    engine: Option<IMFMediaEngine>,
    hwnd: isize,
    source: Option<PathBuf>,
    opts: PlaybackOptions,
    /// Desired play state; applied whenever an engine exists.
    want_playing: bool,
    // Declaration order = drop order: the engine shuts down in `Drop` first,
    // then `_mf` runs MFShutdown, then `_com` runs CoUninitialize
    // (design-m2 §3.3).
    _mf: MfStartup,
    _com: ComInit,
}

impl MediaEngineBackend {
    /// Construct the COM + MF runtime. `MFStartup` runs here, once per
    /// process (design-m2 §3.2).
    pub fn new() -> BackendResult<Self> {
        let com = ComInit::new()?;
        let mf = MfStartup::new()?;
        Ok(Self {
            engine: None,
            hwnd: 0,
            source: None,
            opts: PlaybackOptions::default(),
            want_playing: true,
            _mf: mf,
            _com: com,
        })
    }

    /// Create the engine bound to `hwnd`, muted before any source can make
    /// a sound (design-m2 §5).
    fn create_engine(hwnd: isize) -> BackendResult<IMFMediaEngine> {
        let mut attrs: Option<IMFAttributes> = None;
        // SAFETY: valid out-pointer; size 0 is documented for "no initial
        // attributes".
        unsafe { MFCreateAttributes(&mut attrs, 0) }
            .map_err(|err| at("MFCreateAttributes", err))?;
        let attrs = attrs
            .ok_or_else(|| BackendError::Mf("MFCreateAttributes returned no object".into()))?;
        // SAFETY: the HWND travels as a UINT64 (Microsoft docs:
        // MF_MEDIA_ENGINE_PLAYBACK_HWND).
        unsafe { attrs.SetUINT64(&MF_MEDIA_ENGINE_PLAYBACK_HWND, hwnd as u64) }
            .map_err(|err| at("SetUINT64 PLAYBACK_HWND", err))?;
        // The event sink is mandatory in every mode (MSDN:
        // MF_MEDIA_ENGINE_CALLBACK); the engine AddRefs it for its own use.
        let notify: IMFMediaEngineNotify = EngineNotify.into();
        unsafe { attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify) }
            .map_err(|err| at("SetUnknown CALLBACK", err))?;
        // SAFETY: documented in-proc class factory; null outer interface.
        let factory: IMFMediaEngineClassFactory = unsafe {
            CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)
        }
        .map_err(|err| at("CoCreateInstance class factory", err))?;
        // SAFETY: dwflags 0 is the only documented value for CreateInstance.
        let engine = unsafe { factory.CreateInstance(0, &attrs) }
            .map_err(|err| at("CreateInstance", err))?;
        // Never audible (design-m2 §5): mute before a source exists.
        match unsafe { engine.SetMuted(true) } {
            Ok(()) => Ok(engine),
            Err(err) => {
                // SAFETY: created above; shut down before releasing it.
                let _ = unsafe { engine.Shutdown() };
                Err(at("SetMuted", err))
            }
        }
    }

    /// Load `source` into an existing engine (rare path: `load` after the
    /// surface is already attached).
    fn run_source(
        engine: &IMFMediaEngine,
        source: &Path,
        looping: bool,
        play: bool,
    ) -> BackendResult<()> {
        let url = file_url(source);
        // SAFETY: engine is live; the BSTR outlives the call.
        unsafe { engine.SetSource(&BSTR::from(url.as_str())) }
            .map_err(|err| at("SetSource", err))?;
        // SAFETY: live engine.
        unsafe { engine.Load() }.map_err(|err| at("Load", err))?;
        wait_ready(engine)?;
        // SAFETY: live engine.
        unsafe { engine.SetLoop(looping) }.map_err(|err| at("SetLoop", err))?;
        if play {
            // SAFETY: live engine.
            unsafe { engine.Play() }.map_err(|err| at("Play", err))?;
        }
        Ok(())
    }

    /// (Re)start the remembered source on `hwnd`. Any previous engine is
    /// shut down first: reattach recreates it (design-m2 §4).
    fn start_on(&mut self, hwnd: isize) -> BackendResult<()> {
        if let Some(old) = self.engine.take() {
            // SAFETY: created by us on this thread; released before a
            // replacement is created.
            let _ = unsafe { old.Shutdown() };
        }
        let source = self
            .source
            .clone()
            .ok_or_else(|| BackendError::Mf("attach_surface called before load".into()))?;
        let engine = Self::create_engine(hwnd)?;
        match Self::run_source(&engine, &source, self.opts.looping, self.want_playing) {
            Ok(()) => {
                self.engine = Some(engine);
                self.hwnd = hwnd;
                Ok(())
            }
            Err(err) => {
                // SAFETY: created above; release it on the failure path so
                // MFShutdown never races a live engine.
                let _ = unsafe { engine.Shutdown() };
                Err(err)
            }
        }
    }
}

impl Drop for MediaEngineBackend {
    fn drop(&mut self) {
        if let Some(engine) = self.engine.take() {
            // SAFETY: created by us on this thread; the engine must shut
            // down before MFShutdown (design-m2 §3.3).
            let _ = unsafe { engine.Shutdown() };
        }
        // Fields drop next in declaration order: `_mf` (MFShutdown) before
        // `_com` (CoUninitialize).
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
        if self.engine.is_some() {
            // Reload on an already attached engine.
            let hwnd = self.hwnd;
            return self.start_on(hwnd);
        }
        Ok(())
    }

    fn play(&mut self) -> BackendResult<()> {
        self.want_playing = true;
        if let Some(engine) = &self.engine {
            // SAFETY: live engine on its own thread.
            unsafe { engine.Play() }.map_err(mf_err)?;
        }
        Ok(())
    }

    fn pause(&mut self) -> BackendResult<()> {
        self.want_playing = false;
        if let Some(engine) = &self.engine {
            // SAFETY: live engine on its own thread.
            unsafe { engine.Pause() }.map_err(mf_err)?;
        }
        Ok(())
    }

    fn set_looping(&mut self, on: bool) -> BackendResult<()> {
        self.opts.looping = on;
        if let Some(engine) = &self.engine {
            // SAFETY: live engine on its own thread.
            unsafe { engine.SetLoop(on) }.map_err(mf_err)?;
        }
        Ok(())
    }

    fn stats(&self) -> BackendStats {
        // Hardware decode is not observable through the engine; the probe
        // (MFTEnumEx) is slice 4. M2 measures externally instead.
        let mut stats = BackendStats {
            hardware_decode: false,
            ..BackendStats::default()
        };
        let Some(engine) = &self.engine else {
            return stats;
        };
        // SAFETY: out-pointers are locals that outlive the call; the engine
        // is live.
        unsafe {
            let (mut width, mut height) = (0u32, 0u32);
            if engine
                .GetNativeVideoSize(Some(&mut width), Some(&mut height))
                .is_ok()
            {
                stats.video_size = (width, height);
            }
        }
        // Statistics are best-effort counters for `status` (M4); an engine
        // without them still plays.
        if let Ok(ex) = engine.cast::<IMFMediaEngineEx>() {
            stats.decoded_frames = statistic(&ex, MF_MEDIA_ENGINE_STATISTIC_FRAMES_RENDERED)
                .map(|value| value as u64)
                .unwrap_or(0);
            stats.dropped_frames = statistic(&ex, MF_MEDIA_ENGINE_STATISTIC_FRAMES_DROPPED)
                .map(|value| value as u64)
                .unwrap_or(0);
            stats.fps =
                statistic(&ex, MF_MEDIA_ENGINE_STATISTIC_FRAMES_PER_SECOND).unwrap_or(0.0) as f32;
        }
        stats
    }

    fn attach_surface(&mut self, surface: HostSurface) -> BackendResult<()> {
        // Reattach semantics (design-m2 §4): the HWND is a creation
        // attribute, so every attach recreates the engine and reloads from 0.
        self.start_on(surface.hwnd)
    }

    fn detach_surface(&mut self) -> BackendResult<()> {
        if let Some(engine) = self.engine.take() {
            // SAFETY: created by us; releases the playback window first
            // (design-m2 §3.3). Source and desired state survive so a later
            // `attach_surface` starts over from the file.
            let _ = unsafe { engine.Shutdown() };
        }
        self.hwnd = 0;
        Ok(())
    }

    fn shutdown(self: Box<Self>) {
        // `Drop` performs engine Shutdown, then MFShutdown, then
        // CoUninitialize — in that order (design-m2 §3.3).
        drop(self);
    }
}

fn mf_err(err: windows::core::Error) -> BackendError {
    BackendError::Mf(format!("0x{:08X} {}", err.code().0, err.message()))
}

/// Like `mf_err`, but names the API that failed: MF returns the same error
/// codes from very different calls, and a bare code is not actionable.
fn at(stage: &str, err: windows::core::Error) -> BackendError {
    BackendError::Mf(format!("{stage}: 0x{:08X} {}", err.code().0, err.message()))
}

/// Bounded wait for the first decodable frame. The engine also posts
/// `MF_MEDIA_ENGINE_CALLBACK` events, but readiness is polled through
/// `GetReadyState` instead of consumed from the sink: one code path, and a
/// nested pump keeps the daemon responsive during the wait (design-m2 §3.1
/// guard rail: no main-loop stall).
fn wait_ready(engine: &IMFMediaEngine) -> BackendResult<()> {
    let deadline = Instant::now() + LOAD_TIMEOUT;
    loop {
        // SAFETY: live engine; no out-parameters.
        let ready = unsafe { engine.GetReadyState() };
        if ready as i32 >= MF_MEDIA_ENGINE_READY_HAVE_CURRENT_DATA.0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(BackendError::Mf(timeout_reason(engine, ready)));
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

/// Nested pump so the load wait never starves window messages: the host
/// window and the control window live on this thread.
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
}
