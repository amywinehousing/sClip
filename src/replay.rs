use std::collections::VecDeque;
use std::io::{self, BufRead, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::media;

const WINDOW: usize = 30;
const BYTE_LIMIT: usize = 128 * 1024 * 1024;
const BOX_LIMIT: usize = 16 * 1024 * 1024;

pub enum Action {
    Save,
    Quit,
}

pub struct Snapshot {
    pub header: Arc<[u8]>,
    pub fragments: Vec<Arc<[u8]>>,
}

#[derive(Default)]
struct History {
    header: Arc<[u8]>,
    fragments: VecDeque<Arc<[u8]>>,
    bytes: usize,
}

impl History {
    fn push(&mut self, fragment: Vec<u8>, count_limit: usize, byte_limit: usize) {
        self.bytes += fragment.len();
        self.fragments.push_back(Arc::from(fragment));
        while self.fragments.len() > count_limit || self.bytes > byte_limit {
            self.bytes -= self.fragments.pop_front().expect("over limit").len();
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            header: self.header.clone(),
            fragments: self.fragments.iter().cloned().collect(),
        }
    }
}

enum Packet {
    Init(Vec<u8>),
    Fragment(Vec<u8>),
}

// The local muxer emits 32-bit boxes. Reject missing, truncated, or unexpectedly large
// boxes before allocating memory; only whole keyframe-aligned fragments enter history.
fn read_box(reader: &mut impl Read, kind: &[u8; 4]) -> io::Result<Vec<u8>> {
    let mut header = [0; 8];
    reader.read_exact(&mut header)?;
    let size = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
    if header[4..] != kind[..] || !(8..=BOX_LIMIT).contains(&size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unexpected MP4 box {:?} of size {size}; expected {:?}",
                String::from_utf8_lossy(&header[4..]),
                String::from_utf8_lossy(kind)
            ),
        ));
    }
    let mut bytes = vec![0; size];
    bytes[..8].copy_from_slice(&header);
    reader.read_exact(&mut bytes[8..])?;
    Ok(bytes)
}

fn pump(mut reader: impl Read, packets: &SyncSender<io::Result<Packet>>) -> io::Result<()> {
    let mut init = read_box(&mut reader, b"ftyp")?;
    init.extend(read_box(&mut reader, b"moov")?);
    if packets.send(Ok(Packet::Init(init))).is_err() {
        return Ok(());
    }
    loop {
        let mut fragment = read_box(&mut reader, b"moof")?;
        fragment.extend(read_box(&mut reader, b"mdat")?);
        if packets.send(Ok(Packet::Fragment(fragment))).is_err() {
            return Ok(());
        }
    }
}

fn finish_save(worker: JoinHandle<io::Result<()>>) {
    match worker.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!("Could not save clip: {error}"),
        Err(_) => eprintln!("Clip writer stopped unexpectedly"),
    }
}

pub fn run(display: u32, dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut child = media::start_replay(display)?;
    let stdout = child.stdout.take().expect("piped output");
    let (tx, rx) = mpsc::sync_channel(2);
    let reader = thread::spawn(move || {
        if let Err(error) = pump(stdout, &tx) {
            let _ = tx.send(Err(error));
        }
    });
    #[cfg(windows)]
    let hotkeys = crate::hotkeys::Hotkeys::register();
    let (control, actions) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            let action = match line.trim() {
                "" | "save" => Action::Save,
                "q" | "quit" => Action::Quit,
                _ => {
                    eprintln!("Press Enter to save or type q to quit.");
                    continue;
                }
            };
            if control.send(action).is_err() {
                break;
            }
        }
    });

    eprintln!(
        "Buffering display {display}. Ctrl+Alt+F9 or Enter saves a clip. Ctrl+Alt+F10 or q then Enter quits."
    );
    let mut history = History::default();
    let mut writers: Vec<JoinHandle<io::Result<()>>> = Vec::new();
    let mut pending = false;
    let mut quit_requested_at = None;
    let result = loop {
        writers = writers
            .into_iter()
            .filter_map(|worker| {
                if worker.is_finished() {
                    finish_save(worker);
                    None
                } else {
                    Some(worker)
                }
            })
            .collect();
        let controls = actions.try_iter();
        #[cfg(windows)]
        let controls = controls.chain(std::iter::from_fn(|| hotkeys.poll()));
        for action in controls {
            match action {
                Action::Quit => {
                    quit_requested_at.get_or_insert_with(Instant::now);
                }
                Action::Save if quit_requested_at.is_none() && !pending && writers.len() < 2 => {
                    pending = true
                }
                Action::Save => eprintln!("Save already queued or busy writing clips."),
            }
        }
        if let Some(requested_at) = quit_requested_at {
            if !pending {
                break Ok(());
            }
            if requested_at.elapsed() >= Duration::from_secs(3) {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "capture stalled before the requested clip could be saved",
                ));
            }
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(Ok(Packet::Init(header))) => history.header = header.into(),
            Ok(Ok(Packet::Fragment(fragment))) => {
                history.push(fragment, WINDOW, BYTE_LIMIT);
                if pending {
                    let snapshot = history.snapshot();
                    let path = media::clip_path(dir);
                    eprintln!("Saving {}s of replay...", snapshot.fragments.len());
                    writers.push(thread::spawn(move || media::save(snapshot, &path)));
                    pending = false;
                }
            }
            Ok(Err(error)) => {
                break Err(io::Error::other(format!(
                    "capture stream stopped: {error}. Check FFmpeg's diagnostic above."
                )));
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                break Err(io::Error::other("capture reader disconnected"));
            }
        }
    };
    drop(rx);
    let _ = child.kill();
    let status = child.wait();
    let _ = reader.join();
    for writer in writers {
        finish_save(writer);
    }
    result.and(status.map(|_| ()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_save_keeps_its_frames_as_history_rolls_over() {
        let mut h = History::default();
        for n in 1..=2 {
            h.push(vec![n; 3], 2, 10);
        }
        let save = h.snapshot();
        h.push(vec![3; 3], 2, 10);
        assert_eq!(h.fragments.front().unwrap()[0], 2);
        h.push(vec![4; 8], 2, 10);
        assert_eq!(h.bytes, 8);
        assert_eq!(h.fragments.len(), 1);
        assert_eq!(
            save.fragments.iter().map(|f| f[0]).collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn truncated_fragment_never_reaches_history() {
        let stream = b"\0\0\0\x08ftyp\0\0\0\x08moov\0\0\0\x08moof\0\0\0\x0cmdatab";
        let (tx, rx) = mpsc::sync_channel(2);
        assert_eq!(
            pump(&stream[..], &tx).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(matches!(rx.try_recv(), Ok(Ok(Packet::Init(_)))));
        assert!(rx.try_recv().is_err());
    }
}
