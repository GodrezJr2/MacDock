//! GPU path for the genie: the snapshot is uploaded once as a texture and every frame only the 48-row strip
//! mesh (98 vertices) is drawn with D3D11 into a composition swap chain shown by DirectComposition. Nothing
//! is copied through the CPU per frame, so a full-screen window costs the same as a small one. Edges are
//! anti-aliased by 4x MSAA, whose resolve gives premultiplied coverage directly (the mesh is opaque).
//! The window has no redirection bitmap: DWM composes our swap chain straight from the GPU.

use std::sync::Arc;

use tiny_skia::Pixmap;
use windows::core::{s, w, Interface, PCSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
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

// Each mesh is the window plus a margin around it (uv runs past 0..1 there). In window pixels the pixel
// shader cuts the rounded corners (antialiased) and draws a soft drop shadow in the margin, so the genie
// starts and ends looking like the real window instead of a square cut-out without a shadow.
const SHADER: &str = r#"
cbuffer C : register(b0) { float2 size; float alpha; float radius; float2 win; float sigma; float shadow; float drop; float3 pad; };
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
float box(float2 p, float2 half_, float r) {
    float2 q = abs(p) - half_ + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}
float4 ps(P p) : SV_TARGET {
    float2 px = p.uv * win;
    float2 half_ = win * 0.5;
    float d = box(px - half_, half_, radius);
    float inside = saturate(0.5 - d / max(fwidth(d), 1e-4));
    float3 c = tex.Sample(smp, saturate(p.uv)).rgb;
    float sh = 0.0;
    if (shadow > 0.0) {
        float ds = box(px - half_ - float2(0.0, drop), half_, radius);
        sh = shadow * (0.5 - 0.5 * tanh(0.8 * ds / sigma)); // a blurred edge, about erfc
    }
    return float4(c * inside, inside + (1.0 - inside) * sh) * alpha;
}
"#;

unsafe extern "system" fn proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(h, msg, w, l),
    }
}

/// How a window is dressed: its size in screen pixels, the margin drawn around it, corner radius, and the
/// shadow (blur sigma, opacity, how far it drops down), all in screen pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub w: f32,
    pub h: f32,
    pub margin: f32,
    pub radius: f32,
    pub sigma: f32,
    pub shadow: f32,
    pub drop: f32,
}

