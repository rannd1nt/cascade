//! Direct3D 11 presentation to the host windows.
//!
//! Windows 11 24H2's "raised desktop" (`Progman` is
//! `WS_EX_NOREDIRECTIONBITMAP`, `SHELLDLL_DefView` is a layered child) only
//! composites the *presented* surface of a layered child. GDI painting into
//! it — `WM_PAINT`, `FillRect`, even a plain `DefWindowProc` class brush —
//! never reaches the screen, which is why slice 1 was invisible on build
//! 26200. A DXGI swap chain on the host HWND is the supported path and it is
//! the same path the video backend needs later.
//!
//! This module is intentionally small: create a device, a swap chain for the
//! window, and clear it. Decoding and drawing are separate concerns.

use windows::core::{Error as WinError, Interface};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11DepthStencilView, ID3D11Device, ID3D11DeviceContext,
    ID3D11RenderTargetView, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGIOutput, IDXGISwapChain1, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT, DXGI_SWAP_EFFECT_DISCARD,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

/// Which swap-chain flavour the machine accepted. Logged once, useful when
/// comparing builds and GPUs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapEffect {
    FlipDiscard,
    FlipSequential,
    Discard,
}

impl SwapEffect {
    pub fn label(self) -> &'static str {
        match self {
            Self::FlipDiscard => "flip_discard",
            Self::FlipSequential => "flip_sequential",
            Self::Discard => "discard",
        }
    }

    fn raw(self) -> (DXGI_SWAP_EFFECT, u32) {
        match self {
            Self::FlipDiscard => (DXGI_SWAP_EFFECT_FLIP_DISCARD, 2),
            Self::FlipSequential => (DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, 2),
            Self::Discard => (DXGI_SWAP_EFFECT_DISCARD, 1),
        }
    }
}

#[derive(Debug)]
pub enum PresenterError {
    Device(String),
    Factory(String),
    SwapChain(String),
    BackBuffer(String),
}

impl std::fmt::Display for PresenterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Device(msg) => write!(f, "D3D11CreateDevice failed: {msg}"),
            Self::Factory(msg) => write!(f, "could not reach the DXGI factory: {msg}"),
            Self::SwapChain(msg) => write!(f, "could not create a swap chain: {msg}"),
            Self::BackBuffer(msg) => write!(f, "could not bind the back buffer: {msg}"),
        }
    }
}

impl std::error::Error for PresenterError {}

/// One D3D11 device and one swap chain bound to one host window.
///
/// The device is private to the window for now. will hoist it into a shared
/// context once several windows and the decoder have to agree on one device.
pub struct Presenter {
    #[allow(dead_code)]
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain1,
    target: ID3D11RenderTargetView,
    size: (u32, u32),
    effect: SwapEffect,
}

impl Presenter {
    pub fn new(hwnd: isize, width: u32, height: u32) -> Result<Self, PresenterError> {
        let width = width.max(1);
        let height = height.max(1);
        // SAFETY: every COM object is checked before use; all out-pointers
        // point at locals that outlive the call.
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let levels = [D3D_FEATURE_LEVEL_11_0];
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|err| PresenterError::Device(err.message()))?;

            let device = device.ok_or_else(|| PresenterError::Device("no device".into()))?;
            let context = context.ok_or_else(|| PresenterError::Device("no context".into()))?;

            let dxgi_device: IDXGIDevice = device
                .cast()
                .map_err(|err: WinError| PresenterError::Factory(err.message()))?;
            let adapter = dxgi_device
                .GetAdapter()
                .map_err(|err| PresenterError::Factory(err.message()))?;
            let factory: IDXGIFactory2 = adapter
                .GetParent()
                .map_err(|err: WinError| PresenterError::Factory(err.message()))?;

            let (swapchain, effect) = create_swapchain(&factory, &device, hwnd, width, height)?;

            let back_buffer: ID3D11Texture2D = swapchain.GetBuffer(0).map_err(|err| {
                PresenterError::BackBuffer(format!("IDXGISwapChain::GetBuffer: {}", err.message()))
            })?;
            let mut target: Option<ID3D11RenderTargetView> = None;
            device
                .CreateRenderTargetView(&back_buffer, None, Some(&mut target))
                .map_err(|err| {
                    PresenterError::BackBuffer(format!(
                        "ID3D11Device::CreateRenderTargetView: {}",
                        err.message()
                    ))
                })?;
            let target =
                target.ok_or_else(|| PresenterError::BackBuffer("no render target".into()))?;

            Ok(Self {
                device,
                context,
                swapchain,
                target,
                size: (width, height),
                effect,
            })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn effect(&self) -> SwapEffect {
        self.effect
    }

    /// Clear the whole surface to `rgba` and present it.
    ///
    /// The values are sRGB components in `0.0..=1.0`; DXGI does no conversion
    /// for a `B8G8R8A8_UNORM` target, so a solid colour round-trips exactly.
    pub fn clear(&self, rgba: [f32; 4]) {
        // SAFETY: all handles belong to this presenter and are alive; the
        // render target is bound to the same size as the swap chain.
        unsafe {
            self.context.OMSetRenderTargets(
                Some(&[Some(self.target.clone())]),
                None::<&ID3D11DepthStencilView>,
            );
            self.context.ClearRenderTargetView(&self.target, &rgba);
            let _ = self.swapchain.Present(1, DXGI_PRESENT(0));
        }
    }
}

fn create_swapchain(
    factory: &IDXGIFactory2,
    device: &ID3D11Device,
    hwnd: isize,
    width: u32,
    height: u32,
) -> Result<(IDXGISwapChain1, SwapEffect), PresenterError> {
    const ATTEMPTS: [SwapEffect; 3] = [
        SwapEffect::FlipDiscard,
        SwapEffect::FlipSequential,
        SwapEffect::Discard,
    ];
    let mut last = String::new();
    for effect in ATTEMPTS {
        let (raw_effect, buffers) = effect.raw();
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: buffers,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: raw_effect,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        // SAFETY: the window handle comes from the caller and the descriptor
        // is a local that lives for the call.
        let attempt = unsafe {
            factory.CreateSwapChainForHwnd(
                device,
                HWND(hwnd as *mut core::ffi::c_void),
                &desc,
                None,
                None::<&IDXGIOutput>,
            )
        };
        match attempt {
            Ok(swapchain) => return Ok((swapchain, effect)),
            Err(err) => last = format!("{}: {}", effect.label(), err.message()),
        }
    }
    Err(PresenterError::SwapChain(last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_effect_labels_are_stable() {
        // These strings appear in the daemon log (`present=dxgi/...`).
        assert_eq!(SwapEffect::FlipDiscard.label(), "flip_discard");
        assert_eq!(SwapEffect::FlipSequential.label(), "flip_sequential");
        assert_eq!(SwapEffect::Discard.label(), "discard");
    }

    #[test]
    fn swap_effect_buffer_counts_match_contract() {
        // Flip models need a back buffer of 2; the legacy discard path uses 1.
        assert_eq!(SwapEffect::FlipDiscard.raw().1, 2);
        assert_eq!(SwapEffect::FlipSequential.raw().1, 2);
        assert_eq!(SwapEffect::Discard.raw().1, 1);
    }
}
