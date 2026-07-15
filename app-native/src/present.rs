//! Raw D3D11 FLIP_DISCARD present — A0 slice 3 (design D5,
//! docs/design/unified-app-architecture.md §4-2).
//!
//! A dedicated stream child HWND (design D9) carries an
//! `IDXGISwapChain2` created with `FLIP_DISCARD |
//! FRAME_LATENCY_WAITABLE_OBJECT`, `SetMaximumFrameLatency(1)` on the
//! *swapchain* (never the device — moonlight-qt d3d11va.cpp:550 trap),
//! and the render loop waits on the latency waitable before each draw.
//!
//! A0 scope: the decoder's CPU RGBA output is uploaded to a DEFAULT
//! texture and copied to the backbuffer; the swapchain buffers match the
//! video dimensions and `DXGI_SCALING_STRETCH` maps them onto the child
//! HWND, whose rect the UI thread keeps aspect-fit. Zero-copy NV12
//! (decoder D3D11 texture straight into the swapchain) and
//! ALLOW_TEARING/vsync-off land with Phase B (§2 topology).

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC,
    D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAP_WRITE_DISCARD, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT, D3D11CreateDevice, ID3D11Buffer,
    ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2, IDXGISwapChain2,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWL_STYLE, GWLP_USERDATA, GetCursorPos,
    GetWindowLongPtrW, GetWindowRect, HTTRANSPARENT, MoveWindow, RegisterClassW, SetWindowLongPtrW,
    WINDOW_EX_STYLE, WM_ERASEBKGND, WM_NCHITTEST, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN,
    WS_CLIPSIBLINGS, WS_VISIBLE,
};
use windows::core::{Interface, PCSTR, PCWSTR, s, w};

use crate::VideoShared;
use crate::video::RgbaFrame;

/// Present failure. Fatal to the raw surface only: the caller latches
/// the egui-texture fallback and the session keeps running.
#[derive(Debug)]
pub struct PresentError(pub String);

impl std::fmt::Display for PresentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<windows::core::Error> for PresentError {
    fn from(e: windows::core::Error) -> Self {
        Self(e.to_string())
    }
}

// ── Stream child window ────────────────────────────────────────────────────

const CLASS_NAME: PCWSTR = w!("BetterParsecStreamSurface");

/// Hit-test transparent so egui (parent) keeps receiving mouse events in
/// the stream area until input is enabled (A2 attaches an
/// [`crate::input::InputCtx`] via GWLP_USERDATA, after which the window
/// consumes mouse/keyboard and forwards them onto the wire). No
/// background erase: the swapchain owns every pixel.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ctx = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const crate::input::InputCtx;
    if !ctx.is_null()
        && let Some(r) = crate::input::handle(unsafe { &*ctx }, hwnd, msg, wp, lp)
    {
        return r;
    }
    match msg {
        WM_NCHITTEST if ctx.is_null() => LRESULT(HTTRANSPARENT as isize),
        WM_ERASEBKGND => LRESULT(1),
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn register_class() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        // 0 == failure, but the only plausible cause after Once is a
        // ClassAlreadyExists race, which CreateWindowExW surfaces anyway.
        RegisterClassW(&wc);
    });
}

fn create_stream_child(parent: HWND) -> Result<HWND, PresentError> {
    register_class();
    unsafe {
        // The parent (winit/eframe) does not clip children; without this
        // the GL chrome swap can flicker over the stream area (design §7
        // risk 3).
        let style = GetWindowLongPtrW(parent, GWL_STYLE);
        SetWindowLongPtrW(parent, GWL_STYLE, style | WS_CLIPCHILDREN.0 as isize);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            None,
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            0,
            0,
            0,
            0,
            Some(parent),
            None,
            None,
            None,
        )
        .map_err(|e| PresentError(format!("CreateWindowExW: {e}")))
    }
}

// ── Renderer (render-thread owned) ─────────────────────────────────────────

/// Inline HLSL for the opt-in client-side sharpen pass: a full-screen
/// triangle (no vertex/index buffers, driven by `SV_VertexID`) followed
/// by a light CAS-style/unsharp kernel — center + 4 axial neighbors,
/// pushed away from their average by `strength`. Single pass, no
/// external dependencies; compiled at runtime via `D3DCompile`.
const SHARPEN_HLSL: &str = r#"
cbuffer SharpenCB : register(b0) {
    float2 invResolution;
    float strength;
    float _pad;
};

