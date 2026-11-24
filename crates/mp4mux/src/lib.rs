//! Minimal MP4 (ISO BMFF) muxer: one H.264 track + optional AAC-LC track.
//!
//! Samples are streamed into `mdat` as they arrive (constant memory); the `moov` index is written
//! on [`Mp4Writer::finish`], so a file is only playable once finished. Video resolution changes
//! (device rotation) are supported by adding a new sample description per distinct SPS/PPS.
//! No B-frames: composition time equals decode time.

use std::io::{self, Seek, SeekFrom, Write};

const VIDEO_TIMESCALE: u32 = 90_000;
const AAC_FRAME: u32 = 1024;
/// AAC frames grouped into one chunk (~0.5 s at 48 kHz).
const AUDIO_CHUNK_FRAMES: usize = 24;

struct VideoDesc {
    width: u16,
    height: u16,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

struct VideoSample {
    offset: u64,
    size: u32,
    key: bool,
    pts_us: u64,
    desc: u32, // 1-based
}

struct AudioChunk {
    offset: u64,
    frames: u32,
}

pub struct Mp4Writer<W: Write + Seek> {
    w: W,
    pos: u64,
    mdat_start: u64,
    descs: Vec<VideoDesc>,
    cur_desc: Option<u32>,
    vsamples: Vec<VideoSample>,
    audio: Option<(u32, u8)>, // sample rate, channels
    asizes: Vec<u32>,
    achunks: Vec<AudioChunk>,
    abuf: Vec<Vec<u8>>,
}

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

/// Wrap `payload` in a box.
fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend(be32(8 + payload.len() as u32));
    v.extend(kind);
    v.extend(payload);
    v
}

fn full(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![version];
    p.extend(&flags.to_be_bytes()[1..]);
    p.extend(payload);
    bx(kind, &p)
}

fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

/// Split an Annex-B buffer into NAL units (without start codes).
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = vec![];
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = vec![];
    for (n, &s) in starts.iter().enumerate() {
        let mut e = starts.get(n + 1).map(|&x| x - 3).unwrap_or(data.len());
        while e > s && data[e - 1] == 0 {
            e -= 1; // trailing zero of a 4-byte start code / padding
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

/// Extract (SPS, PPS) from Annex-B parameter sets.
pub fn parameter_sets(annexb: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (mut sps, mut pps) = (None, None);
    for n in split_annexb(annexb) {
        match n[0] & 0x1f {
            7 if sps.is_none() => sps = Some(n.to_vec()),
            8 if pps.is_none() => pps = Some(n.to_vec()),
            _ => {}
        }
    }
    Some((sps?, pps?))
}

/// Annex-B access unit -> AVCC (4-byte length prefixed), dropping SPS/PPS/AUD/SEI-less noise.
pub fn annexb_to_avcc(au: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len());
    for n in split_annexb(au) {
        if matches!(n[0] & 0x1f, 7 | 8 | 9) {
            continue;
        }
        out.extend(be32(n.len() as u32));
        out.extend(n);
    }
    out
}

/// Like [`annexb_to_avcc`] but keeps SPS/PPS inside the sample. Use for keyframes so a decoder can
/// re-initialise at an IDR when the parameter sets change mid-track (players that only read the
/// first sample entry otherwise decode later segments with the wrong parameters).
pub fn annexb_to_avcc_in_band(au: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len());
    for n in split_annexb(au) {
        if (n[0] & 0x1f) == 9 {
            continue;
        }
        out.extend(be32(n.len() as u32));
        out.extend(n);
    }
    out
}

