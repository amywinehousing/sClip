use super::*;
use std::process::Command;

#[test]
fn burst_budget_accepts_uhd_but_rejects_odd_width_and_8k() {
    assert!(validate_dimensions(3840, 2160).is_ok());
    assert!(validate_dimensions(1921, 1080).is_err());
    assert!(validate_dimensions(7680, 4320).is_err());
}

#[test]
#[ignore = "uses an independent FFmpeg decoder"]
fn decode_partial_macroblocks_and_check_contrast() {
    // Thirteen macroblocks across exercise 8-, 4-, and 1-macroblock slices.
    // The last row and column also require pixel replication.
    let (w, h) = (194, 35);
    let reference: Vec<u8> = (0..w * h)
        .map(|i| {
            let x = i % w;
            let y = i / w;
            if ((x / 3 + y / 5 + x * y / 107) % 2) == 0 {
                0
            } else {
                255
            }
        })
        .collect();
    let bgra: Vec<u8> = reference.iter().flat_map(|&g| [g, g, g, 255]).collect();
    let mut temp = tempfile::NamedTempFile::new().unwrap();
    let mut movie = mov::Movie::new(temp.as_file_mut(), w as u16, h as u16).unwrap();
    movie
        .sample(&encoder::Encoder::new(w, h).frame(&bgra))
        .unwrap();
    movie.finish().unwrap();
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join(".tools/ffmpeg.exe");
    let ffmpeg = std::env::var_os("INSTANTREPLAY_FFMPEG").unwrap_or_else(|| {
        if local.is_file() {
            local.into_os_string()
        } else {
            "ffmpeg".into()
        }
    });
    let decoded = Command::new(ffmpeg)
        .args(["-v", "error", "-xerror", "-i"])
        .arg(temp.path())
        .args(["-pix_fmt", "yuv422p10le", "-f", "rawvideo", "pipe:1"])
        .output()
        .unwrap();
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    assert_eq!(decoded.stdout.len(), w * h * 4);
    let samples: Vec<u16> = decoded
        .stdout
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&b| u16::from_le_bytes(b))
        .collect();
    let mut square_error = 0.0;
    for (i, &g) in reference.iter().enumerate() {
        let expected = if g == 0 { 64.0 } else { 940.0 };
        square_error += (f64::from(samples[i]) - expected).powi(2);
    }
    let rms = (square_error / (w * h) as f64).sqrt();
    assert!(rms < 30.0, "10-bit luma RMS error: {rms}");
    assert!(samples[w * h..].iter().all(|&v| v.abs_diff(512) <= 1));
}

#[cfg(windows)]
#[test]
fn bitrate_setting_changes_encoded_size_and_fps_header() {
    let pixels: Vec<u8> = (0..256 * 144)
        .flat_map(|i| {
            let (x, y) = (i % 256, i / 256);
            let v = ((x * 13 + y * 19) ^ (x * y / 53)) as u8;
            [v, v.wrapping_mul(3), v.wrapping_mul(7), 255]
        })
        .collect();
    let mut low = encoder::Encoder::new(256, 144);
    low.configure(30, 1);
    let low = low.frame(&pixels);
    let mut high = encoder::Encoder::new(256, 144);
    high.configure(60, 100);
    let high = high.frame(&pixels);
    assert!(low.len() < high.len());
    assert_eq!(low[21], 5);
    assert_eq!(high[21], 8);
}