Texture2D srcTex : register(t0);
SamplerState samp : register(s0);

struct VSOut {
    float4 pos : SV_POSITION;
    float2 uv : TEXCOORD0;
};

VSOut VSMain(uint id : SV_VertexID) {
    VSOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.uv = uv;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

float4 PSMain(VSOut i) : SV_TARGET {
    float4 center = srcTex.Sample(samp, i.uv);
    float4 up = srcTex.Sample(samp, i.uv - float2(0.0, invResolution.y));
    float4 down = srcTex.Sample(samp, i.uv + float2(0.0, invResolution.y));
    float4 left = srcTex.Sample(samp, i.uv - float2(invResolution.x, 0.0));
    float4 right = srcTex.Sample(samp, i.uv + float2(invResolution.x, 0.0));
    float4 sharp = center + strength * (center * 4.0 - up - down - left - right) * 0.25;
    return saturate(sharp);
}
"#;

/// Matches the `SharpenCB` HLSL cbuffer layout (16-byte aligned).
#[repr(C)]
struct SharpenCbData {
    inv_resolution: [f32; 2],
    strength: f32,
    _pad: f32,
}

/// `BP_SHARPEN`: integer percent 0..100 → strength 0.0..1.0. Absent or
/// unparsable → 0.0 (off, keeps the default bit-exact copy path).
fn sharpen_from_env() -> f32 {
    std::env::var("BP_SHARPEN")
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .map(|v| v.clamp(0, 100) as f32 / 100.0)
        .unwrap_or(0.0)
}

/// Compile an HLSL source string via `D3DCompile`, returning shader
/// bytecode ready for `CreateVertexShader`/`CreatePixelShader`.
fn compile_hlsl(src: &str, entry: PCSTR, target: PCSTR) -> Result<Vec<u8>, PresentError> {
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        let result = D3DCompile(
            src.as_ptr().cast(),
            src.len(),
            None,
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut blob,
            Some(&mut errors),
        );
        if let Err(e) = result {
            let detail = errors
                .map(|b| {
                    let ptr = b.GetBufferPointer().cast::<u8>();
                    let len = b.GetBufferSize();
                    String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned()
                })
                .unwrap_or_default();
            return Err(PresentError(format!("D3DCompile: {e}: {detail}")));
        }
        let blob = blob.ok_or_else(|| PresentError("D3DCompile produced no blob".into()))?;
        let ptr = blob.GetBufferPointer().cast::<u8>();
        let len = blob.GetBufferSize();
        Ok(std::slice::from_raw_parts(ptr, len).to_vec())
    }
}

/// D3D11 device + FLIP_DISCARD swapchain bound to the stream child HWND.
/// Split from the thread loop so tests can drive draw/present/readback
/// synchronously.
struct Renderer {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    swapchain: IDXGISwapChain2,
    waitable: HANDLE,
    /// DEFAULT-usage upload target for the decoded RGBA rows, matching
    /// the swapchain buffer dimensions.
    upload: Option<ID3D11Texture2D>,
    /// Shader resource view onto [`Self::upload`]; recreated whenever the
    /// upload texture is (re)created (resolution change), never reused
    /// across textures.
    upload_srv: Option<ID3D11ShaderResourceView>,
    /// Opt-in client-side sharpen strength in `[0.0, 1.0]`; `0.0` (the
    /// default) keeps the exact bit-copy present path. Set from
    /// `BP_SHARPEN` (integer percent, 0..100) or forced by tests.
    sharpen: f32,
    /// Full-screen-triangle sharpen pass resources, compiled and created
    /// lazily on first use (device-level, independent of resolution).
    sharpen_vs: Option<ID3D11VertexShader>,
    sharpen_ps: Option<ID3D11PixelShader>,
    sharpen_sampler: Option<ID3D11SamplerState>,
    /// `SharpenCB` constant buffer (inverse resolution + strength).
    sharpen_cbuf: Option<ID3D11Buffer>,
    width: u32,
    height: u32,
}

// SAFETY: created on and then exclusively owned by the render thread (or
// a test); D3D11 immediate contexts are single-thread affine but not
// creation-thread affine.
unsafe impl Send for Renderer {}