impl<W: Write + Seek> Mp4Writer<W> {
    pub fn new(mut w: W) -> io::Result<Self> {
        let ftyp = bx(b"ftyp", &cat(&[b"isom".to_vec(), be32(512).to_vec(), b"isomiso2avc1mp41".to_vec()]));
        w.write_all(&ftyp)?;
        let mdat_start = ftyp.len() as u64;
        // 64-bit mdat: size==1, then 'mdat', then u64 largesize (patched in finish()).
        w.write_all(&be32(1))?;
        w.write_all(b"mdat")?;
        w.write_all(&0u64.to_be_bytes())?;
        Ok(Self {
            w,
            pos: mdat_start + 16,
            mdat_start,
            descs: vec![],
            cur_desc: None,
            vsamples: vec![],
            audio: None,
            asizes: vec![],
            achunks: vec![],
            abuf: vec![],
        })
    }

    /// Declare (or change) the video parameters. Identical SPS/PPS/size reuse the same description.
    pub fn set_video_config(&mut self, width: u32, height: u32, sps: Vec<u8>, pps: Vec<u8>) {
        let idx = self
            .descs
            .iter()
            .position(|d| d.sps == sps && d.pps == pps && d.width as u32 == width && d.height as u32 == height)
            .unwrap_or_else(|| {
                self.descs.push(VideoDesc { width: width as u16, height: height as u16, sps, pps });
                self.descs.len() - 1
            });
        self.cur_desc = Some(idx as u32 + 1);
    }

    pub fn set_audio_config(&mut self, sample_rate: u32, channels: u8) {
        self.audio.get_or_insert((sample_rate, channels));
    }

    pub fn has_video_config(&self) -> bool {
        self.cur_desc.is_some()
    }

    pub fn video_samples(&self) -> usize {
        self.vsamples.len()
    }

    /// `avcc` is one access unit in length-prefixed form (see [`annexb_to_avcc`]).
    pub fn add_video_sample(&mut self, avcc: &[u8], key: bool, pts_us: u64) -> io::Result<()> {
        let desc = self.cur_desc.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no video config"))?;
        if avcc.is_empty() {
            return Ok(());
        }
        self.w.write_all(avcc)?;
        self.vsamples.push(VideoSample { offset: self.pos, size: avcc.len() as u32, key, pts_us, desc });
        self.pos += avcc.len() as u64;
        Ok(())
    }

    /// One raw AAC-LC access unit (no ADTS header) = 1024 samples per channel.
    pub fn add_audio_frame(&mut self, aac: &[u8]) -> io::Result<()> {
        self.abuf.push(aac.to_vec());
        if self.abuf.len() >= AUDIO_CHUNK_FRAMES {
            self.flush_audio()?;
        }
        Ok(())
    }

    fn flush_audio(&mut self) -> io::Result<()> {
        if self.abuf.is_empty() {
            return Ok(());
        }
        self.achunks.push(AudioChunk { offset: self.pos, frames: self.abuf.len() as u32 });
        for f in std::mem::take(&mut self.abuf) {
            self.w.write_all(&f)?;
            self.pos += f.len() as u64;
            self.asizes.push(f.len() as u32);
        }
        Ok(())
    }

