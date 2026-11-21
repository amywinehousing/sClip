#[path = "../src/prores/encoder.rs"]
mod encoder;
#[path = "../src/prores/entropy.rs"]
mod entropy;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::time::Duration;

fn benchmark(c: &mut Criterion) {
    let (width, height) = (1920, 1080);
    let mut group = c.benchmark_group("prores_1080p");
    group
        .sample_size(20)
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(5));
    for detailed in [false, true] {
        let pixels: Vec<u8> = (0..width * height)
            .flat_map(|i| {
                let (x, y) = (i % width, i / width);
                let texture = if detailed {
                    ((x * 13 + y * 19) ^ (x * y / 53)) as u8
                } else {
                    0
                };
                [
                    (x / 8) as u8 ^ texture,
                    (y / 5) as u8 ^ texture,
                    ((x + y) / 12) as u8,
                    255,
                ]
            })
            .collect();
        let mut encoder = encoder::Encoder::new(width, height);
        #[cfg(windows)]
        encoder.configure(60, 0);
        let name = if detailed { "textured" } else { "smooth" };
        if let Some(dir) = std::env::var_os("PRORES_BENCH_DUMP") {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                std::path::Path::new(&dir).join(format!("{name}.prores")),
                encoder.frame(&pixels),
            )
            .unwrap();
        }
        group.bench_function(name, |b| {
            b.iter(|| black_box(encoder.frame(black_box(&pixels))))
        });
    }
    group.finish();
}
criterion_group!(benches, benchmark);
criterion_main!(benches);