impl Renderer {
    fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self, PresentError> {
        unsafe {
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .map_err(|e| PresentError(format!("D3D11CreateDevice: {e}")))?;
            let device = device.ok_or_else(|| PresentError("no D3D11 device".into()))?;
            let ctx = ctx.ok_or_else(|| PresentError("no immediate context".into()))?;

            let dxgi_dev: IDXGIDevice = device.cast()?;
            let factory: IDXGIFactory2 = dxgi_dev.GetAdapter()?.GetParent()?;

            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                // Buffers stay at video dimensions; DXGI stretches onto
                // the child HWND, which the UI keeps aspect-fit.
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
                ..Default::default()
            };
            let swapchain: IDXGISwapChain2 = factory
                .CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)
                .map_err(|e| PresentError(format!("CreateSwapChainForHwnd: {e}")))?
                .cast()?;
            // On the swapchain, NOT the device (Present would block).
            swapchain.SetMaximumFrameLatency(1)?;
            let waitable = HANDLE(swapchain.GetFrameLatencyWaitableObject().0);
            if waitable.is_invalid() {
                return Err(PresentError("no frame latency waitable".into()));
            }

            Ok(Self {
                device,
                ctx,
                swapchain,
                waitable,
                upload: None,
                upload_srv: None,
                sharpen: sharpen_from_env(),
                sharpen_vs: None,
                sharpen_ps: None,
                sharpen_sampler: None,
                sharpen_cbuf: None,
                width,
                height,
            })
        }
    }

    fn ensure_size(&mut self, width: u32, height: u32) -> Result<(), PresentError> {
        if width == self.width && height == self.height {
            return Ok(());
        }
        self.upload = None; // release before ResizeBuffers
        self.upload_srv = None; // tied to the (now-stale) upload texture
        unsafe {
            self.swapchain
                .ResizeBuffers(
                    0,
                    width,
                    height,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
                )
                .map_err(|e| PresentError(format!("ResizeBuffers: {e}")))?;
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Wait for a latency slot, upload the frame, copy to the backbuffer.
    /// `present()` completes the cycle; split so tests can read the
    /// backbuffer before FLIP rotates it away.
    fn draw(&mut self, frame: &RgbaFrame) -> Result<(), PresentError> {
        let (w, h) = (frame.width as u32, frame.height as u32);
        if w == 0 || h == 0 || frame.rgba.len() != frame.width * frame.height * 4 {
            return Err(PresentError("bad frame dimensions".into()));
        }
        self.ensure_size(w, h)?;
        unsafe {
            // 100ms cap: a lost waitable must never stall the pipe.
            WaitForSingleObjectEx(self.waitable, 100, false);

            if self.upload.is_none() {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    ..Default::default()
                };
                let mut tex = None;
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut tex))
                    .map_err(|e| PresentError(format!("CreateTexture2D: {e}")))?;
                self.upload = tex;
                // The old SRV (if any) pointed at the texture we just
                // replaced — never reuse it across textures.
                self.upload_srv = None;
            }
            // Cloned (COM AddRef, not a copy): the borrow ends here so the
            // sharpen path below can take `&mut self` for lazy resource
            // creation while still using the same underlying texture.
            let upload = self.upload.clone().expect("just created");
            self.ctx
                .UpdateSubresource(&upload, 0, None, frame.rgba.as_ptr().cast(), w * 4, 0);
            let back: ID3D11Texture2D = self.swapchain.GetBuffer(0)?;
            if self.sharpen > 0.0 {
                self.draw_sharpen(&back, &upload, w, h)?;
            } else {
                self.ctx.CopyResource(&back, &upload);
            }
        }
        Ok(())
    }

    /// Lazily compile/create the sharpen pass's device-level resources
    /// (shaders, sampler, constant buffer). Independent of resolution —
    /// created once per `Renderer` and reused across frames/resizes.
    fn ensure_sharpen_resources(&mut self) -> Result<(), PresentError> {
        if self.sharpen_vs.is_some() {
            return Ok(());
        }
        unsafe {
            let vs_bytecode = compile_hlsl(SHARPEN_HLSL, s!("VSMain"), s!("vs_5_0"))?;
            let ps_bytecode = compile_hlsl(SHARPEN_HLSL, s!("PSMain"), s!("ps_5_0"))?;

            let mut vs = None;
            self.device
                .CreateVertexShader(&vs_bytecode, None, Some(&mut vs))
                .map_err(|e| PresentError(format!("CreateVertexShader: {e}")))?;
            let mut ps = None;
            self.device
                .CreatePixelShader(&ps_bytecode, None, Some(&mut ps))
                .map_err(|e| PresentError(format!("CreatePixelShader: {e}")))?;

            let sampler_desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            self.device
                .CreateSamplerState(&sampler_desc, Some(&mut sampler))
                .map_err(|e| PresentError(format!("CreateSamplerState: {e}")))?;

            let cbuf_desc = D3D11_BUFFER_DESC {
                ByteWidth: size_of::<SharpenCbData>() as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut cbuf = None;
            self.device
                .CreateBuffer(&cbuf_desc, None, Some(&mut cbuf))
                .map_err(|e| PresentError(format!("CreateBuffer (SharpenCB): {e}")))?;

            self.sharpen_vs = vs;
            self.sharpen_ps = ps;
            self.sharpen_sampler = sampler;
            self.sharpen_cbuf = cbuf;
        }
        Ok(())
    }

    /// Opt-in sharpen path: draws a full-screen triangle sampling
    /// `upload` through the unsharp kernel straight onto `back`, in
    /// place of the plain `CopyResource`. The default (`sharpen == 0.0`)
    /// path in [`Self::draw`] never calls this — the bit-exact copy is
    /// unchanged.
    fn draw_sharpen(
        &mut self,
        back: &ID3D11Texture2D,
        upload: &ID3D11Texture2D,
        w: u32,
        h: u32,
    ) -> Result<(), PresentError> {
        self.ensure_sharpen_resources()?;
        unsafe {
            if self.upload_srv.is_none() {
                let mut srv = None;
                self.device
                    .CreateShaderResourceView(upload, None, Some(&mut srv))
                    .map_err(|e| PresentError(format!("CreateShaderResourceView: {e}")))?;
                self.upload_srv = srv;
            }
            let srv = self.upload_srv.clone().expect("just created");

            let mut rtv: Option<ID3D11RenderTargetView> = None;
            self.device
                .CreateRenderTargetView(back, None, Some(&mut rtv))
                .map_err(|e| PresentError(format!("CreateRenderTargetView: {e}")))?;
            let rtv = rtv.ok_or_else(|| PresentError("no render target view".into()))?;

            let cbuf = self.sharpen_cbuf.clone().expect("ensured above");
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .map_err(|e| PresentError(format!("Map cbuffer: {e}")))?;
            let data = SharpenCbData {
                inv_resolution: [1.0 / w as f32, 1.0 / h as f32],
                strength: self.sharpen,
                _pad: 0.0,
            };
            std::ptr::copy_nonoverlapping(&data, mapped.pData.cast(), 1);
            self.ctx.Unmap(&cbuf, 0);

            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: w as f32,
                Height: h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            self.ctx.RSSetViewports(Some(&[viewport]));
            self.ctx
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.ctx.VSSetShader(self.sharpen_vs.as_ref(), None);
            self.ctx.PSSetShader(self.sharpen_ps.as_ref(), None);
            self.ctx.PSSetShaderResources(0, Some(&[Some(srv)]));
            self.ctx
                .PSSetSamplers(0, Some(std::slice::from_ref(&self.sharpen_sampler)));
            self.ctx.PSSetConstantBuffers(0, Some(&[Some(cbuf)]));
            self.ctx.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            self.ctx.Draw(3, 0);

            // Unbind the SRV/RTV: the next frame's UpdateSubresource on
            // the same upload texture (and the next FLIP_DISCARD
            // buffer's implicit reuse) must never race a still-bound
            // view — debug-layer hazard otherwise.
            self.ctx.PSSetShaderResources(0, Some(&[None]));
            self.ctx.OMSetRenderTargets(None, None);
        }
        Ok(())
    }

    #[cfg(test)]
    fn set_sharpen_for_test(&mut self, strength: f32) {
        self.sharpen = strength;
    }

    fn present(&mut self) -> Result<(), PresentError> {
        // Present(0): flip-model windowed replaces the queued frame
        // without tearing; ALLOW_TEARING/vsync policy is Phase B.
        unsafe { self.swapchain.Present(0, DXGI_PRESENT(0)) }
            .ok()
            .map_err(|e| PresentError(format!("Present: {e}")))?;
        Ok(())
    }

    /// Copy the current backbuffer to a staging texture and return its
    /// pixels tightly packed (test/readback path).
    #[cfg(test)]
    fn read_backbuffer(&mut self) -> Result<Vec<u8>, PresentError> {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_USAGE_STAGING,
        };
        unsafe {
            let back: ID3D11Texture2D = self.swapchain.GetBuffer(0)?;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: self.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            };
            let mut staging = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .map_err(|e| PresentError(format!("staging CreateTexture2D: {e}")))?;
            let staging = staging.ok_or_else(|| PresentError("no staging texture".into()))?;
            self.ctx.CopyResource(&staging, &back);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| PresentError(format!("Map: {e}")))?;
            let row = self.width as usize * 4;
            let mut out = vec![0u8; row * self.height as usize];
            for y in 0..self.height as usize {
                let src = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                std::ptr::copy_nonoverlapping(src, out[y * row..].as_mut_ptr(), row);
            }
            self.ctx.Unmap(&staging, 0);
            Ok(out)
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.waitable);
        }
    }
}

