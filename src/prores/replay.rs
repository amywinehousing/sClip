use std::collections::VecDeque;
use std::io::{self, BufRead, BufWriter};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{desktop::Desktop, encoder::Encoder, mov::Movie};
use crate::{hotkeys::Hotkeys, media, replay::Action};

#[derive(Clone, Copy)]
pub struct Settings {
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub seconds: u32,
    pub cpu_only: bool,
}

const MEMORY_LIMIT: usize = 4 * 1024 * 1024 * 1024;

#[derive(Clone)]
struct Frame {
    // Only the first slot referencing this allocation accounts for its storage.
    bytes: Arc<Vec<u8>>,
    charged: usize,
}

#[derive(Clone)]
struct History {
    width: u16,
    height: u16,
    origin: Instant,
    end: u64,
    frames: VecDeque<Frame>,
    bytes: usize,
    warned: bool,
    capacity: usize,
    fps: u32,
}

impl History {
    fn new(width: u16, height: u16, fps: u32, seconds: u32) -> Self {
        Self {
            fps,
            width,
            height,
            origin: Instant::now(),
            end: 0,
            frames: VecDeque::new(),
            bytes: 0,
            warned: false,
            capacity: seconds as usize * fps as usize,
        }
    }

    fn slot(&self, time: Instant) -> u64 {
        (time.saturating_duration_since(self.origin).as_nanos() * u128::from(self.fps)
            / 1_000_000_000) as u64
    }

    fn append(&mut self, frame: Frame) {
        self.bytes += frame.charged;
        self.frames.push_back(frame);
        self.end += 1;
        while self.frames.len() > self.capacity || self.bytes > MEMORY_LIMIT {
            if self.bytes > MEMORY_LIMIT && !self.warned {
                eprintln!(
                    "ProRes history reached 4 GiB. Retaining a shorter replay until bitrate falls."
                );
                self.warned = true;
            }
            let old = self.frames.pop_front().unwrap();
            // Transfer ownership of the byte charge when duplicate slots remain.
            if let Some(next) = self
                .frames
                .front_mut()
                .filter(|f| Arc::ptr_eq(&f.bytes, &old.bytes))
            {
                next.charged += old.charged;
            } else {
                self.bytes -= old.charged;
            }
        }
    }

    fn extend_to(&mut self, end: u64) {
        if let Some(last) = self.frames.back().cloned() {
            // Bound work after a long suspend or a stalled capture.
            self.end = self.end.max(end.saturating_sub(self.capacity as u64));
            while self.end < end {
                self.append(Frame {
                    bytes: last.bytes.clone(),
                    charged: 0,
                });
            }
        }
    }

    fn push(&mut self, slot: u64, bytes: Vec<u8>) {
        if self.frames.is_empty() {
            self.end = slot;
        }
        self.extend_to(slot);
        self.append(Frame {
            charged: bytes.len(),
            bytes: Arc::new(bytes),
        });
    }

    fn snapshot(&self, at: Instant) -> Vec<Frame> {
        let mut copy = self.clone();
        copy.extend_to(self.slot(at) + 1);
        copy.frames.into_iter().collect()
    }
}

fn capture_loop(
    display: u32,
    settings: Settings,
    shared: &Mutex<Option<History>>,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut allow_gpu = !settings.cpu_only;
    while !stop.load(Ordering::Relaxed) {
        let desktop = match Desktop::open(display) {
            Ok(desktop) => desktop,
            Err(error) => {
                eprintln!("Waiting for display {display}: {error}");
                thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        let mut resize = super::resize::Scaler::new(desktop.width, desktop.height, settings.height);
        let (width, height) = resize.dimensions();
        let mut encoder = Encoder::new(width, height);
        encoder.configure(settings.fps, settings.bitrate);
        let gpu = if allow_gpu {
            match super::gpu::Transform::new(&desktop, &encoder, width, height) {
                Ok(gpu) => Some(gpu),
                Err(error) => {
                    eprintln!("GPU encoder initialization failed, using CPU: {error}");
                    allow_gpu = false;
                    None
                }
            }
        } else {
            None
        };
        let mut pixels = Vec::new();
        let mut first = true;
        *shared.lock().unwrap() = None;
        while !stop.load(Ordering::Relaxed) {
            let before = Instant::now();
            let acquired = if let Some(gpu) = &gpu {
                desktop
                    .with_frame(100, first, |surface| {
                        gpu.dispatch(&desktop.context, surface)
                    })
                    .map(|r| r.is_some())
            } else {
                desktop.next(100, first, &mut pixels)
            };
            match acquired {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    eprintln!("Reconnecting display capture: {error}");
                    *shared.lock().unwrap() = None;
                    thread::sleep(Duration::from_millis(200));
                    break;
                }
            };
            let captured = Instant::now();
            let encoded = if let Some(gpu) = &gpu {
                match gpu.encode(&desktop.context, &encoder) {
                    Ok(frame) => frame,
                    Err(error) => {
                        eprintln!("GPU readback failed, restarting on CPU: {error}");
                        allow_gpu = false;
                        break;
                    }
                }
            } else {
                encoder.frame(resize.frame(&pixels))
            };
            let mut history = shared.lock().unwrap();
            let h = history.get_or_insert_with(|| {
                eprintln!(
                    "ProRes replay active at {}x{} ({}). F5 saves, Ctrl+Alt+F10 quits.",
                    width,
                    height,
                    if gpu.is_some() {
                        "GPU accelerated"
                    } else {
                        "CPU"
                    }
                );
                let mut h =
                    History::new(width as u16, height as u16, settings.fps, settings.seconds);
                h.origin = captured;
                h
            });
            h.push(h.slot(captured), encoded);
            drop(history);
            first = false;
            thread::sleep(
                Duration::from_secs_f64(1.0 / f64::from(settings.fps))
                    .saturating_sub(before.elapsed()),
            );
        }
    }
    Ok(())
}

fn save(frames: Vec<Frame>, width: u16, height: u16, fps: u32, output: &Path) -> io::Result<()> {
    let mut file = media::output_temp(output, "mov")?;
    let mut movie = Movie::new(BufWriter::new(file.as_file_mut()), width, height)?;
    movie.set_fps(fps)?;
    for frame in &frames {
        movie.sample(&frame.bytes)?;
    }
    movie.finish()?;
    file.persist_noclobber(output)?;
    eprintln!(
        "Saved {} ({:.2}s)",
        output.display(),
        frames.len() as f64 / f64::from(fps)
    );
    Ok(())
}

fn join_save(worker: JoinHandle<io::Result<()>>) {
    match worker.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("Save failed: {e}"),
        Err(_) => eprintln!("Save worker panicked"),
    }
}

