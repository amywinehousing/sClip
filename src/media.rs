use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::replay::Snapshot;

fn ffmpeg() -> Command {
    let executable = std::env::var_os("INSTANTREPLAY_FFMPEG").unwrap_or_else(|| {
        let binary = std::env::current_exe().ok();
        if let Some(dir) = binary.as_deref().and_then(Path::parent) {
            let sibling = dir.join("ffmpeg.exe");
            if sibling.is_file() {
                return sibling.into_os_string();
            }
            // Cargo places development and release binaries under target/<profile>/.
            if let Some(root) = dir
                .parent()
                .filter(|p| p.ends_with("target"))
                .and_then(Path::parent)
            {
                let portable = root.join(".tools/ffmpeg.exe");
                if portable.is_file() {
                    return portable.into_os_string();
                }
            }
        }
        OsString::from("ffmpeg")
    });
    let mut cmd = Command::new(executable);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostdin",
        "-xerror",
        "-abort_on",
        "empty_output",
    ]);
    cmd
}

fn capture(display: u32) -> Command {
    let mut cmd = ffmpeg();
    // With BGRA input NVENC converts sRGB to limited-range BT.601 YUV.
    // Describe that conversion in both the muxer parameters and the H.264 VUI.
    let filter = format!(
        "ddagrab=output_idx={display}:framerate=60:dup_frames=1:output_fmt=bgra,\
        setparams=range=limited:color_primaries=bt709:color_trc=iec61966-2-1:colorspace=smpte170m"
    );
    cmd.args([
        "-filter_complex",
        &filter,
        "-an",
        "-c:v",
        "h264_nvenc",
        "-bsf:v",
        "h264_metadata=video_full_range_flag=0:colour_primaries=1:transfer_characteristics=13:matrix_coefficients=6",
        "-preset",
        "p4",
        "-tune",
        "ll",
        "-rc",
        "cbr",
        "-b:v",
        "20M",
        "-maxrate",
        "20M",
        "-bufsize",
        "20M",
        "-r",
        "60",
        "-fps_mode",
        "cfr",
        "-g",
        "60",
        "-bf",
        "0",
        "-rc-lookahead",
        "0",
        "-no-scenecut",
        "1",
        "-forced-idr",
        "1",
        "-force_key_frames",
        "expr:gte(n,n_forced*60)",
    ]);
    cmd
}

fn start(cmd: &mut Command) -> io::Result<Child> {
    cmd.spawn().map_err(|error| io::Error::new(error.kind(), format!(
        "could not start FFmpeg ({error}). Set INSTANTREPLAY_FFMPEG to the full path of ffmpeg.exe."
    )))
}

pub fn start_replay(display: u32) -> io::Result<Child> {
    start(
        capture(display)
            .args([
                "-movflags",
                "+frag_keyframe+empty_moov+default_base_moof+skip_trailer",
                "-flush_packets",
                "1",
                "-f",
                "mp4",
                "pipe:1",
            ])
            .stdout(Stdio::piped()),
    )
}

pub fn clip_path(dir: &Path) -> PathBuf {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    dir.join(format!("clip-{}.mp4", time.as_millis()))
}

pub fn output_temp(output: &Path, extension: &str) -> io::Result<tempfile::NamedTempFile> {
    if !output
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
    {
        return Err(io::Error::other(format!(
            "the output filename must end in .{extension}"
        )));
    }
    if output.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", output.display()),
        ));
    }
    let dir = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    tempfile::Builder::new()
        .prefix(".instantreplay-")
        .suffix(".part")
        .tempfile_in(dir)
}

fn finish(child: &mut Child, file: tempfile::NamedTempFile, output: &Path) -> io::Result<()> {
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "FFmpeg exited with {status}. See its diagnostic above."
        )));
    }
    if file.as_file().metadata()?.len() == 0 {
        return Err(io::Error::other("FFmpeg produced an empty clip"));
    }
    file.persist_noclobber(output)?;
    eprintln!("Saved {}", output.display());
    Ok(())
}

pub fn record(display: u32, seconds: u32, output: &Path) -> io::Result<()> {
    let file = output_temp(output, "mp4")?;
    eprintln!("Recording display {display} for {seconds}s at 60 fps...");
    let mut child = start(
        capture(display)
            .args([
                "-t",
                &seconds.to_string(),
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
                "-y",
            ])
            .arg(file.path()),
    )?;
    finish(&mut child, file, output)
}

pub fn save(snapshot: Snapshot, output: &Path) -> io::Result<()> {
    let file = output_temp(output, "mp4")?;
    let mut child = start(
        ffmpeg()
            .args([
                "-f",
                "mp4",
                "-i",
                "pipe:0",
                "-map",
                "0:v:0",
                "-c:v",
                "copy",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
                "-y",
            ])
            .arg(file.path())
            .stdin(Stdio::piped()),
    )?;
    let mut input = child.stdin.take().expect("piped input");
    let result = (|| {
        input.write_all(&snapshot.header)?;
        for fragment in snapshot.fragments {
            input.write_all(&fragment)?;
        }
        Ok::<_, io::Error>(())
    })();
    drop(input);
    if let Err(error) = result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    finish(&mut child, file, output)
}