    /// Finalise the file and return the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.flush_audio()?;
        let moov = self.build_moov();
        self.w.write_all(&moov)?;
        let mdat_len = self.pos - self.mdat_start;
        self.w.seek(SeekFrom::Start(self.mdat_start + 8))?;
        self.w.write_all(&mdat_len.to_be_bytes())?;
        self.w.seek(SeekFrom::End(0))?;
        self.w.flush()?;
        Ok(self.w)
    }

    // ---- moov ---------------------------------------------------------------------------------

    fn video_ticks(&self) -> Vec<u32> {
        let t = |us: u64| (us * VIDEO_TIMESCALE as u64 / 1_000_000) as i64;
        let n = self.vsamples.len();
        let mut d: Vec<u32> = (0..n)
            .map(|i| {
                if i + 1 < n {
                    (t(self.vsamples[i + 1].pts_us) - t(self.vsamples[i].pts_us)).max(1) as u32
                } else {
                    0
                }
            })
            .collect();
        if n > 0 {
            d[n - 1] = if n > 1 { d[n - 2] } else { VIDEO_TIMESCALE / 30 };
        }
        d
    }

    fn build_moov(&self) -> Vec<u8> {
        let vdur = self.video_ticks();
        let video_total_ms: u64 = vdur.iter().map(|&d| d as u64).sum::<u64>() * 1000 / VIDEO_TIMESCALE as u64;
        let audio_total_ms: u64 = self.audio.map(|(r, _)| self.asizes.len() as u64 * AAC_FRAME as u64 * 1000 / r as u64).unwrap_or(0);
        let movie_ms = video_total_ms.max(audio_total_ms) as u32;
        let next_track = 1 + (!self.vsamples.is_empty()) as u32 + self.audio.is_some() as u32;

        let mut mvhd = vec![];
        mvhd.extend(be32(0)); // creation
        mvhd.extend(be32(0)); // modification
        mvhd.extend(be32(1000));
        mvhd.extend(be32(movie_ms));
        mvhd.extend(be32(0x0001_0000)); // rate
        mvhd.extend([0x01, 0x00]); // volume
        mvhd.extend([0u8; 10]);
        mvhd.extend(MATRIX);
        mvhd.extend([0u8; 24]);
        mvhd.extend(be32(next_track));

        let mut parts = vec![full(b"mvhd", 0, 0, &mvhd)];
        if !self.vsamples.is_empty() {
            parts.push(self.video_trak(&vdur, video_total_ms as u32));
        }
        if self.audio.is_some() && !self.asizes.is_empty() {
            parts.push(self.audio_trak(audio_total_ms as u32));
        }
        bx(b"moov", &cat(&parts))
    }

    fn tkhd(id: u32, dur_ms: u32, audio: bool, w: u32, h: u32) -> Vec<u8> {
        let mut t = vec![];
        t.extend(be32(0));
        t.extend(be32(0));
        t.extend(be32(id));
        t.extend(be32(0));
        t.extend(be32(dur_ms));
        t.extend([0u8; 8]);
        t.extend([0, 0, 0, 0]); // layer, alternate group
        t.extend(if audio { [0x01, 0x00] } else { [0, 0] });
        t.extend([0, 0]);
        t.extend(MATRIX);
        t.extend(be32(w << 16));
        t.extend(be32(h << 16));
        full(b"tkhd", 0, 7, &t)
    }

    fn mdhd(timescale: u32, dur: u32) -> Vec<u8> {
        let mut m = vec![];
        m.extend(be32(0));
        m.extend(be32(0));
        m.extend(be32(timescale));
        m.extend(be32(dur));
        m.extend([0x55, 0xc4, 0, 0]); // language 'und'
        full(b"mdhd", 0, 0, &m)
    }

    fn hdlr(kind: &[u8; 4], name: &str) -> Vec<u8> {
        let mut h = vec![0u8; 4];
        h.extend(kind);
        h.extend([0u8; 12]);
        h.extend(name.as_bytes());
        h.push(0);
        full(b"hdlr", 0, 0, &h)
    }

    fn dinf() -> Vec<u8> {
        let url = full(b"url ", 0, 1, &[]);
        let dref = full(b"dref", 0, 0, &cat(&[be32(1).to_vec(), url]));
        bx(b"dinf", &dref)
    }

    fn stts(runs: &[(u32, u32)]) -> Vec<u8> {
        let mut p = be32(runs.len() as u32).to_vec();
        for (c, d) in runs {
            p.extend(be32(*c));
            p.extend(be32(*d));
        }
        full(b"stts", 0, 0, &p)
    }

    fn co64(offsets: &[u64]) -> Vec<u8> {
        let mut p = be32(offsets.len() as u32).to_vec();
        for o in offsets {
            p.extend(o.to_be_bytes());
        }
        full(b"co64", 0, 0, &p)
    }

    fn video_trak(&self, durs: &[u32], dur_ms: u32) -> Vec<u8> {
        let (w, h) = (self.descs[0].width as u32, self.descs[0].height as u32);
        // stsd: one avc1 entry per description
        let mut entries = vec![];
        for d in &self.descs {
            let mut avcc = vec![1, d.sps[1], d.sps[2], d.sps[3], 0xff, 0xe1];
            avcc.extend((d.sps.len() as u16).to_be_bytes());
            avcc.extend(&d.sps);
            avcc.push(1);
            avcc.extend((d.pps.len() as u16).to_be_bytes());
            avcc.extend(&d.pps);
            let mut e = vec![0u8; 6];
            e.extend([0, 1]); // data reference index
            e.extend([0u8; 16]);
            e.extend(d.width.to_be_bytes());
            e.extend(d.height.to_be_bytes());
            e.extend(be32(0x0048_0000)); // 72 dpi
            e.extend(be32(0x0048_0000));
            e.extend(be32(0));
            e.extend([0, 1]); // frame count
            e.extend([0u8; 32]); // compressor name
            e.extend([0, 0x18, 0xff, 0xff]); // depth, pre_defined -1
            e.extend(bx(b"avcC", &avcc));
            entries.push(bx(b"avc1", &e));
        }
        let stsd = full(b"stsd", 0, 0, &cat(&[be32(entries.len() as u32).to_vec(), cat(&entries)]));

        // stts (run-length)
        let mut runs: Vec<(u32, u32)> = vec![];
        for &d in durs {
            match runs.last_mut() {
                Some((c, rd)) if *rd == d => *c += 1,
                _ => runs.push((1, d)),
            }
        }
        // stss
        let keys: Vec<u32> = self.vsamples.iter().enumerate().filter(|(_, s)| s.key).map(|(i, _)| i as u32 + 1).collect();
        let mut stss = be32(keys.len() as u32).to_vec();
        for k in keys {
            stss.extend(be32(k));
        }
        // stsc: one sample per chunk; entry on each description change
        let mut stsc = vec![];
        let mut n = 0u32;
        let mut last_desc = 0;
        for (i, s) in self.vsamples.iter().enumerate() {
            if s.desc != last_desc {
                stsc.extend(be32(i as u32 + 1));
                stsc.extend(be32(1));
                stsc.extend(be32(s.desc));
                n += 1;
                last_desc = s.desc;
            }
        }
        let stsc = full(b"stsc", 0, 0, &cat(&[be32(n).to_vec(), stsc]));
        // stsz
        let mut stsz = cat(&[be32(0).to_vec(), be32(self.vsamples.len() as u32).to_vec()]);
        for s in &self.vsamples {
            stsz.extend(be32(s.size));
        }
        let offs: Vec<u64> = self.vsamples.iter().map(|s| s.offset).collect();
        let total: u64 = durs.iter().map(|&d| d as u64).sum();

        let stbl = bx(
            b"stbl",
            &cat(&[stsd, Self::stts(&runs), full(b"stss", 0, 0, &stss), stsc, full(b"stsz", 0, 0, &stsz), Self::co64(&offs)]),
        );
        let minf = bx(b"minf", &cat(&[full(b"vmhd", 0, 1, &[0u8; 8]), Self::dinf(), stbl]));
        let mdia = bx(b"mdia", &cat(&[Self::mdhd(VIDEO_TIMESCALE, total as u32), Self::hdlr(b"vide", "VideoHandler"), minf]));
        bx(b"trak", &cat(&[Self::tkhd(1, dur_ms, false, w, h), mdia]))
    }

    fn audio_trak(&self, dur_ms: u32) -> Vec<u8> {
        let (rate, ch) = self.audio.unwrap();
        let sf_index = [96000u32, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350]
            .iter()
            .position(|&r| r == rate)
            .unwrap_or(3) as u16;
        // AudioSpecificConfig: objectType 2 (AAC-LC) | freq index | channel config
        let asc: u16 = (2 << 11) | (sf_index << 7) | ((ch as u16) << 3);
        let asc = asc.to_be_bytes();

        let dec_specific = [vec![0x05, 0x02], asc.to_vec()].concat();
        let mut dec_cfg = vec![0x40, 0x15, 0, 0, 0]; // objectType AAC, streamType audio, bufferSize
        dec_cfg.extend(be32(0)); // maxBitrate
        dec_cfg.extend(be32(0)); // avgBitrate
        dec_cfg.extend(&dec_specific);
        let dec_cfg = [vec![0x04, dec_cfg.len() as u8], dec_cfg].concat();
        let sl = vec![0x06, 0x01, 0x02];
        let es_body = [vec![0, 1, 0], dec_cfg, sl].concat();
        let es = [vec![0x03, es_body.len() as u8], es_body].concat();
        let esds = full(b"esds", 0, 0, &es);

        let mut e = vec![0u8; 6];
        e.extend([0, 1]);
        e.extend([0u8; 8]);
        e.extend((ch as u16).to_be_bytes());
        e.extend([0, 16]); // sample size
        e.extend([0, 0, 0, 0]);
        e.extend(be32(rate << 16));
        e.extend(esds);
        let mp4a = bx(b"mp4a", &e);
        let stsd = full(b"stsd", 0, 0, &cat(&[be32(1).to_vec(), mp4a]));

        let n = self.asizes.len() as u32;
        let stts = Self::stts(&[(n, AAC_FRAME)]);
        // stsc: runs of equal chunk sizes
        let mut stsc_entries = vec![];
        let mut cnt = 0u32;
        let mut last = 0u32;
        for (i, c) in self.achunks.iter().enumerate() {
            if c.frames != last {
                stsc_entries.extend(be32(i as u32 + 1));
                stsc_entries.extend(be32(c.frames));
                stsc_entries.extend(be32(1));
                cnt += 1;
                last = c.frames;
            }
        }
        let stsc = full(b"stsc", 0, 0, &cat(&[be32(cnt).to_vec(), stsc_entries]));
        let mut stsz = cat(&[be32(0).to_vec(), be32(n).to_vec()]);
        for s in &self.asizes {
            stsz.extend(be32(*s));
        }
        let offs: Vec<u64> = self.achunks.iter().map(|c| c.offset).collect();
        let stbl = bx(b"stbl", &cat(&[stsd, stts, stsc, full(b"stsz", 0, 0, &stsz), Self::co64(&offs)]));
        let minf = bx(b"minf", &cat(&[full(b"smhd", 0, 0, &[0u8; 4]), Self::dinf(), stbl]));
        let mdia = bx(b"mdia", &cat(&[Self::mdhd(rate, n * AAC_FRAME), Self::hdlr(b"soun", "SoundHandler"), minf]));
        bx(b"trak", &cat(&[Self::tkhd(2, dur_ms, true, 0, 0), mdia]))
    }
}

