//! HDR outputs. With HDR on, Windows composes the desktop in linear scRGB (1.0 = 80 nits) and
//! draws SDR windows at the "SDR content brightness" white, e.g. 3.0 for 240 nits. An 8-bit
//! duplication of that brightens mid greys and clips light ones to white: the phone shows a
//! washed-out desktop. So an HDR output is duplicated as FP16 and drawn back to sRGB BGRA on the
//! GPU, divided by the SDR white so SDR windows look as they do on the monitor; brighter HDR
//! highlights are clipped (keeping their hue). Everything after that is the SDR path.

use windows::core::{s, Interface, PCSTR};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIOutput6;

fn err(e: impl std::fmt::Display) -> String { e.to_string() }

/// Whether the output shows HDR now (HDR10 signal), not merely whether the monitor could.
pub fn is_hdr(output: &windows::Win32::Graphics::Dxgi::IDXGIOutput) -> bool {
    output.cast::<IDXGIOutput6>().ok().and_then(|o| unsafe { o.GetDesc1() }.ok())
        .is_some_and(|d| d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
}

/// SDR white in scRGB units (nits / 80) of the display with GDI name `gdi` ("\\.\DISPLAY1").
/// Windows' own default (200 nits) when it can't be read.
pub fn sdr_white(gdi: &str) -> f32 {
    sdr_white_level(gdi).unwrap_or_else(|| { crate::log!("{gdi}: SDR white unknown, taking 200 nits"); 2.5 })
}

fn sdr_white_level(gdi: &str) -> Option<f32> {
    let (mut np, mut nm) = (0u32, 0u32);
    unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) }.ok().ok()?;
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    unsafe { QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None) }.ok().ok()?;
    for p in &paths[..np as usize] {
        let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        name.header = DISPLAYCONFIG_DEVICE_INFO_HEADER { r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: std::mem::size_of_val(&name) as u32, adapterId: p.sourceInfo.adapterId, id: p.sourceInfo.id };
        if unsafe { DisplayConfigGetDeviceInfo(&mut name.header) } != 0 { continue; }
        if String::from_utf16_lossy(&name.viewGdiDeviceName).trim_end_matches('\0') != gdi { continue; }
        let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
        white.header = DISPLAYCONFIG_DEVICE_INFO_HEADER { r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
            size: std::mem::size_of_val(&white) as u32, adapterId: p.targetInfo.adapterId, id: p.targetInfo.id };
        if unsafe { DisplayConfigGetDeviceInfo(&mut white.header) } != 0 { return None; }
        // In thousandths of 80 nits.
        return Some(white.SDRWhiteLevel as f32 / 1000.0).filter(|w| *w >= 1.0);
    }
    None
}

const SHADER: &str = r#"
cbuffer C : register(b0) { float white; float3 pad; };
Texture2D<float4> src : register(t0);
float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}
float4 ps(float4 pos : SV_Position) : SV_Target {
    float3 c = max(src.Load(int3(pos.xy, 0)).rgb / white, 0);
    c /= max(1, max(c.r, max(c.g, c.b)));
    float3 lo = c * 12.92, hi = 1.055 * pow(c, 1 / 2.4) - 0.055;
    return float4(c <= 0.0031308 ? lo : hi, 1);
}
"#;

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob, String> {
    let (mut code, mut errors) = (None, None);
    let r = unsafe { D3DCompile(SHADER.as_ptr() as *const _, SHADER.len(), None, None, None, entry, target, 0, 0, &mut code, Some(&mut errors)) };
    if let Err(e) = r {
        let msg = errors.map(|b: ID3DBlob| unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).into_owned() });
        return Err(format!("shader: {e} {}", msg.unwrap_or_default()));
    }
    code.ok_or_else(|| "shader: no code".into())
}

fn bytes(b: &ID3DBlob) -> &[u8] { unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) } }

/// Draws FP16 scRGB frames into an sRGB BGRA texture of the same size.
pub struct ToSdr {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    constants: ID3D11Buffer,
    out: Option<(ID3D11Texture2D, ID3D11RenderTargetView, u32, u32)>,
    /// The SDR white the constants hold.
    white: f32,
}

impl ToSdr {
    pub fn new(device: &ID3D11Device) -> Result<ToSdr, String> {
        let (vs_code, ps_code) = (compile(s!("vs"), s!("vs_5_0"))?, compile(s!("ps"), s!("ps_5_0"))?);
        let (mut vs, mut ps, mut constants) = (None, None, None);
        let desc = D3D11_BUFFER_DESC { ByteWidth: 16, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
        unsafe {
            device.CreateVertexShader(bytes(&vs_code), None, Some(&mut vs)).map_err(err)?;
            device.CreatePixelShader(bytes(&ps_code), None, Some(&mut ps)).map_err(err)?;
            device.CreateBuffer(&desc, None, Some(&mut constants)).map_err(err)?;
        }
        Ok(ToSdr { vs: vs.ok_or("no vertex shader")?, ps: ps.ok_or("no pixel shader")?, constants: constants.ok_or("no buffer")?, out: None, white: 0.0 })
    }

    /// `src` (FP16 scRGB, w×h) as sRGB BGRA, with SDR white (scRGB units) at 1.0. The texture
    /// returned is the same one each time until the size changes.
    pub fn convert(&mut self, device: &ID3D11Device, context: &ID3D11DeviceContext, src: &ID3D11Texture2D, w: u32, h: u32, white: f32)
        -> Result<ID3D11Texture2D, String> {
        if self.out.as_ref().is_none_or(|(_, _, ow, oh)| (*ow, *oh) != (w, h)) {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, CPUAccessFlags: 0, MiscFlags: 0,
            };
            let (mut tex, mut rtv) = (None, None);
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(err)?;
            let tex = tex.ok_or("no texture")?;
            unsafe { device.CreateRenderTargetView(&tex, None, Some(&mut rtv)) }.map_err(err)?;
            self.out = Some((tex, rtv.ok_or("no render target")?, w, h));
        }
        if white != self.white {
            let c = [white, 0.0, 0.0, 0.0];
            unsafe { context.UpdateSubresource(&self.constants, 0, None, c.as_ptr() as *const _, 0, 0) };
            self.white = white;
        }
        let mut srv = None;
        unsafe { device.CreateShaderResourceView(src, None, Some(&mut srv)) }.map_err(err)?;
        let (tex, rtv, _, _) = self.out.as_ref().unwrap();
        let viewport = D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w as f32, Height: h as f32, MinDepth: 0.0, MaxDepth: 1.0 };
        unsafe {
            context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            context.RSSetViewports(Some(&[viewport]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vs, None);
            context.PSSetShader(&self.ps, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetShaderResources(0, Some(&[srv]));
            context.Draw(3, 0);
            // Let go of the duplicated frame and the target, which the video processor reads next.
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None);
        }
        Ok(tex.clone())
    }
}
