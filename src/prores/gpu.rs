use super::{
    desktop::Desktop,
    encoder::{Encoder, SCAN, WEIGHTS},
};
use std::io;
use windows::{
    Win32::Graphics::{
        Direct3D::Fxc::D3DCompile, Direct3D11::*, Dxgi::Common::*, Dxgi::IDXGIResource,
    },
    core::{Interface, PCSTR, s},
};

pub(super) struct Transform {
    input: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
    sampler: ID3D11SamplerState,
    shader: ID3D11ComputeShader,
    output: ID3D11Buffer,
    writable: ID3D11UnorderedAccessView,
    staging: ID3D11Buffer,
    cols: u32,
    rows: u32,
}

impl Transform {
    pub fn new(
        desktop: &Desktop,
        encoder: &Encoder,
        width: usize,
        height: usize,
    ) -> io::Result<Self> {
        let cols = width.div_ceil(16) as u32;
        let rows = height.div_ceil(16) as u32;
        fn table<T: std::fmt::Display>(values: impl Iterator<Item = T>) -> String {
            values.map(|v| v.to_string()).collect::<Vec<_>>().join(",")
        }
        let source = format!(
            "#define WIDTH {width}\n#define HEIGHT {height}\nstatic const float transfer[256] = {{{}}};\nstatic const float basis[32] = {{{}}};\nstatic const uint scan[64] = {{{}}};\nstatic const uint weights[64] = {{{}}};\n{}",
            table(encoder.rec709.iter()),
            table(encoder.basis.iter().flatten()),
            table(SCAN.iter()),
            table(WEIGHTS.iter()),
            include_str!("transform.hlsl")
        );
        let device = &desktop.device;
        // Each resource is owned by this capture device and reused for the session.
        unsafe {
            let (mut code, mut errors) = (None, None);
            if let Err(err) = D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                PCSTR::null(),
                None,
                None,
                s!("main"),
                s!("cs_5_0"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            ) {
                let message = errors.map(|blob| {
                    String::from_utf8_lossy(std::slice::from_raw_parts(
                        blob.GetBufferPointer().cast(),
                        blob.GetBufferSize(),
                    ))
                    .trim_end_matches('\0')
                    .to_owned()
                });
                return Err(io::Error::other(message.unwrap_or_else(|| err.to_string())));
            }
            let code = code.unwrap();
            let mut shader = None;
            device.CreateComputeShader(
                std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize()),
                None,
                Some(&mut shader),
            )?;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: desktop.width as _,
                Height: desktop.height as _,
                ArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as _,
                ..Default::default()
            };
            let mut input = None;
            device.CreateTexture2D(&desc, None, Some(&mut input))?;
            let input = input.unwrap();
            let mut view = None;
            device.CreateShaderResourceView(&input, None, Some(&mut view))?;
            let mut sampler = None;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    ComparisonFunc: D3D11_COMPARISON_NEVER,
                    MaxAnisotropy: 1,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            let bytes = cols * rows * 8 * 64 * 4;
            let mut output = None;
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: bytes,
                    BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as _,
                    MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as _,
                    StructureByteStride: 4,
                    ..Default::default()
                },
                None,
                Some(&mut output),
            )?;
            let output = output.unwrap();
            let mut writable = None;
            device.CreateUnorderedAccessView(&output, None, Some(&mut writable))?;
            let mut staging = None;
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: bytes,
                    Usage: D3D11_USAGE_STAGING,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as _,
                    ..Default::default()
                },
                None,
                Some(&mut staging),
            )?;
            Ok(Self {
                input,
                view: view.unwrap(),
                sampler: sampler.unwrap(),
                shader: shader.unwrap(),
                output,
                writable: writable.unwrap(),
                staging: staging.unwrap(),
                cols,
                rows,
            })
        }
    }

    pub fn dispatch(
        &self,
        context: &ID3D11DeviceContext,
        resource: &IDXGIResource,
    ) -> io::Result<()> {
        let texture: ID3D11Texture2D = resource.cast()?;
        unsafe {
            context.CopyResource(&self.input, &texture);
            context.CSSetShader(&self.shader, None);
            context.CSSetShaderResources(0, Some(&[Some(self.view.clone())]));
            context.CSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.CSSetUnorderedAccessViews(0, 1, Some(&Some(self.writable.clone())), None);
            context.Dispatch(self.cols, self.rows, 8);
            context.CSSetUnorderedAccessViews(0, 1, Some(&None), None);
            context.CSSetShaderResources(0, Some(&[None]));
            context.CopyResource(&self.staging, &self.output);
        }
        Ok(())
    }

    pub fn encode(&self, context: &ID3D11DeviceContext, encoder: &Encoder) -> io::Result<Vec<u8>> {
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            // The mapped buffer contains eight contiguous blocks per macroblock.
            // All Rayon workers finish reading it before Unmap.
            let blocks = std::slice::from_raw_parts(
                mapped.pData.cast(),
                (self.cols * self.rows * 8) as usize,
            );
            let frame = encoder.transformed(blocks);
            context.Unmap(&self.staging, 0);
            Ok(frame)
        }
    }
}