// ── StreamSurface (UI-thread handle + render thread) ───────────────────────

/// The stream child window plus its render thread. Created on the UI
/// thread once the parent HWND exists; dropped on disconnect (joins the
/// thread, destroys the window — Drop runs on the creating UI thread).
pub struct StreamSurface {
    hwnd: HWND,
    shared: Arc<VideoShared>,
    thread: Option<std::thread::JoinHandle<()>>,
    last_rect: (i32, i32, i32, i32),
    /// An `InputCtx` is attached to the window (owned via USERDATA).
    input: bool,
    /// Immersive mouse capture engaged (RawInput registration + clip).
    capture: bool,
}

impl StreamSurface {
    /// `parent` is the raw Win32 handle of the eframe chrome window.
    pub fn create(parent: isize, shared: Arc<VideoShared>) -> Result<Self, PresentError> {
        let hwnd = create_stream_child(HWND(parent as *mut _))?;
        let thread = {
            let shared = shared.clone();
            let hwnd_val = hwnd.0 as isize;
            std::thread::Builder::new()
                .name("a0-present".into())
                .spawn(move || render_loop(HWND(hwnd_val as *mut _), &shared))
                .map_err(|e| PresentError(format!("spawn present thread: {e}")))?
        };
        Ok(Self {
            hwnd,
            shared,
            thread: Some(thread),
            last_rect: (0, 0, 0, 0),
            input: false,
            capture: false,
        })
    }

