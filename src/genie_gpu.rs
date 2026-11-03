//! GPU path for the genie: the snapshot is uploaded once as a texture and every frame only the 48-row strip
//! mesh (98 vertices) is drawn with D3D11 into a composition swap chain shown by DirectComposition. Nothing
//! is copied through the CPU per frame, so a full-screen window costs the same as a small one. Edges are
//! anti-aliased by 4x MSAA, whose resolve gives premultiplied coverage directly (the mesh is opaque).
//! The window has no redirection bitmap: DWM composes our swap chain straight from the GPU.

use std::sync::Arc;

use tiny_skia::Pixmap;
use windows::core::{s, w, Interface, PCSTR};
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

const FMT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;
const SAMPLES: u32 = 4;

const SHADER: &str = r#"
cbuffer C : register(b0) { float2 size; float2 pad; };
struct V { float2 pos : POSITION; float2 uv : TEXCOORD; };
struct P { float4 pos : SV_POSITION; float2 uv : TEXCOORD; };
P vs(V v) {
    P o;
    o.pos = float4(v.pos.x / size.x * 2.0 - 1.0, 1.0 - v.pos.y / size.y * 2.0, 0.0, 1.0);
    o.uv = v.uv;
    return o;
}
Texture2D tex : register(t0);
SamplerState smp : register(s0);
float4 ps(P p) : SV_TARGET { return float4(tex.Sample(smp, p.uv).rgb, 1.0); }
"#;

unsafe extern "system" fn proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(h, msg, w, l),
    }
}

pub struct Gpu {
    pub hwnd: HWND,
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
    swap: IDXGISwapChain1,
    _dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    vbuf: ID3D11Buffer,
    cbuf: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    raster: ID3D11RasterizerState,
    size: (u32, u32),
    msaa: Option<(ID3D11Texture2D, ID3D11RenderTargetView)>,
    tex: Option<(usize, ID3D11ShaderResourceView)>, // keyed by the snapshot's address
}

fn compile(entry: PCSTR, target: PCSTR) -> windows::core::Result<Vec<u8>> {
    unsafe {
        let mut code = None;
        let mut err = None;
        D3DCompile(SHADER.as_ptr() as _, SHADER.len(), None, None, None, entry, target, 0, 0, &mut code, Some(&mut err))?;
        let b = code.unwrap();
        Ok(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec())
    }
}

