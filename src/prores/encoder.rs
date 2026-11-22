use rayon::prelude::*;
use std::f32::consts::{FRAC_1_SQRT_2, PI};

use super::entropy;

pub(super) const WEIGHTS: [u8; 64] = [
    4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 5,
    4, 4, 4, 4, 4, 4, 5, 5, 4, 4, 4, 4, 4, 5, 5, 6, 4, 4, 4, 4, 5, 5, 6, 7, 4, 4, 4, 4, 5, 6, 7, 7,
];
// Natural coefficient location -> progressive scan position (RDD 36 figure 4).
pub(super) const SCAN: [usize; 64] = [
    0, 1, 4, 5, 16, 17, 21, 22, 2, 3, 6, 7, 18, 20, 23, 28, 8, 9, 12, 13, 19, 24, 27, 29, 10, 11,
    14, 15, 25, 26, 30, 31, 32, 33, 37, 38, 45, 46, 53, 54, 34, 36, 39, 44, 47, 52, 55, 60, 35, 40,
    43, 48, 51, 56, 59, 61, 41, 42, 49, 50, 57, 58, 62, 63,
];

#[derive(Clone, Copy)]
enum Input<'a> {
    Pixels(&'a [u8]),
    #[cfg(windows)]
    Transformed(&'a [[f32; 64]]),
}

pub(super) struct Encoder {
    width: usize,
    height: usize,
    pub(super) rec709: [f32; 256],
    pub(super) basis: [[f32; 4]; 8],
    budget: usize,
    max_q: u8,
    fps_code: u8,
}

// Each row is assembled independently by a Rayon worker.
#[derive(Default)]
struct Row {
    sizes: Vec<u16>,
    payload: Vec<u8>,
}
struct Scratch {
    blocks: [Vec<[f32; 64]>; 3],
    quantized: [[i32; 64]; 32], // Up to eight macroblocks, four luma blocks each.
}
impl Default for Scratch {
    fn default() -> Self {
        Self {
            blocks: [
                Vec::with_capacity(32),
                Vec::with_capacity(16),
                Vec::with_capacity(16),
            ],
            quantized: [[0; 64]; 32],
        }
    }
}

impl Encoder {
    pub fn new(width: usize, height: usize) -> Self {
        let rec709 = std::array::from_fn(|i| {
            let s = i as f32 / 255.0;
            let light = if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            };
            if light < 0.018 {
                4.5 * light
            } else {
                1.099 * light.powf(0.45) - 0.099
            }
        });
        let basis = std::array::from_fn(|k| {
            std::array::from_fn(|i| {
                let norm = if k == 0 { FRAC_1_SQRT_2 } else { 1.0 };
                0.5 * norm * (((2 * i + 1) * k) as f32 * PI / 16.0).cos()
            })
        });
        Self {
            width,
            height,
            rec709,
            basis,
            budget: 112,
            max_q: 6,
            fps_code: 8,
        }
    }

    #[cfg(windows)]
    pub fn configure(&mut self, fps: u32, bitrate: u32) {
        self.fps_code = match fps {
            24 => 2,
            25 => 3,
            30 => 5,
            50 => 6,
            60 => 8,
            _ => 0,
        };
        if bitrate != 0 {
            let macroblocks = self.width.div_ceil(16) * self.height.div_ceil(16);
            self.budget = ((u64::from(bitrate) * 1_000_000 / 8 / u64::from(fps)) as usize
                / macroblocks)
                .max(8);
            self.max_q = 63;
        }
    }

    fn dct_line(&self, input: [f32; 8]) -> [f32; 8] {
        let sums: [f32; 4] = std::array::from_fn(|i| input[i] + input[7 - i]);
        let diffs: [f32; 4] = std::array::from_fn(|i| input[i] - input[7 - i]);
        std::array::from_fn(|k| {
            let t = if k % 2 == 0 { &sums } else { &diffs };
            let b = &self.basis[k];
            t[0] * b[0] + t[1] * b[1] + t[2] * b[2] + t[3] * b[3]
        })
    }

    fn transform(&self, pixels: &[f32], stride: usize, offset: usize) -> [f32; 64] {
        let rows: [[f32; 8]; 8] = std::array::from_fn(|y| {
            let p = offset + stride * y;
            self.dct_line(pixels[p..p + 8].try_into().unwrap())
        });
        let mut result = [0.0; 64];
        let columns: [[f32; 8]; 8] = std::array::from_fn(|u| rows.map(|row| row[u]));
        for (u, column) in columns.into_iter().enumerate() {
            let col = self.dct_line(column);
            for (v, coefficient) in col.into_iter().enumerate() {
                let index = 8 * v + u;
                // RDD 36 normalizes input samples to nine signed bits before the DCT.
                result[SCAN[index]] = coefficient * (4.0 / f32::from(WEIGHTS[index]));
            }
        }
        result
    }

    fn samples(&self, row: &[u8], x: usize) -> [f32; 3] {
        let p = 4 * x.min(self.width - 1);
        let [b, g, r] = [row[p], row[p + 1], row[p + 2]].map(|v| self.rec709[v as usize]);
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        // Limited range, centered about the 10-bit midpoint (512).
        [
            876.0 * luma - 448.0,
            (b - luma) * (448.0 / 0.9278),
            (r - luma) * (448.0 / 0.7874),
        ]
    }

    fn macroblock(&self, bgra: &[u8], x: usize, y: usize, blocks: &mut [Vec<[f32; 64]>; 3]) {
        let (mut luma, mut cb, mut cr) = ([0.0; 256], [0.0; 128], [0.0; 128]);
        for dy in 0..16 {
            let offset = (y + dy).min(self.height - 1) * self.width * 4;
            let row = &bgra[offset..offset + self.width * 4];
            let mut previous = self.samples(row, x.saturating_sub(1));
            for pair in 0..8 {
                let first = self.samples(row, x + 2 * pair);
                let second = self.samples(row, x + 2 * pair + 1);
                luma[dy * 16 + 2 * pair] = first[0];
                luma[dy * 16 + 2 * pair + 1] = second[0];
                cb[dy * 8 + pair] = (previous[1] + 2.0 * first[1] + second[1]) * 0.25;
                cr[dy * 8 + pair] = (previous[2] + 2.0 * first[2] + second[2]) * 0.25;
                previous = second;
            }
        }
        for offset in [0, 8, 128, 136] {
            blocks[0].push(self.transform(&luma, 16, offset));
        }
        for offset in [0, 64] {
            blocks[1].push(self.transform(&cb, 8, offset));
            blocks[2].push(self.transform(&cr, 8, offset));
        }
    }

    fn slice(
        &self,
        input: Input<'_>,
        x_mb: usize,
        y_mb: usize,
        count: usize,
        scratch: &mut Scratch,
        row: &mut Row,
    ) {
        let blocks = &mut scratch.blocks;
        for channel in blocks.iter_mut() {
            channel.clear();
        }
        for mb in x_mb..x_mb + count {
            match input {
                Input::Pixels(bgra) => self.macroblock(bgra, mb * 16, y_mb * 16, blocks),
                #[cfg(windows)]
                Input::Transformed(data) => {
                    let base = (y_mb * self.width.div_ceil(16) + mb) * 8;
                    let mb = &data[base..base + 8];
                    blocks[0].extend_from_slice(&mb[..4]);
                    blocks[1].extend_from_slice(&mb[4..6]);
                    blocks[2].extend_from_slice(&mb[6..]);
                }
            }
        }
        // At 1080p60 the HQ target is about 112 bytes per padded macroblock.
        // Keep fine edges sharp on noisy slices even when they exceed the soft target.
        let max_q = self.max_q;
        let budget = count * self.budget;
        let mut q = if max_q == 6 { 3u8 } else { 1u8 };
        let start = row.payload.len();
        loop {
            row.payload.resize(start + 6, 0);
            let mut lengths = [0; 3];
            for (channel, length) in blocks.iter().zip(&mut lengths) {
                let before = row.payload.len();
                entropy::component(channel, q, &mut scratch.quantized, &mut row.payload);
                *length = (row.payload.len() - before) as u16;
                // Even this partial size forces the next trial to the maximum q.
                // Remaining components cannot change that decision.
                if q < max_q
                    && usize::from(q) * (row.payload.len() - start)
                        > usize::from(max_q - 1) * budget
                {
                    break;
                }
            }
            let size = row.payload.len() - start;
            if size <= budget || q == max_q {
                let header = &mut row.payload[start..start + 6];
                header[..2].copy_from_slice(&[6 << 3, q]);
                header[2..4].copy_from_slice(&lengths[0].to_be_bytes());
                header[4..6].copy_from_slice(&lengths[1].to_be_bytes());
                row.sizes.push(size as u16);
                return;
            }
            q = ((usize::from(q) * size).div_ceil(budget))
                .clamp(usize::from(q) + 1, usize::from(max_q)) as u8;
        }
    }

    pub fn frame(&self, bgra: &[u8]) -> Vec<u8> {
        self.encode(Input::Pixels(bgra))
    }

    #[cfg(windows)]
    pub(super) fn transformed(&self, blocks: &[[f32; 64]]) -> Vec<u8> {
        self.encode(Input::Transformed(blocks))
    }

    fn encode(&self, input: Input<'_>) -> Vec<u8> {
        let cols = self.width.div_ceil(16);
        let rows = self.height.div_ceil(16);
        let row_slices: Vec<Row> = (0..rows)
            .into_par_iter()
            .map_init(Scratch::default, |scratch, y| {
                let mut row = Row::default();
                let mut x = 0;
                while x < cols {
                    let count = (cols - x).min(8).ilog2();
                    let count = 1 << count;
                    self.slice(input, x, y, count, scratch, &mut row);
                    x += count;
                }
                row
            })
            .collect();
        let count: usize = row_slices.iter().map(|row| row.sizes.len()).sum();
        let payload: usize = row_slices.iter().map(|row| row.payload.len()).sum();
        let picture_size = 8 + count * 2 + payload;
        let header_size = 20 + WEIGHTS.len(); // Chroma uses the same quantization matrix.
        let frame_size = 8 + header_size + picture_size;
        let mut out = Vec::with_capacity(frame_size);
        out.extend((frame_size as u32).to_be_bytes());
        out.extend(b"icpf");
        out.extend((header_size as u16).to_be_bytes());
        out.extend([0, 0]);
        out.extend(b"IRrs");
        out.extend((self.width as u16).to_be_bytes());
        out.extend((self.height as u16).to_be_bytes());
        out.extend([0x80, self.fps_code, 1, 1, 1, 0, 0, 2]);
        out.extend(WEIGHTS);
        out.push(8 << 3);
        out.extend((picture_size as u32).to_be_bytes());
        out.extend((count as u16).to_be_bytes());
        out.push(3 << 4);
        for row in &row_slices {
            for size in &row.sizes {
                out.extend(size.to_be_bytes());
            }
        }
        for row in row_slices {
            out.extend_from_slice(&row.payload);
        }
        out
    }
}