    /// Attach input capture: the window stops being hit-test transparent
    /// and its wndproc forwards mouse/keyboard onto the session's input
    /// channels. UI thread only (same thread as the wndproc).
    pub fn enable_input(&mut self, ctx: crate::input::InputCtx) {
        self.disable_input();
        let ptr = Box::into_raw(Box::new(ctx));
        unsafe { SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, ptr as isize) };
        self.input = true;
        // Korean/CJK IME: detach the client-side IME context from the
        // stream child so the local IME never composes (Hangul, kana,
        // pinyin). Raw Win32 VK key-downs then reach `input::translate`
        // and go on the wire, letting the *host* IME compose — exactly
        // what the web client gets by suppressing the browser IME.
        // Without this the client IME swallows letter keys (WM_KEYDOWN
        // arrives as VK_PROCESSKEY 0xE5) and the composed WM_CHAR text is
        // dropped, so no Korean ever reaches the host. Moonlight/Parsec
        // do the same (detach/disable IME for the stream window).
        unsafe {
            let _ = windows::Win32::UI::Input::Ime::ImmAssociateContext(
                self.hwnd,
                windows::Win32::UI::Input::Ime::HIMC::default(),
            );
        }
    }

    fn disable_input(&mut self) {
        if !self.input {
            return;
        }
        let ptr = unsafe { SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0) };
        if ptr != 0 {
            drop(unsafe { Box::from_raw(ptr as *mut crate::input::InputCtx) });
        }
        self.input = false;
    }

    /// Position the child window (client-area pixel coordinates).
    pub fn set_rect(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let rect = (x, y, w.max(0), h.max(0));
        if rect == self.last_rect {
            return;
        }
        self.last_rect = rect;
        unsafe {
            let _ = MoveWindow(self.hwnd, rect.0, rect.1, rect.2, rect.3, true);
        }
    }

    /// Whether the global cursor is inside the stream child window. The
    /// shell reports `CursorIcon::None` to egui while true: winit
    /// re-applies its own cursor every frame while the chrome window has
    /// focus, which fights the child's WM_SETCURSOR hide — double cursor
    /// until the first click moves focus (field report 2026-07-15).
    pub fn cursor_over(&self) -> bool {
        let mut pt = POINT::default();
        let mut rect = RECT::default();
        unsafe {
            if GetCursorPos(&mut pt).is_err() || GetWindowRect(self.hwnd, &mut rect).is_err() {
                return false;
            }
        }
        pt.x >= rect.left && pt.x < rect.right && pt.y >= rect.top && pt.y < rect.bottom
    }

    /// The raw path is gone (init failed); the UI should fall back to the
    /// interim egui texture present.
    pub fn failed(&self) -> bool {
        self.shared.raw_present_failed.load(Ordering::Acquire)
    }

    /// Engage immersive mouse capture (M4 Phase B): register the raw
    /// mouse for WM_INPUT on the child, focus it, and install the
    /// Keyboard Lock hook (Phase B2, `input.rs` — Win keys / Alt+Tab /
    /// Ctrl+Alt+Shift+Q escape hatch; least-invasive signature change:
    /// callers now pass the capture flag and input sender the hook
    /// needs, since a `HOOKPROC` gets no user context of its own). The
    /// cursor clip is re-asserted per frame via
    /// [`Self::clip_cursor_to_self`].
    pub fn engage_mouse_capture(
        &mut self,
        capture: Arc<crate::input::CaptureShared>,
        sender: client_transport::session::InputSender,
    ) {
        if self.capture {
            return;
        }
        use windows::Win32::UI::Input::{
            RAWINPUTDEVICE, RAWINPUTDEVICE_FLAGS, RegisterRawInputDevices,
        };
        let rid = RAWINPUTDEVICE {
            usUsagePage: HID_PAGE_GENERIC,
            usUsage: HID_USAGE_MOUSE,
            dwFlags: RAWINPUTDEVICE_FLAGS(0),
            hwndTarget: self.hwnd,
        };
        unsafe {
            if RegisterRawInputDevices(&[rid], size_of::<RAWINPUTDEVICE>() as u32).is_err() {
                tracing::warn!("RegisterRawInputDevices failed — relative input unavailable");
            }
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(self.hwnd));
        }
        crate::input::install_keyboard_hook(capture, sender);
        self.capture = true;
    }

    /// Release immersive mouse capture (idempotent).
    pub fn release_mouse_capture(&mut self) {
        if !self.capture {
            return;
        }
        self.capture = false;
        release_mouse_capture_global();
    }

    /// Re-assert the cursor clip onto the child window rect — called
    /// every frame while immersive, so moves/resizes need no event
    /// plumbing.
    pub fn clip_cursor_to_self(&self) {
        use windows::Win32::UI::WindowsAndMessaging::ClipCursor;
        let mut rect = RECT::default();
        unsafe {
            if GetWindowRect(self.hwnd, &mut rect).is_ok() {
                let _ = ClipCursor(Some(&rect));
            }
        }
    }
    /// Undo [`Self::clip_cursor_to_self`] (Phase B2 host-authority
    /// auto-switch, `immersive::wants_relative_capture`): unclips the
    /// cursor so it can move freely across the whole desktop again while
    /// the host shows its own cursor. Idempotent and safe to call
    /// whenever relative capture is not wanted, even if never clipped.
    pub fn release_cursor_clip(&self) {
        use windows::Win32::UI::WindowsAndMessaging::ClipCursor;
        unsafe {
            let _ = ClipCursor(None);
        }
    }
}