impl Look {
    /// square corners, no shadow
    pub fn flat(w: f64, h: f64) -> Look {
        Look { w: w as f32, h: h as f32, margin: 0.0, radius: 0.0, sigma: 1.0, shadow: 0.0, drop: 0.0 }
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
    blend: ID3D11BlendState,
    size: (u32, u32),
    msaa: Option<(ID3D11Texture2D, ID3D11RenderTargetView)>,
    tex: Vec<(usize, ID3D11ShaderResourceView)>, // keyed by the snapshot's address, newest last
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
        // the process-wide device the glass backdrop uses too: a second one cost ~50 MB and ~50 driver threads
        let (dev, ctx) = crate::gfx::d3d().ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
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
                ByteWidth: 48, Usage: D3D11_USAGE_DYNAMIC, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
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
        // premultiplied "over": windows in front cover those behind, corners and shadows blend
        let mut bd = D3D11_BLEND_DESC::default();
        bd.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(), SrcBlend: D3D11_BLEND_ONE, DestBlend: D3D11_BLEND_INV_SRC_ALPHA, BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE, DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA, BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let mut blend = None;
        dev.CreateBlendState(&bd, Some(&mut blend))?;
        Ok(Gpu {
            hwnd, dev, ctx, swap, _dcomp: dcomp, _target: target, _visual: visual, vs: vs.unwrap(), ps: ps.unwrap(),
            layout: layout.unwrap(), vbuf: vbuf.unwrap(), cbuf: cbuf.unwrap(), sampler: sampler.unwrap(), raster: raster.unwrap(),
            blend: blend.unwrap(),
            size: (8, 8), msaa: None, tex: Vec::new(),
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
        if let Some((_, srv)) = self.tex.iter().find(|(k, _)| *k == key) {
            return Ok(srv.clone());
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
            self.tex.push((key, srv.clone()));
            if self.tex.len() > 8 {
                self.tex.remove(0);
            }
            Ok(srv)
        }
    }

    /// Draw the mesh; `rows` are (left, right, y) in screen pixels, `origin` is the window's top-left.
    /// `alpha` fades the whole mesh (premultiplied), 1.0 while it moves.
    #[cfg(test)]
    pub fn draw(&mut self, img: &Arc<Pixmap>, rows: &[(f64, f64, f64)], origin: (f64, f64), alpha: f32) -> windows::core::Result<()> {
        self.draw_many(&[(img, rows, Look::flat(1.0, 1.0))], origin, alpha)
    }

    /// Draw several meshes in one frame, the first at the back; each is (snapshot, rows, look) with rows as
    /// (left, right, y) in screen pixels around the window grown by `look.margin`; `origin` is the overlay's
    /// top-left. `alpha` fades them all.
    pub fn draw_many(&mut self, meshes: &[(&Arc<Pixmap>, &[(f64, f64, f64)], Look)], origin: (f64, f64), alpha: f32) -> windows::core::Result<()> {
        let Some((msaa, rtv)) = self.msaa.clone() else { return Ok(()) };
        // upload first: a new snapshot's mip generation runs on the same context
        let srvs = meshes.iter().map(|(img, _, _)| self.texture(img)).collect::<windows::core::Result<Vec<_>>>()?;
        unsafe {
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.OMSetBlendState(&self.blend, None, 0xffff_ffff);
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
            self.ctx.PSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            self.ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            for (&(_, rows, k), srv) in meshes.iter().zip(srvs) {
                let n = rows.len() - 1;
                self.ctx.Map(&self.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
                *(m.pData as *mut [f32; 12]) = [
                    self.size.0 as f32, self.size.1 as f32, alpha.clamp(0.0, 1.0), k.radius,
                    k.w, k.h, k.sigma.max(0.01), k.shadow, k.drop, 0.0, 0.0, 0.0,
                ];
                self.ctx.Unmap(&self.cbuf, 0);
                // uv past 0..1 over the margin
                let (mu, mv) = (k.margin / k.w.max(1.0), k.margin / k.h.max(1.0));
                self.ctx.Map(&self.vbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
                let v = std::slice::from_raw_parts_mut(m.pData as *mut [f32; 4], rows.len() * 2);
                for (i, &(l, r, y)) in rows.iter().enumerate() {
                    let (yy, tv) = ((y - origin.1) as f32, -mv + (1.0 + 2.0 * mv) * i as f32 / n as f32);
                    v[i * 2] = [(l - origin.0) as f32, yy, -mu, tv];
                    v[i * 2 + 1] = [(r - origin.0) as f32, yy, 1.0 + mu, tv];
                }
                self.ctx.Unmap(&self.vbuf, 0);
                self.ctx.PSSetShaderResources(0, Some(&[Some(srv)]));
                self.ctx.Draw((rows.len() * 2) as u32, 0);
            }
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            self.ctx.ResolveSubresource(&back, 0, &msaa, 0, FMT);
            self.swap.Present(0, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }

    /// Drop the snapshot texture (it can be large) once an animation is over.
    pub fn release_texture(&mut self) {
        self.tex.clear();
    }

    /// Shrink the swap chain and drop the MSAA target once an animation is over. Both are sized to the
    /// animated window (a maximized one costs ~100 MB on an iGPU, where video memory is system RAM), and
    /// rebuilding them on the next animation takes a few milliseconds.
    pub fn release_buffers(&mut self) {
        unsafe {
            self.ctx.OMSetRenderTargets(None, None);
            self.msaa = None;
            self.ctx.ClearState();
            self.ctx.Flush();
            if self.swap.ResizeBuffers(0, 1, 1, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0)).is_ok() {
                self.size = (1, 1);
            }
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
