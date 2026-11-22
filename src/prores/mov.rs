use std::io::{self, Seek, SeekFrom, Write};

// QuickTime video and stereo PCM tracks use independent chunk indexes.
fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend(((payload.len() + 8) as u32).to_be_bytes());
    out.extend(kind);
    out.extend(payload);
    out
}

fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

pub(super) struct Movie<W: Write + Seek> {
    out: W,
    width: u16,
    height: u16,
    sizes: Vec<u32>,
    data_start: u64,
    next_offset: u64,
    video_offsets: Vec<u64>,
    audio_chunks: Vec<(u64, u32)>,
    frame_duration: u32,
    timescale: u32,
}

impl<W: Write + Seek> Movie<W> {
    pub fn new(mut out: W, width: u16, height: u16) -> io::Result<Self> {
        out.write_all(&atom(b"ftyp", b"qt  \0\0\0\0qt  "))?;
        // Extended-size mdat permits sequential frame writes and a single backpatch.
        out.write_all(b"\0\0\0\x01mdat\0\0\0\0\0\0\0\0")?;
        let data_start = out.stream_position()?;
        Ok(Self {
            out,
            width,
            height,
            sizes: Vec::new(),
            data_start,
            next_offset: data_start,
            video_offsets: Vec::new(),
            audio_chunks: Vec::new(),
            frame_duration: 100,
            timescale: 6000,
        })
    }

    pub fn set_fps(&mut self, fps: u32) -> io::Result<()> {
        if fps == 0 || fps > 1000 {
            return Err(io::Error::other("FPS must be between 1 and 1000"));
        }
        self.timescale = if 6000 % fps == 0 { 6000 } else { fps * 100 };
        self.frame_duration = self.timescale / fps;
        Ok(())
    }

    pub fn sample(&mut self, frame: &[u8]) -> io::Result<()> {
        self.out.write_all(frame)?;
        self.video_offsets.push(self.next_offset);
        self.next_offset += frame.len() as u64;
        self.sizes
            .push(u32::try_from(frame.len()).map_err(io::Error::other)?);
        Ok(())
    }

