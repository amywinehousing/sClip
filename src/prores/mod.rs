#[cfg(windows)]
pub(crate) mod desktop;
mod encoder;
#[cfg(windows)]
mod gpu;
mod entropy;
mod mov;
#[cfg(windows)]
pub mod replay;
#[cfg(windows)]
mod resize;

use std::io::{self, BufWriter};
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

const FPS: usize = 60;

#[cfg(test)]
mod tests;

struct Burst {
    width: usize,
    height: usize,
    frames: Vec<Rc<Vec<u8>>>,
}

fn validate_dimensions(width: usize, height: usize) -> io::Result<()> {
    // Hold at most one second of uncompressed frames, including 3840x2160.
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || !width.is_multiple_of(2)
        || width as u64 * height as u64 * 4 * FPS as u64 > 2 * 1024 * 1024 * 1024
    {
        return Err(io::Error::other(format!(
            "unsupported {width}x{height} display: use an even width and a resolution up to the 2 GiB capture budget (1080p or UHD 4K)"
        )));
    }
    Ok(())
}

fn pattern() -> Burst {
    let (width, height) = (640, 360);
    let colors = [
        [255, 255, 255],
        [0, 255, 255],
        [255, 255, 0],
        [0, 255, 0],
        [255, 0, 255],
        [0, 0, 255],
        [255, 0, 0],
        [0, 0, 0],
    ];
    let frames = (0..FPS)
        .map(|tick| {
            let mut pixels = Vec::with_capacity(width * height * 4);
            for y in 0..height {
                for x in 0..width {
                    let gray = (x * 255 / (width - 1)) as u8;
                    let bgr = if (x / 8 == tick && y >= height / 2)
                        || (y == height - 1 && x.is_multiple_of(2))
                    {
                        [255; 3]
                    } else if y < height / 2 {
                        colors[x * 8 / width]
                    } else {
                        [gray; 3]
                    };
                    pixels.extend([bgr[0], bgr[1], bgr[2], 255]);
                }
            }
            Rc::new(pixels)
        })
        .collect();
    Burst {
        width,
        height,
        frames,
    }
}

pub fn record(display: u32, test_pattern: bool, output: &Path) -> io::Result<()> {
    let mut file = crate::media::output_temp(output, "mov")?;
    let burst = if test_pattern {
        pattern()
    } else {
        eprintln!("Capturing one second from display {display}...");
        #[cfg(windows)]
        {
            desktop::capture(display)?
        }
        #[cfg(not(windows))]
        {
            return Err(io::Error::other("desktop capture requires Windows"));
        }
    };
    validate_dimensions(burst.width, burst.height)?;
    let start = Instant::now();
    let encoder = encoder::Encoder::new(burst.width, burst.height);
    let writer = BufWriter::new(file.as_file_mut());
    let mut movie = mov::Movie::new(writer, burst.width as u16, burst.height as u16)?;
    let mut last = None;
    let mut encoded = Vec::new();
    let mut encoded_count = 0;
    for pixels in burst.frames {
        if !last.as_ref().is_some_and(|p| Rc::ptr_eq(p, &pixels)) {
            encoded = encoder.frame(&pixels);
            encoded_count += 1;
        }
        movie.sample(&encoded)?;
        last = Some(pixels);
    }
    movie.finish()?;
    file.persist_noclobber(output)?;
    eprintln!(
        "Saved {} ({}x{}, 60 frames, 1 second). Encoded {encoded_count} source frames and wrote the MOV in {:.2}s.",
        output.display(),
        burst.width,
        burst.height,
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