const HID_PAGE_GENERIC: u16 = 0x01;
const HID_USAGE_MOUSE: u16 = 0x02;

/// Global immersive-capture teardown: deregister the raw mouse, unclip
/// the cursor, and uninstall the Keyboard Lock hook (Phase B2,
/// `input.rs`). Idempotent, and safe without a live surface (App reset
/// paths run it after the surface is already gone) — every release path
/// (`StreamSurface::release_mouse_capture`, its `Drop`, and the
/// session-teardown paths in `main.rs`) converges here, so the hook can
/// never leak installed.
pub fn release_mouse_capture_global() {
    use windows::Win32::UI::Input::{RAWINPUTDEVICE, RIDEV_REMOVE, RegisterRawInputDevices};
    use windows::Win32::UI::WindowsAndMessaging::ClipCursor;
    let rid = RAWINPUTDEVICE {
        usUsagePage: HID_PAGE_GENERIC,
        usUsage: HID_USAGE_MOUSE,
        dwFlags: RIDEV_REMOVE,
        hwndTarget: HWND::default(),
    };
    unsafe {
        let _ = RegisterRawInputDevices(&[rid], size_of::<RAWINPUTDEVICE>() as u32);
        let _ = ClipCursor(None);
    }
    crate::input::uninstall_keyboard_hook();
}

