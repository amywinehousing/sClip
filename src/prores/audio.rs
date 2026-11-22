use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::Win32::{
    Media::Audio::*,
    System::{Com::*, Performance::*},
};
use windows::core::Result;

pub(super) const RATE: u32 = 48_000;
type Stereo = [i16; 2];

#[derive(Clone)]
struct Packet {
    start: i64,
    samples: Arc<[Stereo]>,
}
impl Packet {
    fn end(&self) -> i64 {
        self.start + self.samples.len() as i64
    }
}
type History = Arc<Mutex<VecDeque<Packet>>>;

pub(super) struct Capture {
    origin: Instant,
    sources: Vec<History>,
    workers: Vec<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

fn ticks(d: Duration) -> i64 {
    (d.as_nanos() * u128::from(RATE) / 1_000_000_000) as i64
}

impl Capture {
    pub fn start(seconds: u32, microphone: bool) -> Self {
        let mut capture = Self {
            origin: Instant::now(),
            sources: Vec::new(),
            workers: Vec::new(),
            stop: Arc::default(),
        };
        for mic in [false, true]
            .into_iter()
            .take(if microphone { 2 } else { 1 })
        {
            let history = History::default();
            capture.sources.push(history.clone());
            let origin = capture.origin;
            let stop = capture.stop.clone();
            capture.workers.push(thread::spawn(move || {
                // COM initialization and the endpoint interfaces belong to this thread.
                if let Err(err) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() } {
                    eprintln!("Audio capture unavailable: {err}");
                    return;
                }
                let mut last_error = String::new();
                while !stop.load(Ordering::Relaxed) {
                    if let Err(err) = capture_device(mic, origin, seconds, &stop, &history) {
                        let error = err.to_string();
                        if last_error != error {
                            let name = if mic { "Microphone" } else { "Desktop audio" };
                            eprintln!("{name} unavailable, retrying: {error}");
                            last_error = error;
                        }
                        for _ in 0..10 {
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
                unsafe { CoUninitialize() };
            }));
        }
        capture
    }

    pub fn snapshot(&self, start: Instant, length: usize) -> Mix {
        let start = ticks(start.saturating_duration_since(self.origin));
        let sources = self
            .sources
            .iter()
            .map(|history| {
                history
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|p| p.start < start + length as i64 && p.end() > start)
                    .cloned()
                    .collect()
            })
            .collect();
        Mix {
            cursor: start,
            sources,
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

pub(super) struct Mix {
    cursor: i64,
    sources: Vec<VecDeque<Packet>>,
}

impl Mix {
    pub fn read(&mut self, output: &mut [Stereo]) {
        output.fill([0, 0]);
        let end = self.cursor + output.len() as i64;
        for packets in &mut self.sources {
            while packets.front().is_some_and(|p| p.end() <= self.cursor) {
                packets.pop_front();
            }
            for packet in packets.iter().take_while(|p| p.start < end) {
                let first = self.cursor.max(packet.start);
                let count = (end.min(packet.end()) - first) as usize;
                let offset = (first - self.cursor) as usize;
                let src = (first - packet.start) as usize;
                for (dst, sample) in output[offset..offset + count]
                    .iter_mut()
                    .zip(&packet.samples[src..])
                {
                    dst[0] = dst[0].saturating_add(sample[0]);
                    dst[1] = dst[1].saturating_add(sample[1]);
                }
            }
        }
        self.cursor = end;
    }
}

#[derive(Default)]
struct PacketClock {
    next: Option<i64>,
    device_end: Option<u64>,
    last_timestamp_end: Option<i64>,
    drift: f64,
}

impl PacketClock {
    fn place(&mut self, count: u32, position: u64, timestamp: i64, flags: u32) -> (i64, usize) {
        let valid = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0;
        let discontinuity = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
        let device_gap = self.device_end.and_then(|end| position.checked_sub(end));
        let last_timestamp = self.last_timestamp_end;
        self.last_timestamp_end = valid.then_some(timestamp + i64::from(count));
        self.device_end = valid.then_some(position.saturating_add(u64::from(count)));

        let next = self.next.unwrap_or(timestamp);
        // Some loopback endpoints stop producing packets when playback is idle.
        // Compare successive timestamps for a pause, not the accumulated clock drift.
        let resumed =
            valid && last_timestamp.is_some_and(|end| timestamp - end > i64::from(RATE / 10));
        let mut length = count as usize;
        let start = if self.next.is_none() || discontinuity || resumed {
            self.drift = 0.0;
            // Preserve small, known losses exactly. Use wall time across device resets/idle.
            let wall_gap = (timestamp - next).max(0);
            let gap = device_gap
                .and_then(|gap| i64::try_from(gap).ok())
                .filter(|gap| {
                    valid
                        && discontinuity
                        && !resumed
                        && gap.abs_diff(wall_gap) <= u64::from(RATE / 100)
                })
                .unwrap_or(wall_gap);
            next + gap
        } else {
            // Append every sample. Device/QPC timestamps share the video time base,
            // but their packet-to-packet jitter must never cut holes in the waveform.
            if valid && count > 2 {
                self.drift += ((timestamp - next) as f64 - self.drift) / 32.0;
                // Correct sustained clock drift gradually, with interpolation.
                let tolerance = f64::from(RATE) * 0.002;
                if self.drift > tolerance {
                    length += 1;
                } else if self.drift < -tolerance {
                    length -= 1;
                }
            }
            next
        };
        self.next = Some(start + length as i64);
        (start, length)
    }
}

// Change a drifting packet's length by one frame while preserving its endpoints.
fn fit_packet(samples: Arc<[Stereo]>, length: usize) -> Arc<[Stereo]> {
    if samples.len() == length {
        return samples;
    }
    let denominator = (length - 1) as i64;
    (0..length)
        .map(|i| {
            let phase = i * (samples.len() - 1);
            let index = phase / (length - 1);
            let fraction = (phase % (length - 1)) as i64;
            let a = samples[index];
            let b = samples[(index + 1).min(samples.len() - 1)];
            std::array::from_fn(|c| {
                ((i64::from(a[c]) * (denominator - fraction) + i64::from(b[c]) * fraction)
                    / denominator) as i16
            })
        })
        .collect()
}

fn capture_device(
    mic: bool,
    origin: Instant,
    seconds: u32,
    stop: &AtomicBool,
    history: &History,
) -> Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let endpoint =
            enumerator.GetDefaultAudioEndpoint(if mic { eCapture } else { eRender }, eConsole)?;
        let client: IAudioClient = endpoint.Activate(CLSCTX_ALL, None)?;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as _,
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
            | if mic { 0 } else { AUDCLNT_STREAMFLAGS_LOOPBACK };
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, &format, None)?;
        let input: IAudioCaptureClient = client.GetService()?;
        let (mut freq, mut counter) = (0, 0);
        QueryPerformanceFrequency(&mut freq)?;
        QueryPerformanceCounter(&mut counter)?;
        let base = ticks(origin.elapsed());
        let qpc_base = i128::from(counter) * 10_000_000 / i128::from(freq);
        client.Start()?;
        let mut clock = PacketClock::default();
        while !stop.load(Ordering::Relaxed) {
            if input.GetNextPacketSize()? == 0 {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            let (mut data, mut count, mut flags, mut stamp) = (std::ptr::null_mut(), 0, 0, 0);
            let mut position = 0;
            input.GetBuffer(
                &mut data,
                &mut count,
                &mut flags,
                Some(&mut position),
                Some(&mut stamp),
            )?;
            if count == 0 {
                continue;
            }
            let samples: Arc<[Stereo]> =
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    vec![[0, 0]; count as usize].into()
                } else {
                    std::slice::from_raw_parts(data.cast::<Stereo>(), count as usize).into()
                };
            input.ReleaseBuffer(count)?;
            let timestamp = if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 {
                base + ((i128::from(stamp) - qpc_base) * i128::from(RATE) / 10_000_000) as i64
            } else {
                ticks(origin.elapsed()) - i64::from(count)
            };
            let (start, output_frames) = clock.place(count, position, timestamp, flags);
            let samples = fit_packet(samples, output_frames);
            let mut packets = history.lock().unwrap();
            while packets
                .front()
                .is_some_and(|p| p.end() < start - i64::from(seconds + 2) * i64::from(RATE))
            {
                packets.pop_front();
            }
            // A discontinuity can overlap a preceding packet. Keep one sample per timestamp.
            let skip = packets
                .back()
                .map_or(0, |last| (last.end() - start).max(0) as usize);
            if skip == 0 {
                packets.push_back(Packet { start, samples });
            } else if skip < samples.len() {
                packets.push_back(Packet {
                    start: start + skip as i64,
                    samples: samples[skip..].into(),
                });
            }
        }
        client.Stop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_device_packets_survive_timestamp_jitter() {
        let mut clock = PacketClock::default();
        for packet in 0..12_000 {
            let expected = 1044 + packet * 480;
            let jitter = if packet % 12 == 7 { 34 } else { 0 };
            assert_eq!(
                clock.place(480, 319375680 + packet as u64 * 480, expected + jitter, 0),
                (expected, 480)
            );
        }
    }

    #[test]
    fn a_real_dropout_preserves_elapsed_time() {
        let mut clock = PacketClock::default();
        assert_eq!(clock.place(480, 0, 100, 0), (100, 480));
        let lost = AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32;
        assert_eq!(clock.place(480, 960, 1064, lost), (1060, 480));
        // An idle render endpoint resumes without necessarily flagging an overrun.
        assert_eq!(clock.place(480, 1440, 49000, 0), (49000, 480));
        let invalid = AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32;
        assert_eq!(clock.place(480, 0, 50100, invalid), (49480, 480));
    }

    #[test]
    fn independent_device_clock_stays_in_sync_without_gaps() {
        let mut clock = PacketClock::default();
        let mut end = 0;
        for n in 0..60000 {
            let timestamp = n * 480 + n / 10;
            let (start, length) = clock.place(480, (n * 480) as u64, timestamp, 0);
            assert_eq!(start, end);
            assert!(timestamp.abs_diff(start) < 120);
            end = start + length as i64;
        }
        let input: Arc<[Stereo]> = Arc::from([[0, 0], [100, -100], [200, -200]]);
        assert_eq!(
            &*fit_packet(input, 4),
            &[[0, 0], [66, -66], [133, -133], [200, -200]]
        );
    }

    #[test]
    fn mixes_overlaps_and_keeps_gaps_across_video_frames() {
        let desktop = Packet {
            start: 5,
            samples: Arc::from([[100, -100]; 3]),
        };
        let microphone = Packet {
            start: 7,
            samples: Arc::from([[32760, -32760]; 2]),
        };
        let mut mix = Mix {
            cursor: 6,
            sources: vec![VecDeque::from([desktop]), VecDeque::from([microphone])],
        };
        let mut first = [[0; 2]; 2];
        mix.read(&mut first);
        assert_eq!(first, [[100, -100], [32767, -32768]]);
        let mut next = [[1; 2]; 2];
        mix.read(&mut next);
        assert_eq!(next, [[32760, -32760], [0, 0]]);
    }
}