const MATRIX: [u8; 36] = [
    0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0, 0, 0,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Minimal box walker for assertions.
    fn find<'a>(data: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
        let mut cur = data;
        'outer: for kind in path {
            let mut i = 0;
            while i + 8 <= cur.len() {
                let mut size = u32::from_be_bytes(cur[i..i + 4].try_into().unwrap()) as usize;
                let mut hdr = 8;
                if size == 1 {
                    size = u64::from_be_bytes(cur[i + 8..i + 16].try_into().unwrap()) as usize;
                    hdr = 16;
                }
                if &cur[i + 4..i + 8] == *kind {
                    cur = &cur[i + hdr..i + size];
                    continue 'outer;
                }
                if size < 8 {
                    return None;
                }
                i += size;
            }
            return None;
        }
        Some(cur)
    }

    const SPS: [u8; 4] = [0x67, 0x42, 0xc0, 0x1f];
    const PPS: [u8; 2] = [0x68, 0xce];

    #[test]
    fn annexb_helpers() {
        let au = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9, 9, 9];
        let (s, p) = parameter_sets(&au).unwrap();
        assert_eq!(s, [0x67, 1, 2]);
        assert_eq!(p, [0x68, 3]);
        assert_eq!(annexb_to_avcc(&au), [0, 0, 0, 4, 0x65, 9, 9, 9]);
    }

    #[test]
    fn in_band_keeps_parameter_sets() {
        let au = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9];
        assert_eq!(annexb_to_avcc_in_band(&au), [0, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 2, 0x68, 3, 0, 0, 0, 2, 0x65, 9]);
    }

    #[test]
    fn writes_valid_structure_with_resolution_change() {
        let mut m = Mp4Writer::new(Cursor::new(Vec::new())).unwrap();
        m.set_audio_config(48_000, 2);
        m.set_video_config(1080, 1920, SPS.to_vec(), PPS.to_vec());
        for i in 0..5u64 {
            m.add_video_sample(&[0, 0, 0, 2, 0x65, i as u8], i == 0, i * 33_333).unwrap();
        }
        // rotation: new description
        m.set_video_config(1920, 1080, vec![0x67, 0x42, 0xc0, 0x28], PPS.to_vec());
        for i in 5..8u64 {
            m.add_video_sample(&[0, 0, 0, 2, 0x65, i as u8], i == 5, i * 33_333).unwrap();
        }
        for _ in 0..30 {
            m.add_audio_frame(&[0x21, 0x00, 0x49]).unwrap();
        }
        let file = m.finish().unwrap().into_inner();

        assert_eq!(&file[4..8], b"ftyp");
        // mdat largesize covers everything between its start and moov
        let moov = find(&file, &[b"moov"]).unwrap();
        let mdat = find(&file, &[b"mdat"]).unwrap();
        assert_eq!(mdat.len(), 8 * 6 + 30 * 3);
        assert!(moov.len() > 100);
        let v = find(moov, &[b"trak", b"mdia", b"minf", b"stbl"]).unwrap();
        let stsd = find(v, &[b"stsd"]).unwrap();
        assert_eq!(u32::from_be_bytes(stsd[4..8].try_into().unwrap()), 2, "two avc1 descriptions");
        let stsz = find(v, &[b"stsz"]).unwrap();
        assert_eq!(u32::from_be_bytes(stsz[8..12].try_into().unwrap()), 8, "8 video samples");
        let stss = find(v, &[b"stss"]).unwrap();
        assert_eq!(u32::from_be_bytes(stss[4..8].try_into().unwrap()), 2, "two keyframes");
        let stsc = find(v, &[b"stsc"]).unwrap();
        assert_eq!(u32::from_be_bytes(stsc[4..8].try_into().unwrap()), 2, "stsc switches description once");
    }

    #[test]
    fn sample_offsets_point_at_sample_bytes() {
        let mut m = Mp4Writer::new(Cursor::new(Vec::new())).unwrap();
        m.set_video_config(64, 64, SPS.to_vec(), PPS.to_vec());
        m.add_video_sample(&[0, 0, 0, 3, 0x65, 0xaa, 0xbb], true, 0).unwrap();
        m.add_video_sample(&[0, 0, 0, 3, 0x41, 0xcc, 0xdd], false, 40_000).unwrap();
        let file = m.finish().unwrap().into_inner();
        let moov = find(&file, &[b"moov"]).unwrap();
        let stbl = find(moov, &[b"trak", b"mdia", b"minf", b"stbl"]).unwrap();
        let co = find(stbl, &[b"co64"]).unwrap();
        let off1 = u64::from_be_bytes(co[8 + 8..8 + 16].try_into().unwrap()) as usize;
        assert_eq!(&file[off1..off1 + 7], &[0, 0, 0, 3, 0x41, 0xcc, 0xdd]);
    }
}
