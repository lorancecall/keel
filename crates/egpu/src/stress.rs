//! `keel egpu stress <seconds>`: a steady full load on the eGPU for the main window's stress test.
//! A small D3D11 compute shader (arithmetic only, a 4 MB buffer) is dispatched in batches on the
//! eGPU adapter; each batch is waited for, so no single submission comes near the 2 s GPU timeout
//! (TDR). The window measures power, temperature, clock and what held the clock back from the
//! service's telemetry while this runs; this command only makes the load. User session, no admin.
use anyhow::{Context, Result, anyhow, bail};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
use windows::core::s;

const SHADER: &str = r#"
RWStructuredBuffer<float4> buf : register(u0);
[numthreads(256, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    float4 v = buf[id.x] + float4(id.x * 1e-7, 0.1, 0.2, 0.3);
    [loop] for (int i = 0; i < 2048; i++) {
        v = v * 0.99991 + float4(0.00013, 0.00017, 0.00019, 0.00023);
        v = mad(v, v.yzwx, v.wxyz) * 0.5;
    }
    buf[id.x] = v;
}
"#;
/// 1024 groups x 256 threads = 262 144 threads per dispatch (4 MB buffer of float4).
const GROUPS: u32 = 1024;
const BATCH: u32 = 64;

/// The eGPU's DXGI adapter: the one whose vendor / device has a profile in the config.
fn egpu_adapter(cfg: &crate::config::Config) -> Result<(IDXGIAdapter1, String)> {
    unsafe {
        let f: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut i = 0;
        while let Ok(a) = f.EnumAdapters1(i) {
            let d = a.GetDesc1()?;
            if cfg.profile_for(d.VendorId, d.DeviceId).is_some() {
                let name = String::from_utf16_lossy(&d.Description[..d.Description.iter().position(|&c| c == 0).unwrap_or(128)]);
                return Ok((a, name));
            }
            i += 1;
        }
    }
    bail!("no eGPU attached (no adapter matches a profile in the config)")
}

fn blob_text(b: &ID3DBlob) -> String {
    unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).into_owned() }
}

/// Runs the load for `secs` seconds; prints one JSON line per second (`{"elapsed":..,"secs":..}`)
/// and a last line `{"done":true,...}` so the window can follow it.
pub fn run(secs: u64) -> Result<()> {
    anyhow::ensure!((5..=1800).contains(&secs), "length 5-1800 seconds");
    let cfg = crate::config::Config::load();
    let (adapter, name) = egpu_adapter(&cfg)?;
    unsafe {
        let mut dev: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(&adapter, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0),
                          Some(&[D3D_FEATURE_LEVEL_11_0]), D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx))
            .context("D3D11CreateDevice on the eGPU")?;
        let (dev, ctx) = (dev.ok_or_else(|| anyhow!("no device"))?, ctx.ok_or_else(|| anyhow!("no context"))?);

        let mut code: Option<ID3DBlob> = None;
        let mut err: Option<ID3DBlob> = None;
        let r = D3DCompile(SHADER.as_ptr().cast(), SHADER.len(), s!("stress"), None, None, s!("main"), s!("cs_5_0"), 0, 0, &mut code, Some(&mut err));
        if let Err(e) = r { bail!("shader: {e} {}", err.as_ref().map(blob_text).unwrap_or_default()); }
        let code = code.ok_or_else(|| anyhow!("no shader code"))?;
        let bytes = std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize());
        let mut cs: Option<ID3D11ComputeShader> = None;
        dev.CreateComputeShader(bytes, None, Some(&mut cs))?;
        let cs = cs.ok_or_else(|| anyhow!("no compute shader"))?;

        let n = GROUPS * 256;
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: n * 16, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
            CPUAccessFlags: 0, MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32, StructureByteStride: 16,
        };
        let mut buf: Option<ID3D11Buffer> = None;
        dev.CreateBuffer(&desc, None, Some(&mut buf))?;
        let buf = buf.ok_or_else(|| anyhow!("no buffer"))?;
        let mut uav: Option<ID3D11UnorderedAccessView> = None;
        dev.CreateUnorderedAccessView(&buf, None, Some(&mut uav))?;

        let qd = D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 };
        let mut q: Option<ID3D11Query> = None;
        dev.CreateQuery(&qd, Some(&mut q))?;
        let q = q.ok_or_else(|| anyhow!("no query"))?;

        ctx.CSSetShader(&cs, None);
        ctx.CSSetUnorderedAccessViews(0, 1, Some(&uav), None);
        let start = Instant::now();
        let end = start + Duration::from_secs(secs);
        let mut last = 0u64;
        let mut batches = 0u64;
        println!(r#"{{"started":true,"gpu":{:?},"secs":{secs}}}"#, name);
        while Instant::now() < end {
            for _ in 0..BATCH { ctx.Dispatch(GROUPS, 1, 1); }
            ctx.End(&q);
            // Wait for this batch (keeps each submission short; the CPU mostly sleeps). The event
            // query writes TRUE once the GPU has passed it (S_FALSE and nothing written before).
            let mut done = windows::core::BOOL(0);
            loop {
                let _ = ctx.GetData(&q, Some(&mut done as *mut _ as *mut core::ffi::c_void), 4, 0);
                if done.as_bool() || Instant::now() > end + Duration::from_secs(5) { break; }
                std::thread::sleep(Duration::from_micros(500));
            }
            batches += 1;
            let el = start.elapsed().as_secs();
            if el != last { last = el; println!(r#"{{"elapsed":{el},"secs":{secs}}}"#); }
        }
        println!(r#"{{"done":true,"elapsed":{},"batches":{batches}}}"#, start.elapsed().as_secs());
    }
    Ok(())
}