    #[cfg(windows)]
    pub fn audio(&mut self, samples: &[[i16; 2]]) -> io::Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        self.audio_chunks
            .push((self.next_offset, samples.len() as u32));
        // Windows capture runs on little-endian targets; stereo i16 arrays have no padding.
        let bytes = unsafe {
            std::slice::from_raw_parts(samples.as_ptr().cast(), std::mem::size_of_val(samples))
        };
        self.out.write_all(bytes)?;
        self.next_offset += samples.len() as u64 * 4;
        Ok(())
    }

    fn sample_table(&self) -> Vec<u8> {
        let mut entry = vec![0; 6];
        entry.extend(1u16.to_be_bytes()); // data reference
        entry.extend(words(&[0, u32::from_be_bytes(*b"IRrs"), 0, 0]));
        entry.extend(self.width.to_be_bytes());
        entry.extend(self.height.to_be_bytes());
        entry.extend(words(&[72 << 16, 72 << 16, 0]));
        entry.extend(1u16.to_be_bytes());
        let name = b"ProRes 422 HQ";
        let mut pascal = [0; 32];
        pascal[0] = name.len() as u8;
        pascal[1..=name.len()].copy_from_slice(name);
        entry.extend(pascal);
        entry.extend([0, 24, 0xff, 0xff]);
        entry.extend(atom(b"colr", b"nclc\0\x01\0\x01\0\x01"));
        entry.extend(atom(b"fiel", &[1, 0]));
        let mut description = words(&[0, 1]);
        description.extend(atom(b"apch", &entry));
        let count = self.sizes.len() as u32;
        let mut sizes = words(&[0, 0, count]);
        sizes.extend(words(&self.sizes));
        // QuickTime can seek individual samples in a huge chunk but fails to
        // render them continuously. Index each frame independently; the encoded
        // payload and timing stay unchanged, including for files above 4 GiB.
        let mut offsets = words(&[0, count]);
        for offset in &self.video_offsets {
            offsets.extend(offset.to_be_bytes());
        }
        atom(
            b"stbl",
            &[
                atom(b"stsd", &description),
                atom(b"stts", &words(&[0, 1, count, self.frame_duration])),
                atom(b"stsc", &words(&[0, 1, 1, 1, 1])),
                atom(b"stsz", &sizes),
                atom(b"co64", &offsets),
            ]
            .concat(),
        )
    }

    fn audio_track(&self, duration: u32, matrix: &[u8], dref: &[u8]) -> Vec<u8> {
        let count: u32 = self.audio_chunks.iter().map(|(_, n)| n).sum();
        let mut tkhd = words(&[7, 0, 0, 2, 0, duration, 0, 0, 0, 1 << 24]);
        tkhd.extend(matrix);
        tkhd.extend(words(&[0, 0]));
        let mut entry = vec![0; 6];
        entry.extend(1u16.to_be_bytes());
        entry.extend(words(&[0, 0])); // version, revision, vendor
        entry.extend(words(&[(2 << 16) | 16, 0, 48000 << 16]));
        let mut description = words(&[0, 1]);
        description.extend(atom(b"sowt", &entry));
        let mut offsets = words(&[0, self.audio_chunks.len() as u32]);
        let mut runs = Vec::new();
        let mut previous = 0;
        for (i, &(offset, size)) in self.audio_chunks.iter().enumerate() {
            offsets.extend(offset.to_be_bytes());
            if size != previous {
                runs.extend([i as u32 + 1, size, 1]);
                previous = size;
            }
        }
        let mut mapping = words(&[0, (runs.len() / 3) as u32]);
        mapping.extend(words(&runs));
        let stbl = atom(
            b"stbl",
            &[
                atom(b"stsd", &description),
                atom(b"stts", &words(&[0, 1, count, 1])),
                atom(b"stsc", &mapping),
                atom(b"stsz", &words(&[0, 4, count])),
                atom(b"co64", &offsets),
            ]
            .concat(),
        );
        let minf = atom(
            b"minf",
            &[
                atom(b"smhd", &words(&[0, 0])),
                atom(b"hdlr", b"\0\0\0\0dhlralis\0\0\0\0\0\0\0\0\0\0\0\0\x04Data"),
                atom(b"dinf", &atom(b"dref", dref)),
                stbl,
            ]
            .concat(),
        );
        let mdia = atom(
            b"mdia",
            &[
                atom(b"mdhd", &words(&[0, 0, 0, 48000, count, 0])),
                atom(
                    b"hdlr",
                    b"\0\0\0\0mhlrsoun\0\0\0\0\0\0\0\0\0\0\0\0\x05Sound",
                ),
                minf,
            ]
            .concat(),
        );
        atom(b"trak", &[atom(b"tkhd", &tkhd), mdia].concat())
    }

    pub fn finish(mut self) -> io::Result<()> {
        let end = self.out.stream_position()?;
        self.out.seek(SeekFrom::Start(self.data_start - 8))?;
        self.out
            .write_all(&(end - self.data_start + 16).to_be_bytes())?;
        self.out.seek(SeekFrom::Start(end))?;

        let duration = self.sizes.len() as u32 * self.frame_duration;
        let timescale = self.timescale;
        let matrix = words(&[65536, 0, 0, 0, 65536, 0, 0, 0, 1 << 30]);
        let mut mvhd = words(&[0, 0, 0, timescale, duration, 65536, 1 << 24, 0, 0]);
        mvhd.extend(&matrix);
        mvhd.extend(words(&[
            0,
            0,
            0,
            0,
            0,
            0,
            if self.audio_chunks.is_empty() { 2 } else { 3 },
        ]));
        let mut tkhd = words(&[7, 0, 0, 1, 0, duration, 0, 0, 0, 0]);
        tkhd.extend(&matrix);
        tkhd.extend(words(&[
            u32::from(self.width) << 16,
            u32::from(self.height) << 16,
        ]));
        let mdhd = atom(b"mdhd", &words(&[0, 0, 0, timescale, duration, 0]));
        let handler = atom(
            b"hdlr",
            b"\0\0\0\0mhlrvide\0\0\0\0\0\0\0\0\0\0\0\0\x05Video",
        );
        let mut dref = words(&[0, 1]);
        dref.extend(atom(b"alis", &words(&[1])));
        let minf = atom(
            b"minf",
            &[
                atom(b"vmhd", &words(&[1, 0, 0])),
                atom(b"hdlr", b"\0\0\0\0dhlralis\0\0\0\0\0\0\0\0\0\0\0\0\x04Data"),
                atom(b"dinf", &atom(b"dref", &dref)),
                self.sample_table(),
            ]
            .concat(),
        );
        let mdia = atom(b"mdia", &[mdhd, handler, minf].concat());
        let trak = atom(b"trak", &[atom(b"tkhd", &tkhd), mdia].concat());
        let mut tracks = [atom(b"mvhd", &mvhd), trak].concat();
        if !self.audio_chunks.is_empty() {
            tracks.extend(self.audio_track(duration, &matrix, &dref));
        }
        self.out.write_all(&atom(b"moov", &tracks))?;
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn payload<'a>(mut bytes: &'a [u8], path: &[[u8; 4]]) -> &'a [u8] {
        for kind in path {
            loop {
                let size = u32::from_be_bytes(bytes[..4].try_into().unwrap());
                let (size, header) = if size == 1 {
                    (
                        u64::from_be_bytes(bytes[8..16].try_into().unwrap()) as usize,
                        16,
                    )
                } else {
                    (size as usize, 8)
                };
                if bytes[4..8] == *kind {
                    bytes = &bytes[header..size];
                    break;
                }
                bytes = &bytes[size..];
            }
        }
        bytes
    }

    #[test]
    fn high_refresh_rates_keep_exact_timing() {
        for fps in [75, 120, 144, 165, 240, 360, 480] {
            let mut file = Cursor::new(Vec::new());
            let mut movie = Movie::new(&mut file, 16, 16).unwrap();
            movie.set_fps(fps).unwrap();
            for _ in 0..fps {
                movie.sample(b"frame").unwrap();
            }
            movie.finish().unwrap();
            let bytes = file.into_inner();
            let header = payload(&bytes, &[*b"moov", *b"mvhd"]);
            assert_eq!(&header[12..16], &header[16..20]);
            let stts = payload(
                &bytes,
                &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stts"],
            );
            let scale = u32::from_be_bytes(header[12..16].try_into().unwrap());
            assert_eq!(stts, words(&[0, 1, fps, scale / fps]));
        }
    }
    #[test]
    fn thirty_fps_updates_sample_and_movie_duration() {
        let mut file = Cursor::new(Vec::new());
        let mut movie = Movie::new(&mut file, 16, 16).unwrap();
        movie.set_fps(30).unwrap();
        for _ in 0..30 {
            movie.sample(b"frame").unwrap();
        }
        movie.finish().unwrap();
        let bytes = file.into_inner();
        let stts = payload(
            &bytes,
            &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stts"],
        );
        assert_eq!(stts, words(&[0, 1, 30, 200]));
        let header = payload(&bytes, &[*b"moov", *b"mvhd"]);
        assert_eq!(&header[12..20], words(&[6000, 6000]));
    }

    #[test]
    fn replay_frames_have_independent_chunks_and_correct_offsets() {
        let frames: [&[u8]; 3] = [b"first", b"second-frame", b"last"];
        let mut file = Cursor::new(Vec::new());
        let mut movie = Movie::new(&mut file, 1920, 1080).unwrap();
        for frame in frames {
            movie.sample(frame).unwrap();
        }
        movie.finish().unwrap();
        let bytes = file.into_inner();
        let table = payload(&bytes, &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl"]);
        // One sample per chunk prevents QuickTime from stalling on a replay-sized chunk.
        assert_eq!(payload(table, &[*b"stsc"]), words(&[0, 1, 1, 1, 1]));
        let offsets = payload(table, &[*b"co64"]);
        assert_eq!(&offsets[..8], words(&[0, 3]));
        assert_eq!(offsets.len(), 8 + frames.len() * 8);
        let mut expected_offset = 36;
        for (entry, frame) in offsets[8..].as_chunks::<8>().0.iter().zip(frames) {
            let offset = u64::from_be_bytes(*entry) as usize;
            assert_eq!(offset, expected_offset);
            assert_eq!(&bytes[offset..offset + frame.len()], frame);
            expected_offset += frame.len();
        }
        assert_eq!(payload(table, &[*b"stts"]), words(&[0, 1, 3, 100]));
        assert_eq!(payload(table, &[*b"stsz"]), words(&[0, 0, 3, 5, 12, 4]));
    }
}