impl Gpu {
    pub fn new() -> windows::core::Result<Gpu> {
        unsafe {
            let hinst = GetModuleHandleW(None)?;
            let wc = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: hinst.into(), lpszClassName: w!("MacDockGenieGpu"), ..Default::default() };
            RegisterClassW(&wc);
            // layered + transparent = click-through; with no redirection bitmap the layered part costs nothing
            let hwnd = CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                w!("MacDockGenieGpu"), w!("MacDock Genie"), WS_POPUP, 0, 0, 1, 1, HWND::default(), HMENU::default(), hinst, None,
            )?;
            let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 255, LWA_ALPHA);
            match Self::build(hwnd) {
                Ok(g) => Ok(g),
                Err(e) => {
                    let _ = DestroyWindow(hwnd);
                    Err(e)
                }
            }
        }
    }

    unsafe fn build(hwnd: HWND) -> windows::core::Result<Gpu> {
        let mut dev = None;
        let mut ctx = None;
        D3D11CreateDevice(
            None, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1]), D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx),
        )?;
        let (dev, ctx) = (dev.unwrap(), ctx.unwrap());
        let dxgi: IDXGIDevice = dev.cast()?;
        let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: 8, Height: 8, Format: FMT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT, BufferCount: 2, Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED, ..Default::default()
        };
        let swap = factory.CreateSwapChainForComposition(&dev, &desc, None)?;
        let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
        let target = dcomp.CreateTargetForHwnd(hwnd, true)?;
        let visual = dcomp.CreateVisual()?;
        visual.SetContent(&swap)?;
        target.SetRoot(&visual)?;
        dcomp.Commit()?;

        let vsb = compile(s!("vs"), s!("vs_4_0"))?;
        let psb = compile(s!("ps"), s!("ps_4_0"))?;
        let mut vs = None;
        dev.CreateVertexShader(&vsb, None, Some(&mut vs))?;
        let mut ps = None;
        dev.CreatePixelShader(&psb, None, Some(&mut ps))?;
        let elems = [
            D3D11_INPUT_ELEMENT_DESC { SemanticName: s!("POSITION"), Format: DXGI_FORMAT_R32G32_FLOAT, AlignedByteOffset: 0, InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA, ..Default::default() },
            D3D11_INPUT_ELEMENT_DESC { SemanticName: s!("TEXCOORD"), Format: DXGI_FORMAT_R32G32_FLOAT, AlignedByteOffset: 8, InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA, ..Default::default() },
        ];
        let mut layout = None;
        dev.CreateInputLayout(&elems, &vsb, Some(&mut layout))?;
        let mut vbuf = None;
        dev.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: (crate::genie::ROWS as u32 + 1) * 2 * 16, Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32, CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32, ..Default::default()
            },
            None, Some(&mut vbuf),
        )?;
        let mut cbuf = None;
        dev.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: 16, Usage: D3D11_USAGE_DYNAMIC, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32, ..Default::default()
            },
            None, Some(&mut cbuf),
        )?;
        let mut sampler = None;
        dev.CreateSamplerState(
            &D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR, AddressU: D3D11_TEXTURE_ADDRESS_CLAMP, AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP, MaxLOD: f32::MAX, ..Default::default()
            },
            Some(&mut sampler),
        )?;
        let mut raster = None;
        dev.CreateRasterizerState(
            &D3D11_RASTERIZER_DESC { FillMode: D3D11_FILL_SOLID, CullMode: D3D11_CULL_NONE, MultisampleEnable: true.into(), DepthClipEnable: true.into(), ..Default::default() },
            Some(&mut raster),
        )?;
        Ok(Gpu {
            hwnd, dev, ctx, swap, _dcomp: dcomp, _target: target, _visual: visual, vs: vs.unwrap(), ps: ps.unwrap(),
            layout: layout.unwrap(), vbuf: vbuf.unwrap(), cbuf: cbuf.unwrap(), sampler: sampler.unwrap(), raster: raster.unwrap(),
            size: (8, 8), msaa: None, tex: None,
        })
    }

    /// Place the window over `area` (screen pixels) and size the buffers to it.
    pub fn set_area(&mut self, x: i32, y: i32, w: u32, h: u32) -> windows::core::Result<()> {
        unsafe {
            SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, w as i32, h as i32, SWP_NOACTIVATE)?;
            if self.size != (w, h) || self.msaa.is_none() {
                self.ctx.OMSetRenderTargets(None, None);
                self.msaa = None;
                self.swap.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))?;
                let mut t = None;
                self.dev.CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: FMT,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: SAMPLES, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32, ..Default::default()
                    },
                    None, Some(&mut t),
                )?;
                let t = t.unwrap();
                let mut rtv = None;
                self.dev.CreateRenderTargetView(&t, None, Some(&mut rtv))?;
                self.msaa = Some((t, rtv.unwrap()));
                self.size = (w, h);
            }
        }
        Ok(())
    }

    /// Upload the snapshot unless it is the one already on the GPU. Mipmapped, so the shrink into the icon
    /// stays smooth instead of shimmering.
    fn texture(&mut self, img: &Arc<Pixmap>) -> windows::core::Result<ID3D11ShaderResourceView> {
        let key = Arc::as_ptr(img) as usize;
        if let Some((k, srv)) = &self.tex {
            if *k == key {
                return Ok(srv.clone());
            }
        }
        unsafe {
            let (w, h) = (img.width(), img.height());
            let mut t = None;
            self.dev.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: w, Height: h, MipLevels: 0, ArraySize: 1, Format: FMT,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                    MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32, ..Default::default()
                },
                None, Some(&mut t),
            )?;
            let t = t.unwrap();
            self.ctx.UpdateSubresource(&t, 0, None, img.data().as_ptr() as _, w * 4, 0);
            let mut srv = None;
            self.dev.CreateShaderResourceView(&t, None, Some(&mut srv))?;
            let srv = srv.unwrap();
            self.ctx.GenerateMips(&srv);
            self.tex = Some((key, srv.clone()));
            Ok(srv)
        }
    }

    /// Draw the mesh; `rows` are (left, right, y) in screen pixels, `origin` is the window's top-left.
    pub fn draw(&mut self, img: &Arc<Pixmap>, rows: &[(f64, f64, f64)], origin: (f64, f64)) -> windows::core::Result<()> {
        let srv = self.texture(img)?;
        let Some((msaa, rtv)) = self.msaa.clone() else { return Ok(()) };
        let n = rows.len() - 1;
        unsafe {
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&self.vbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
            let v = std::slice::from_raw_parts_mut(m.pData as *mut [f32; 4], rows.len() * 2);
            for (i, &(l, r, y)) in rows.iter().enumerate() {
                let (yy, tv) = ((y - origin.1) as f32, i as f32 / n as f32);
                v[i * 2] = [(l - origin.0) as f32, yy, 0.0, tv];
                v[i * 2 + 1] = [(r - origin.0) as f32, yy, 1.0, tv];
            }
            self.ctx.Unmap(&self.vbuf, 0);
            self.ctx.Map(&self.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
            *(m.pData as *mut [f32; 4]) = [self.size.0 as f32, self.size.1 as f32, 0.0, 0.0];
            self.ctx.Unmap(&self.cbuf, 0);

            self.ctx.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 0.0]);
            self.ctx.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            self.ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: self.size.0 as f32, Height: self.size.1 as f32, MaxDepth: 1.0, ..Default::default()
            }]));
            self.ctx.RSSetState(&self.raster);
            self.ctx.IASetInputLayout(&self.layout);
            self.ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            let (stride, offset) = (16u32, 0u32);
            self.ctx.IASetVertexBuffers(0, 1, Some(&Some(self.vbuf.clone())), Some(&stride), Some(&offset));
            self.ctx.VSSetShader(&self.vs, None);
            self.ctx.VSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            self.ctx.PSSetShader(&self.ps, None);
            self.ctx.PSSetShaderResources(0, Some(&[Some(srv)]));
            self.ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.ctx.Draw((rows.len() * 2) as u32, 0);
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            self.ctx.ResolveSubresource(&back, 0, &msaa, 0, FMT);
            self.swap.Present(0, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }

    /// Drop the snapshot texture (it can be large) once an animation is over.
    pub fn release_texture(&mut self) {
        self.tex = None;
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