impl Drop for StreamSurface {
    fn drop(&mut self) {
        // Same thread as the wndproc — no message can race the teardown.
        self.release_mouse_capture();
        self.disable_input();
        self.shared.present_stop.store(true, Ordering::Release);
        self.shared.frame_ready.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Render thread: block on the frame condvar, then waitable → upload →
/// copy → present. Newest-wins; a decode burst never queues presents.
fn render_loop(hwnd: HWND, shared: &Arc<VideoShared>) {
    let mut renderer: Option<Renderer> = None;
    loop {
        let frame = {
            let mut guard = shared.frame.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if shared.present_stop.load(Ordering::Acquire) {
                    return;
                }
                if let Some(f) = guard.take() {
                    break f;
                }
                let (g, _) = shared
                    .frame_ready
                    .wait_timeout(guard, std::time::Duration::from_millis(250))
                    .unwrap_or_else(PoisonError::into_inner);
                guard = g;
            }
        };

        if renderer.is_none() {
            match Renderer::new(hwnd, frame.width as u32, frame.height as u32) {
                Ok(r) => {
                    tracing::info!(
                        w = frame.width,
                        h = frame.height,
                        "raw D3D11 FLIP_DISCARD surface up"
                    );
                    renderer = Some(r);
                }
                Err(e) => {
                    tracing::error!(err = %e, "raw present init failed — egui fallback");
                    shared.raw_present_failed.store(true, Ordering::Release);
                    // Put the frame back for the fallback consumer.
                    *shared.frame.lock().unwrap_or_else(PoisonError::into_inner) = Some(frame);
                    return;
                }
            }
        }
        let r = renderer.as_mut().expect("initialized above");
        // Live sharpen strength from the shell UI (0..100 percent).
        r.sharpen = (shared.sharpen_pct.load(Ordering::Relaxed).min(100) as f32) / 100.0;
        if let Err(e) = r.draw(&frame).and_then(|()| r.present()) {
            // Device removed/reset etc.: retry a fresh device once per
            // frame; latch fallback only if recreation also fails.
            tracing::warn!(err = %e, "present failed — recreating device");
            renderer = None;
            match Renderer::new(hwnd, frame.width as u32, frame.height as u32) {
                Ok(mut r) => {
                    if r.draw(&frame).and_then(|()| r.present()).is_ok() {
                        renderer = Some(r);
                    }
                }
                Err(e2) => {
                    tracing::error!(err = %e2, "device recreation failed — egui fallback");
                    shared.raw_present_failed.store(true, Ordering::Release);
                    return;
                }
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{CW_USEDEFAULT, WS_OVERLAPPEDWINDOW};

    /// Hidden top-level window standing in for the eframe chrome.
    fn hidden_parent() -> HWND {
        register_class();
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS_NAME,
                None,
                WS_OVERLAPPEDWINDOW, // not WS_VISIBLE
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                640,
                480,
                None,
                None,
                None,
                None,
            )
            .expect("parent window")
        }
    }

    fn gradient(width: usize, height: usize) -> RgbaFrame {
        let mut rgba = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) * 4;
                rgba[i] = (x * 255 / width.max(1)) as u8;
                rgba[i + 1] = (y * 255 / height.max(1)) as u8;
                rgba[i + 2] = ((x + y) % 251) as u8;
                rgba[i + 3] = 255;
            }
        }
        RgbaFrame {
            width,
            height,
            rgba,
        }
    }

    /// draw → backbuffer readback must reproduce the uploaded pixels
    /// exactly, across a mid-stream resolution change (ResizeBuffers
    /// path), and Present must succeed on the hidden child window.
    #[test]
    fn swapchain_roundtrip_and_resize() {
        let parent = hidden_parent();
        let child = create_stream_child(parent).expect("child window");
        unsafe {
            let _ = MoveWindow(child, 0, 0, 320, 240, false);
        }

        let f1 = gradient(64, 48);
        let mut r = match Renderer::new(child, 64, 48) {
            Ok(r) => r,
            Err(e) => {
                // No hardware D3D11 device (bare CI VM): nothing to test.
                eprintln!("skipping: {e}");
                unsafe {
                    let _ = DestroyWindow(parent);
                }
                return;
            }
        };
        r.draw(&f1).expect("draw f1");
        assert_eq!(r.read_backbuffer().expect("readback f1"), f1.rgba);
        r.present().expect("present f1");

        // Resolution change: ResizeBuffers + fresh upload texture.
        let f2 = gradient(128, 72);
        r.draw(&f2).expect("draw f2");
        assert_eq!(r.read_backbuffer().expect("readback f2"), f2.rgba);
        r.present().expect("present f2");
        assert_eq!((r.width, r.height), (128, 72));

        drop(r);
        unsafe {
            let _ = DestroyWindow(parent); // destroys the child too
        }
    }

    /// A hard vertical edge, one flat shade per half — headroom on both
    /// sides (not 0/255) so the sharpen halo is visible rather than
    /// floor/ceiling-clamped away.
    fn edge_frame(width: usize, height: usize) -> RgbaFrame {
        let mut rgba = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) * 4;
                let v: u8 = if x < width / 2 { 64 } else { 192 };
                rgba[i] = v;
                rgba[i + 1] = v;
                rgba[i + 2] = v;
                rgba[i + 3] = 255;
            }
        }
        RgbaFrame {
            width,
            height,
            rgba,
        }
    }

    /// Sharpen-on draw must leave flat interior regions unchanged (within
    /// rounding) and must steepen the hard edge — darker just before it,
    /// brighter just after — versus the plain-copy result the default
    /// (sharpen-off) path would have produced.
    #[test]
    fn sharpen_pass_sharpens_edge_and_preserves_flat_regions() {
        let parent = hidden_parent();
        let child = create_stream_child(parent).expect("child window");
        unsafe {
            let _ = MoveWindow(child, 0, 0, 320, 240, false);
        }

        let (width, height) = (64usize, 32usize);
        let frame = edge_frame(width, height);
        let mut r = match Renderer::new(child, width as u32, height as u32) {
            Ok(r) => r,
            Err(e) => {
                // No hardware D3D11 device (bare CI VM): nothing to test.
                eprintln!("skipping: {e}");
                unsafe {
                    let _ = DestroyWindow(parent);
                }
                return;
            }
        };
        r.set_sharpen_for_test(1.0);
        r.draw(&frame).expect("draw edge frame");
        let out = r.read_backbuffer().expect("readback");
        r.present().expect("present");

        let row = width * 4;
        let px = |buf: &[u8], x: usize, y: usize, c: usize| buf[y * row + x * 4 + c] as i32;

        // Flat interior, both sides of the edge: unchanged within an
        // 8-bit rounding tolerance.
        for &x in &[4usize, 8, 24, 40, 56, 60] {
            for y in 0..height {
                for c in 0..3 {
                    let want = px(&frame.rgba, x, y, c);
                    let got = px(&out, x, y, c);
                    assert!(
                        (want - got).abs() <= 2,
                        "flat region changed at x={x} y={y} c={c}: {want} -> {got}"
                    );
                }
            }
        }

        // Edge-adjacent columns: the unsharp kernel pushes each side away
        // from the local average, away from the plain-copy value.
        let (edge_left, edge_right, mid_row) = (width / 2 - 1, width / 2, height / 2);
        let before_in = px(&frame.rgba, edge_left, mid_row, 0);
        let before_out = px(&out, edge_left, mid_row, 0);
        let after_in = px(&frame.rgba, edge_right, mid_row, 0);
        let after_out = px(&out, edge_right, mid_row, 0);

        assert!(
            before_out < before_in - 5,
            "expected undershoot just before the edge: in={before_in} out={before_out}"
        );
        assert!(
            after_out > after_in + 5,
            "expected overshoot just after the edge: in={after_in} out={after_out}"
        );

        drop(r);
        unsafe {
            let _ = DestroyWindow(parent); // destroys the child too
        }
    }
}
