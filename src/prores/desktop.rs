use super::{Burst, FPS, validate_dimensions};
use std::{
    io,
    rc::Rc,
    thread,
    time::{Duration, Instant},
};
use windows::Win32::Graphics::{
    Direct3D::D3D_DRIVER_TYPE_UNKNOWN, Direct3D11::*, Dxgi::Common::*, Dxgi::*,
};
use windows::core::Interface;

pub(super) struct Desktop {
    pub(super) device: ID3D11Device,
    duplication: IDXGIOutputDuplication,
    pub(super) context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    pub width: usize,
    pub height: usize,
}

impl Desktop {
    pub fn open(display: u32) -> io::Result<Self> {
        // SAFETY: all COM outputs own their references; device and output use the same adapter.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(io::Error::other)?;
            let adapter: IDXGIAdapter = factory.EnumAdapters1(0)?.cast()?;
            let output: IDXGIOutput1 = adapter
                .EnumOutputs(display)
                .map_err(|e| {
                    io::Error::other(format!("could not open DXGI display {display}: {e}"))
                })?
                .cast()?;
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.unwrap();
            let duplication = output.DuplicateOutput(&device)?;
            let desc = duplication.GetDesc();
            if !matches!(
                desc.Rotation,
                DXGI_MODE_ROTATION_UNSPECIFIED | DXGI_MODE_ROTATION_IDENTITY
            ) {
                return Err(io::Error::other(
                    "this prototype requires an unrotated SDR display",
                ));
            }
            let (width, height) = (desc.ModeDesc.Width as usize, desc.ModeDesc.Height as usize);
            validate_dimensions(width, height)?;
            let staging = staging_texture(&device, width, height)?;
            Ok(Self {
                device,
                duplication,
                context: context.unwrap(),
                staging,
                width,
                height,
            })
        }
    }

    pub fn next(&self, timeout: u32, first: bool, pixels: &mut Vec<u8>) -> io::Result<bool> {
        Ok(self
            .with_frame(timeout, first, |resource| {
                self.read_pixels(resource, pixels)
            })?
            .is_some())
    }

    pub fn with_frame<T>(
        &self,
        timeout: u32,
        first: bool,
        f: impl FnOnce(&IDXGIResource) -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // SAFETY: the acquired surface is held until the copy finishes. ReleaseFrame runs
        // for every successful acquisition, including pointer-only updates and errors.
        unsafe {
            match self
                .duplication
                .AcquireNextFrame(timeout, &mut info, &mut resource)
            {
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) => {
                    return Err(io::Error::other(format!(
                        "display capture interrupted: {e}"
                    )));
                }
                Ok(()) => {}
            }
            let result = if !first && info.LastPresentTime == 0 {
                Ok(None)
            } else {
                f(resource.as_ref().expect("acquired desktop texture")).map(Some)
            };
            self.duplication.ReleaseFrame()?;
            result
        }
    }

    fn read_pixels(&self, resource: &IDXGIResource, pixels: &mut Vec<u8>) -> io::Result<()> {
        let texture: ID3D11Texture2D = resource.cast()?;
        read_bgra(
            &self.context,
            &self.staging,
            &texture,
            self.width,
            self.height,
            pixels,
        )
    }
}

fn staging_texture(
    device: &ID3D11Device,
    width: usize,
    height: usize,
) -> io::Result<ID3D11Texture2D> {
    // SAFETY: dimensions have been validated and the output owns its COM reference.
    unsafe {
        let texture = D3D11_TEXTURE2D_DESC {
            Width: width as u32,
            Height: height as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..Default::default()
        };
        let mut staging = None;
        device.CreateTexture2D(&texture, None, Some(&mut staging))?;
        Ok(staging.unwrap())
    }
}

fn read_bgra(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    texture: &ID3D11Texture2D,
    width: usize,
    height: usize,
    pixels: &mut Vec<u8>,
) -> io::Result<()> {
    pixels.clear();
    pixels.reserve(width * height * 4);
    // SAFETY: copy only the validated BGRA rectangle to the matching staging
    // texture. Read mapped rows using their driver-provided pitch before unmapping.
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        if desc.Width != width as u32
            || desc.Height != height as u32
            || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
        {
            return Err(io::Error::other(
                "unexpected capture texture size or format",
            ));
        }
        context.CopyResource(staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        for row in 0..height {
            let ptr = mapped
                .pData
                .cast::<u8>()
                .add(row * mapped.RowPitch as usize);
            pixels.extend_from_slice(std::slice::from_raw_parts(ptr, width * 4));
        }
        context.Unmap(staging, 0);
    }
    Ok(())
}

pub(super) fn capture(display: u32) -> io::Result<Burst> {
    let desktop = Desktop::open(display)?;
    let mut first = Vec::new();
    if !desktop.next(2000, true, &mut first)? {
        return Err(io::Error::other(
            "timed out waiting for the first desktop frame",
        ));
    }
    let start = Instant::now();
    let mut frames = vec![Rc::new(first)];
    let mut refreshed = 1;
    while frames.len() < FPS {
        let deadline = start + Duration::from_secs_f64(frames.len() as f64 / FPS as f64);
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
        let slot =
            (start.elapsed().as_nanos() * FPS as u128 / 1_000_000_000).min(FPS as u128) as usize;
        let previous = frames.last().unwrap().clone();
        if slot > frames.len() {
            frames.resize(slot, previous.clone());
        }
        if frames.len() == FPS {
            break;
        }
        let mut pixels = Vec::new();
        let frame = if desktop.next(0, false, &mut pixels)? {
            refreshed += 1;
            Rc::new(pixels)
        } else {
            previous
        };
        frames.push(frame);
    }
    eprintln!("Captured {refreshed} desktop updates across 60 frame slots.");
    Ok(Burst {
        width: desktop.width,
        height: desktop.height,
        frames,
    })
}

pub(crate) fn refresh_rate(display: u32) -> io::Result<u32> {
    use windows::{Win32::Graphics::Gdi::*, core::PCWSTR};
    // SAFETY: query the current mode of the same DXGI output used for capture.
    // DeviceName is null terminated and DEVMODEW is initialized with its size.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let output = factory.EnumAdapters1(0)?.EnumOutputs(display)?;
        let desc = output.GetDesc()?;
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !EnumDisplaySettingsW(
            PCWSTR(desc.DeviceName.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
        .as_bool()
            || mode.dmDisplayFrequency < 2
        {
            return Err(io::Error::other("Could not read the display refresh rate"));
        }
        Ok(mode.dmDisplayFrequency)
    }
}