pub fn run(
    display: u32,
    dir: &Path,
    threads: Option<u32>,
    mut settings: Settings,
) -> io::Result<()> {
    let refresh = super::desktop::refresh_rate(display)?;
    settings.fps = if settings.fps == 0 {
        refresh
    } else {
        settings.fps.min(refresh)
    };
    eprintln!(
        "Display refresh {refresh} Hz. Recording at {} FPS.",
        settings.fps
    );
    std::fs::create_dir_all(dir)?;
    let hotkeys = Hotkeys::prores()?;
    let history = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let worker_history = history.clone();
    let worker_stop = stop.clone();
    let workers = threads.map(|n| n as usize).unwrap_or_else(|| {
        thread::available_parallelism().map_or(1, |n| (n.get() / 2).clamp(1, 6))
    });
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .map_err(io::Error::other)?;
    eprintln!("ProRes encoding uses {workers} CPU workers.");
    let capture = thread::spawn(move || {
        pool.install(|| capture_loop(display, settings, &worker_history, &worker_stop))
    });
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            let action = match line.trim() {
                "" | "save" => Action::Save,
                "q" | "quit" => Action::Quit,
                _ => continue,
            };
            if tx.send(action).is_err() {
                break;
            }
        }
    });
    let mut writers: Vec<JoinHandle<io::Result<()>>> = Vec::new();
    eprintln!(
        "Starting continuous ProRes replay. F5 saves to {}. Ctrl+Alt+F10 stops.",
        dir.display()
    );
    'recording: loop {
        writers = writers
            .into_iter()
            .filter_map(|worker| {
                if worker.is_finished() {
                    join_save(worker);
                    None
                } else {
                    Some(worker)
                }
            })
            .collect();
        if capture.is_finished() {
            break;
        }
        for action in rx.try_iter().chain(std::iter::from_fn(|| hotkeys.poll())) {
            match action {
                Action::Quit => break 'recording,
                Action::Save => {
                    if writers.len() >= 2 {
                        eprintln!("Two clips are being saved. Try F5 again when a save finishes.");
                        continue;
                    }
                    let guard = history.lock().unwrap();
                    let Some(h) = guard.as_ref() else {
                        eprintln!("Waiting for the first encoded frame.");
                        continue;
                    };
                    let frames = h.snapshot(Instant::now());
                    let (width, height, fps) = (h.width, h.height, h.fps);
                    drop(guard);
                    let path = media::clip_path(dir).with_extension("mov");
                    eprintln!(
                        "Saving {:.2}s of ProRes...",
                        frames.len() as f64 / f64::from(fps)
                    );
                    writers.push(thread::spawn(move || {
                        save(frames, width, height, fps, &path)
                    }));
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, Ordering::Relaxed);
    let result = capture
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("capture worker panicked")));
    for worker in writers {
        join_save(worker);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_duration_limits_history() {
        let mut h = History::new(1280, 720, 30, 7);
        h.push(0, vec![1]);
        h.push(899, vec![2]);
        assert_eq!(h.slot(h.origin + Duration::from_secs(30)), 900);
        assert_eq!(h.snapshot(h.origin + Duration::from_secs(30)).len(), 210);
    }
    #[test]
    fn snapshot_keeps_thirty_seconds_and_survives_eviction() {
        let mut h = History::new(16, 16, 60, 30);
        let start = h.origin;
        h.push(0, vec![1; 100]);
        h.push(1200, vec![2; 200]);
        let old = h.snapshot(start + Duration::from_secs(30));
        assert_eq!(h.end, 1201, "saving must not advance the capture timeline");
        assert_eq!(old.len(), 1800);
        assert_eq!(old.last().unwrap().bytes[0], 2);
        h.push(3000, vec![3; 300]);
        assert_eq!(old[0].bytes[0], 1);
        assert_eq!(h.bytes, 500);
        h.push(5000, vec![4; 400]);
        assert_eq!(h.bytes, 700);
        assert_eq!(h.frames.len(), 1800);
    }
}
