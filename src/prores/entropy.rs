// SMPTE RDD 36:2022 section 7.1.1. Codes are written most significant bit first.
// Flush four bytes at once. At most 31 pending bits and one bounded
// coefficient code fit in the accumulator.
struct Bits<'a> {
    bytes: &'a mut Vec<u8>,
    tail: u64,
    count: u32,
}

impl Bits<'_> {
    #[inline]
    fn put(&mut self, code: Code) {
        self.tail = (self.tail << code.width) | u64::from(code.value);
        self.count += code.width;
        if self.count >= 32 {
            self.count -= 32;
            self.bytes
                .extend_from_slice(&((self.tail >> self.count) as u32).to_be_bytes());
        }
    }

    fn finish(self) {
        if self.count != 0 {
            let aligned = (self.tail << (32 - self.count)) as u32;
            self.bytes
                .extend_from_slice(&aligned.to_be_bytes()[..self.count.div_ceil(8) as usize]);
        }
    }
}

#[derive(Clone, Copy)]
struct Code {
    value: u32,
    width: u32,
}

const fn exp(symbol: u32, order: u32, prefix: u32) -> Code {
    let value = symbol + (1 << order);
    Code {
        value,
        width: prefix + 2 * (32 - value.leading_zeros()) - order - 1,
    }
}

#[derive(Clone, Copy)]
struct Book {
    rice_prefixes: u32,
    rice: u32,
    exp: u32,
}

const fn book(rice_prefixes: u32, rice: u32, exp: u32) -> Book {
    Book {
        rice_prefixes,
        rice,
        exp,
    }
}

const DC: [Book; 4] = [book(0, 0, 0), book(0, 0, 1), book(2, 2, 3), book(0, 0, 3)];

const fn run_book(previous: u32) -> Book {
    match previous {
        0..=1 => book(3, 0, 1),
        2..=3 => book(2, 0, 1),
        4 => book(0, 0, 0),
        5..=8 => book(2, 1, 2),
        9..=14 => book(0, 0, 1),
        _ => book(0, 0, 2),
    }
}

const fn level_book(previous: u32) -> Book {
    match previous {
        0 => book(3, 0, 2),
        1 => book(2, 0, 1),
        2 => book(3, 0, 1),
        3 => book(0, 0, 0),
        4..=7 => book(0, 0, 1),
        _ => book(0, 0, 2),
    }
}

const fn encode(value: u32, book: Book) -> Code {
    let cutoff = book.rice_prefixes << book.rice;
    if value < cutoff {
        Code {
            value: (1 << book.rice) | (value & ((1 << book.rice) - 1)),
            width: 1 + book.rice + (value >> book.rice),
        }
    } else {
        exp(value - cutoff, book.exp, book.rice_prefixes)
    }
}

// Most run lengths and levels fit in this table. Longer symbols use the
// identical Rice/exponential code calculation.
struct CodeTable {
    book: Book,
    short: [Code; 64],
}
impl CodeTable {
    const fn new(book: Book) -> Self {
        let mut short = [Code { value: 0, width: 0 }; 64];
        let mut i = 0;
        while i < short.len() {
            short[i] = encode(i as u32, book);
            i += 1;
        }
        Self { book, short }
    }

    #[inline]
    fn get(&self, value: u32) -> Code {
        if let Some(&code) = self.short.get(value as usize) {
            code
        } else {
            encode(value, self.book)
        }
    }
}

static RUN: [CodeTable; 16] = {
    let mut tables = [const { CodeTable::new(run_book(0)) }; 16];
    let mut i = 0;
    while i < tables.len() {
        tables[i] = CodeTable::new(run_book(i as u32));
        i += 1;
    }
    tables
};
static LEVEL: [CodeTable; 9] = {
    let mut tables = [const { CodeTable::new(level_book(0)) }; 9];
    let mut i = 0;
    while i < tables.len() {
        tables[i] = CodeTable::new(level_book(i as u32));
        i += 1;
    }
    tables
};

fn signed(value: i32) -> u32 {
    (value.unsigned_abs() << 1) - u32::from(value < 0)
}

