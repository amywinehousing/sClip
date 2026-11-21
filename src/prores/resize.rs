use rayon::prelude::*;

// Precompute bilinear sample positions once for each capture size.
pub(super) struct Scaler {
    source_width: usize,
    width: usize,
    height: usize,
    x: Vec<(usize, usize, u32)>,
    y: Vec<(usize, usize, u32)>,
    output: Vec<u8>,
}

fn axis(source: usize, destination: usize) -> Vec<(usize, usize, u32)> {
    (0..destination)
        .map(|i| {
            let position = (((i as f64 + 0.5) * source as f64 / destination as f64) - 0.5).max(0.0);
            let first = position.floor() as usize;
            (
                first,
                (first + 1).min(source - 1),
                ((position - first as f64) * 256.0) as u32,
            )
        })
        .collect()
}

impl Scaler {
    pub fn new(width: usize, height: usize, limit: u32) -> Self {
        let h = if limit == 0 {
            height
        } else {
            height.min(limit as usize)
        };
        let w = (width * h / height).max(2) & !1;
        let resize = h != height || w != width;
        Self {
            source_width: width,
            width: w,
            height: h,
            x: if resize { axis(width, w) } else { Vec::new() },
            y: if resize { axis(height, h) } else { Vec::new() },
            output: if resize {
                vec![0; w * h * 4]
            } else {
                Vec::new()
            },
        }
    }
    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
    pub fn frame<'a>(&'a mut self, pixels: &'a [u8]) -> &'a [u8] {
        if self.output.is_empty() {
            return pixels;
        }
        let stride = self.source_width * 4;
        self.output
            .par_chunks_mut(self.width * 4)
            .zip(&self.y)
            .for_each(|(row, &(y0, y1, fy))| {
                for (pixel, &(x0, x1, fx)) in row.as_chunks_mut::<4>().0.iter_mut().zip(&self.x) {
                    for c in 0..4 {
                        let a = u32::from(pixels[y0 * stride + x0 * 4 + c]) * (256 - fx)
                            + u32::from(pixels[y0 * stride + x1 * 4 + c]) * fx;
                        let b = u32::from(pixels[y1 * stride + x0 * 4 + c]) * (256 - fx)
                            + u32::from(pixels[y1 * stride + x1 * 4 + c]) * fx;
                        pixel[c] = ((a * (256 - fy) + b * fy + 32768) >> 16) as u8;
                    }
                }
            });
        &self.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scaling_preserves_aspect_and_bilinear_colors() {
        assert_eq!(Scaler::new(1920, 1080, 720).dimensions(), (1280, 720));
        assert_eq!(Scaler::new(1280, 720, 1080).dimensions(), (1280, 720));
        let mut scaler = Scaler::new(4, 4, 2);
        let pixels: Vec<u8> = (0..16).flat_map(|i| [(i * 10) as u8, 0, 0, 255]).collect();
        let scaled = scaler.frame(&pixels);
        assert_eq!(
            scaled,
            &[25, 0, 0, 255, 45, 0, 0, 255, 105, 0, 0, 255, 125, 0, 0, 255]
        );
        let mut native = Scaler::new(4, 4, 0);
        assert_eq!(native.frame(&pixels).as_ptr(), pixels.as_ptr());
    }
}