fn quantized(blocks: &[[f32; 64]], scale: f32, output: &mut [[i32; 64]]) {
    debug_assert_eq!(blocks.len(), output.len());
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        // SAFETY: SSE2 is baseline on x86-64. Each load/store covers four elements
        // within a 64-element block. Encoder DCT coefficients are finite and far
        // inside i32 range. Truncation plus an exact half test matches f32::round,
        // including negative ties, without changing the floating-point mode.
        unsafe {
            let multiplier = _mm_set1_ps(scale);
            let sign_mask = _mm_set1_ps(-0.0);
            for (input, out) in blocks.iter().zip(output) {
                for i in (0..64).step_by(4) {
                    let value = _mm_mul_ps(_mm_loadu_ps(input.as_ptr().add(i)), multiplier);
                    let truncated = _mm_cvttps_epi32(value);
                    let fraction =
                        _mm_andnot_ps(sign_mask, _mm_sub_ps(value, _mm_cvtepi32_ps(truncated)));
                    let adjust = _mm_castps_si128(_mm_cmpge_ps(fraction, _mm_set1_ps(0.5)));
                    let direction = _mm_or_si128(
                        _mm_srai_epi32::<31>(_mm_castps_si128(value)),
                        _mm_set1_epi32(1),
                    );
                    let rounded = _mm_add_epi32(truncated, _mm_and_si128(adjust, direction));
                    _mm_storeu_si128(out.as_mut_ptr().add(i).cast(), rounded);
                }
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    for (input, out) in blocks.iter().zip(output) {
        for (value, quantized) in input.iter().zip(out) {
            *quantized = (*value * scale).round() as i32;
        }
    }
}

// Input blocks are already in progressive scan order, scaled for q=1.
pub(super) fn component(
    input: &[[f32; 64]],
    q: u8,
    scratch: &mut [[i32; 64]],
    bytes: &mut Vec<u8>,
) {
    let blocks = &mut scratch[..input.len()];
    quantized(input, 1.0 / f32::from(q), blocks);
    let mut bits = Bits {
        bytes,
        tail: 0,
        count: 0,
    };
    let mut previous_dc = blocks[0][0];
    bits.put(exp(signed(previous_dc), 5, 0));
    let mut previous_diff = 3i32;
    for block in &blocks[1..] {
        let dc = block[0];
        let diff = dc - previous_dc;
        let code = if previous_diff < 0 { -diff } else { diff };
        bits.put(encode(
            signed(code),
            DC[previous_diff.unsigned_abs().min(3) as usize],
        ));
        previous_dc = dc;
        previous_diff = diff;
    }
    let (mut run, mut previous_run, mut previous_level) = (0, 4, 1);
    for frequency in 1..64 {
        for block in blocks.iter() {
            let level = block[frequency];
            if level == 0 {
                run += 1;
                continue;
            }
            bits.put(RUN[previous_run.min(15) as usize].get(run));
            let symbol = level.unsigned_abs() - 1;
            let magnitude = LEVEL[previous_level.min(8) as usize].get(symbol);
            bits.put(Code {
                value: (magnitude.value << 1) | u32::from(level < 0),
                width: magnitude.width + 1,
            });
            previous_run = run;
            previous_level = symbol;
            run = 0;
        }
    }
    bits.finish()
}

#[cfg(test)]
mod tests {
    #[test]
    fn packs_short_and_extended_codes_across_word_boundaries() {
        // The original writer emitted these bytes for a signed DC difference,
        // successive levels across the 7/8 context boundary and a long zero run.
        let mut blocks = [[0.0; 64]; 2];
        blocks[0][0] = 96.0;
        blocks[1][0] = -192.0;
        blocks[0][1] = -24.0;
        blocks[1][1] = 27.0;
        blocks[0][2] = -270.0;
        blocks[1][2] = 12.0;
        blocks[1][54] = -6.0;
        let mut bytes = Vec::new();
        super::component(&blocks, 3, &mut [[0; 64]; 2], &mut bytes);
        assert_eq!(
            bytes,
            [0x60, 0x0c, 0x78, 0xf9, 0x48, 0x5d, 0xf8, 0x03, 0x32, 0x80]
        );
    }

    #[test]
    fn simd_quantization_matches_scalar_at_ties_and_neighbors() {
        for q in 1..=6 {
            let scale = 1.0 / q as f32;
            for base in -8192..8192 {
                let block = std::array::from_fn(|i| {
                    let value = base as f32 + (i / 3) as f32 * 0.5;
                    match i % 3 {
                        0 => value,
                        1 => value.next_down(),
                        _ => value.next_up(),
                    }
                });
                let mut out = [[0; 64]];
                super::quantized(&[block], scale, &mut out);
                assert_eq!(out[0], block.map(|v| (v * scale).round() as i32));
            }
        }
    }
}
