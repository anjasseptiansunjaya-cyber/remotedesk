//! Screen capture: PipeWire node -> GStreamer (scale + VP8 or JPEG) -> frames.
//! The pipeline only runs while at least one viewer is connected.

use std::{process::Stdio, sync::Arc, time::Duration};

use bytes::Bytes;
use tokio::{io::AsyncReadExt, process::Command, sync::watch, time::sleep};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Vp8,
    Jpeg,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Vp8 => "vp8",
            Codec::Jpeg => "jpeg",
        }
    }
}

/// How the Windows host grabs the screen with ffmpeg.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Grab {
    /// DXGI desktop duplication, primary monitor, physical pixels (ffmpeg >= 6.1).
    Dd,
    /// GDI, whole virtual desktop, works on any ffmpeg but is slower.
    Gdi,
}

/// What to capture. On Linux that is a PipeWire node reached through a portal fd.
pub struct Source {
    pub size: (i32, i32),
    #[cfg(target_os = "linux")]
    pub fd: std::os::fd::OwnedFd,
    #[cfg(target_os = "linux")]
    pub node: u32,
}

pub struct Opts {
    pub codec: Codec,
    #[cfg(windows)]
    pub grab: Grab,
    pub fps: u32,
    pub quality: u32,
    pub bitrate_kbps: u32,
    pub max_width: i32,
}

/// What a viewer needs to decode from "now": the last keyframe plus the delta
/// frames after it. Each frame is `[1 = key | 0 = delta][codec data]`.
/// `id` changes whenever a new keyframe starts a new group.
pub struct Gop {
    pub id: u64,
    pub frames: Vec<Bytes>,
}

pub type GopTx = watch::Sender<Arc<Gop>>;

pub fn empty_gop() -> Arc<Gop> {
    Arc::new(Gop { id: 0, frames: Vec::new() })
}

pub async fn run(
    src: Source,
    opts: Opts,
    mut viewers: watch::Receiver<usize>,
    out: GopTx,
) {
    let (w, h) = out_size(src.size, opts.max_width);
    let mut next_id = 1u64;
    loop {
        while *viewers.borrow_and_update() == 0 {
            if viewers.changed().await.is_err() {
                return;
            }
        }
        let mut child = match spawn(&src, w, h, &opts) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("gst-launch gagal dijalankan: {e}");
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let mut stdout = child.stdout.take().unwrap();
        let mut split = Splitter::new(opts.codec);
        let mut chunk = vec![0u8; 256 * 1024];
        let mut gop: Vec<Bytes> = Vec::new();
        loop {
            tokio::select! {
                n = stdout.read(&mut chunk) => match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => split.push(&chunk[..n], |key, data| {
                        let mut f = Vec::with_capacity(data.len() + 1);
                        f.push(key as u8);
                        f.extend_from_slice(&data);
                        if key {
                            gop.clear();
                            next_id += 1;
                        } else if gop.is_empty() {
                            return; // delta without a keyframe: undecodable
                        }
                        gop.push(Bytes::from(f));
                        let _ = out.send(Arc::new(Gop { id: next_id, frames: gop.clone() }));
                    }),
                },
                r = viewers.changed() => {
                    if r.is_err() { let _ = child.kill().await; return; }
                    if *viewers.borrow_and_update() == 0 { break; }
                }
            }
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
        next_id += 1;
        let _ = out.send(Arc::new(Gop { id: next_id, frames: Vec::new() }));
        if *viewers.borrow() > 0 {
            sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Cap the width, keep aspect ratio, encoders want even sizes.
pub fn out_size((w, h): (i32, i32), max_w: i32) -> (i32, i32) {
    let (w, h) = if w > max_w { (max_w, (h as i64 * max_w as i64 / w as i64) as i32) } else { (w, h) };
    (w & !1, h & !1)
}

#[cfg(target_os = "linux")]
fn spawn(src: &Source, w: i32, h: i32, o: &Opts) -> std::io::Result<tokio::process::Child> {
    use std::os::fd::AsRawFd;
    let encode = match o.codec {
        Codec::Jpeg => format!("jpegenc quality={}", o.quality),
        Codec::Vp8 => format!(
            "vp8enc deadline=1 cpu-used=8 end-usage=cbr target-bitrate={} \
             min-quantizer=2 max-quantizer=40 keyframe-max-dist=60 lag-in-frames=0 \
             static-threshold=100 threads=4 ! rtpvp8pay mtu=1400 ! rtpstreampay",
            o.bitrate_kbps as u64 * 1000
        ),
    };
    let pipeline = format!(
        "pipewiresrc fd=3 path={node} do-timestamp=true ! \
         videorate drop-only=true max-rate={fps} ! \
         queue leaky=downstream max-size-buffers=1 ! \
         videoconvert ! videoscale ! video/x-raw,width={w},height={h} ! \
         {encode} ! fdsink fd=1",
        node = src.node,
        fps = o.fps,
    );
    let raw = src.fd.as_raw_fd();
    let mut cmd = Command::new("gst-launch-1.0");
    cmd.arg("-q").args(pipeline.split_whitespace());
    cmd.stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true);
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(raw, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// ffmpeg next to our exe wins over the one on PATH.
#[cfg(windows)]
fn ffmpeg_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("ffmpeg.exe")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| "ffmpeg".into())
}

#[cfg(windows)]
fn ffmpeg_args(w: i32, h: i32, o: &Opts) -> Vec<String> {
    let fps = o.fps;
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"].map(String::from).into();
    match o.grab {
        Grab::Dd => {
            a.extend(["-filter_complex".into(), format!(
                "ddagrab=output_idx=0:framerate={fps}:draw_mouse=1,hwdownload,format=bgra,\
                 scale={w}:{h}:flags=fast_bilinear,format=yuv420p"
            )]);
        }
        Grab::Gdi => {
            a.extend(["-f", "gdigrab", "-framerate"].map(String::from));
            a.push(fps.to_string());
            a.extend(["-draw_mouse", "1", "-i", "desktop", "-vf"].map(String::from));
            a.push(format!("scale={w}:{h}:flags=fast_bilinear,format=yuv420p"));
        }
    }
    match o.codec {
        Codec::Vp8 => {
            let br = format!("{}k", o.bitrate_kbps);
            a.extend(
                [
                    "-c:v", "libvpx", "-deadline", "realtime", "-cpu-used", "8", "-b:v", br.as_str(), "-maxrate", br.as_str(),
                    "-bufsize", br.as_str(), "-qmin", "2", "-qmax", "40", "-g", "60", "-lag-in-frames", "0",
                    "-threads", "4", "-static-thresh", "100", "-flush_packets", "1", "-f", "ivf", "-",
                ]
                .map(String::from),
            );
        }
        Codec::Jpeg => {
            let q = (100u32.saturating_sub(o.quality) * 29 / 100 + 2).to_string(); // mjpeg qscale 2 (best)..31
            a.extend(["-c:v", "mjpeg", "-q:v", q.as_str(), "-flush_packets", "1", "-f", "image2pipe", "-"].map(String::from));
        }
    }
    a
}

#[cfg(windows)]
fn spawn(_src: &Source, w: i32, h: i32, o: &Opts) -> std::io::Result<tokio::process::Child> {
    let mut cmd = Command::new(ffmpeg_path());
    cmd.args(ffmpeg_args(w, h, o));
    cmd.stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true);
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd.spawn()
}

/// Turns the gst stdout byte stream into `(is_key, frame)` callbacks.
enum Splitter {
    Jpeg(JpegSplitter),
    #[cfg_attr(windows, allow(dead_code))]
    Rtp(Vp8Splitter),
    #[cfg_attr(not(windows), allow(dead_code))]
    Ivf(IvfSplitter),
}

impl Splitter {
    fn new(c: Codec) -> Self {
        match c {
            Codec::Jpeg => Splitter::Jpeg(JpegSplitter::default()),
            #[cfg(not(windows))]
            Codec::Vp8 => Splitter::Rtp(Vp8Splitter::default()),
            #[cfg(windows)]
            Codec::Vp8 => Splitter::Ivf(IvfSplitter::default()),
        }
    }

    fn push(&mut self, data: &[u8], emit: impl FnMut(bool, Vec<u8>)) {
        match self {
            Splitter::Jpeg(s) => s.push(data, emit),
            Splitter::Rtp(s) => s.push(data, emit),
            Splitter::Ivf(s) => s.push(data, emit),
        }
    }
}

/// Splits a concatenated stream of JPEGs on SOI (FFD8) / EOI (FFD9).
#[derive(Default)]
struct JpegSplitter {
    buf: Vec<u8>,
    scanned: usize,
}

impl JpegSplitter {
    fn push(&mut self, data: &[u8], mut emit: impl FnMut(bool, Vec<u8>)) {
        self.buf.extend_from_slice(data);
        loop {
            // Drop garbage before the next SOI.
            match find(&self.buf, 0, [0xff, 0xd8]) {
                Some(0) => {}
                Some(i) => {
                    self.buf.drain(..i);
                    self.scanned = 0;
                }
                None => {
                    let keep = self.buf.last().copied() == Some(0xff);
                    let n = self.buf.len() - keep as usize;
                    self.buf.drain(..n);
                    self.scanned = 0;
                    return;
                }
            }
            let from = self.scanned.max(2);
            match find(&self.buf, from, [0xff, 0xd9]) {
                Some(i) => {
                    let frame: Vec<u8> = self.buf.drain(..i + 2).collect();
                    self.scanned = 0;
                    emit(true, frame);
                }
                None => {
                    self.scanned = self.buf.len().saturating_sub(1);
                    return;
                }
            }
        }
    }
}

fn find(buf: &[u8], from: usize, pat: [u8; 2]) -> Option<usize> {
    buf.get(from..)?
        .windows(2)
        .position(|w| w == pat)
        .map(|i| i + from)
}

/// Reassembles VP8 frames from RFC 4571 framed RTP packets (RFC 7741 payload).
#[derive(Default)]
struct Vp8Splitter {
    buf: Vec<u8>,
    frame: Vec<u8>,
}

impl Vp8Splitter {
    fn push(&mut self, data: &[u8], mut emit: impl FnMut(bool, Vec<u8>)) {
        self.buf.extend_from_slice(data);
        let mut pos = 0;
        while self.buf.len() - pos >= 2 {
            let len = u16::from_be_bytes([self.buf[pos], self.buf[pos + 1]]) as usize;
            if self.buf.len() - pos - 2 < len {
                break;
            }
            let pkt = &self.buf[pos + 2..pos + 2 + len];
            pos += 2 + len;
            let Some((marker, payload)) = vp8_payload(pkt) else { continue };
            self.frame.extend_from_slice(payload);
            if marker {
                let frame = std::mem::take(&mut self.frame);
                if !frame.is_empty() {
                    emit(frame[0] & 1 == 0, frame); // bit 0 of the VP8 frame tag: 0 = keyframe
                }
            }
        }
        self.buf.drain(..pos);
    }
}

/// Returns `(marker bit, VP8 payload without RTP header and payload descriptor)`.
fn vp8_payload(p: &[u8]) -> Option<(bool, &[u8])> {
    if p.len() < 13 {
        return None;
    }
    let marker = p[1] & 0x80 != 0;
    let mut off = 12 + 4 * (p[0] & 0x0f) as usize;
    if p[0] & 0x10 != 0 {
        let words = u16::from_be_bytes([*p.get(off + 2)?, *p.get(off + 3)?]) as usize;
        off += 4 + 4 * words;
    }
    let b0 = *p.get(off)?;
    off += 1;
    if b0 & 0x80 != 0 {
        let ext = *p.get(off)?;
        off += 1;
        if ext & 0x80 != 0 {
            off += if *p.get(off)? & 0x80 != 0 { 2 } else { 1 };
        }
        if ext & 0x40 != 0 {
            off += 1;
        }
        if ext & 0x30 != 0 {
            off += 1;
        }
    }
    Some((marker, p.get(off..)?))
}

/// Parses the IVF container ffmpeg writes: a 32-byte file header, then
/// `[u32 size][u64 pts][frame]` records (all little endian).
#[derive(Default)]
struct IvfSplitter {
    buf: Vec<u8>,
    header_done: bool,
}

impl IvfSplitter {
    fn push(&mut self, data: &[u8], mut emit: impl FnMut(bool, Vec<u8>)) {
        self.buf.extend_from_slice(data);
        if !self.header_done {
            if self.buf.len() < 32 {
                return;
            }
            let hdr_len = u16::from_le_bytes([self.buf[6], self.buf[7]]) as usize;
            if self.buf.len() < hdr_len {
                return;
            }
            self.buf.drain(..hdr_len);
            self.header_done = true;
        }
        let mut pos = 0;
        while self.buf.len() - pos >= 12 {
            let size = u32::from_le_bytes(self.buf[pos..pos + 4].try_into().unwrap()) as usize;
            if self.buf.len() - pos - 12 < size {
                break;
            }
            let frame = self.buf[pos + 12..pos + 12 + size].to_vec();
            pos += 12 + size;
            if !frame.is_empty() {
                emit(frame[0] & 1 == 0, frame);
            }
        }
        self.buf.drain(..pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ivf(frames: &[&[u8]]) -> Vec<u8> {
        let mut v = b"DKIF".to_vec();
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&32u16.to_le_bytes());
        v.extend_from_slice(b"VP80");
        v.resize(32, 0);
        for (i, f) in frames.iter().enumerate() {
            v.extend_from_slice(&(f.len() as u32).to_le_bytes());
            v.extend_from_slice(&(i as u64).to_le_bytes());
            v.extend_from_slice(f);
        }
        v
    }

    #[test]
    fn ivf_split_in_odd_chunks() {
        let key = [0x10u8, 1, 2, 3, 4]; // bit 0 clear -> keyframe
        let delta = [0x31u8, 9, 9];
        let data = ivf(&[&key, &delta, &delta]);
        let mut s = IvfSplitter::default();
        let mut got = Vec::new();
        for c in data.chunks(7) {
            s.push(c, |k, f| got.push((k, f)));
        }
        assert_eq!(got, vec![(true, key.to_vec()), (false, delta.to_vec()), (false, delta.to_vec())]);
    }

    #[test]
    fn rtp_vp8_reassembly() {
        // two packets for one keyframe: RTP header (12) + 1-byte descriptor, marker on the last
        let mk = |marker: bool, start: bool, payload: &[u8]| {
            let mut p = vec![0x80, if marker { 0xe0 } else { 0x60 }];
            p.extend_from_slice(&[0; 10]);
            p.push(if start { 0x10 } else { 0x00 });
            p.extend_from_slice(payload);
            let mut f = (p.len() as u16).to_be_bytes().to_vec();
            f.extend(p);
            f
        };
        let mut data = mk(false, true, &[0x10, 1, 2]);
        data.extend(mk(true, false, &[3, 4]));
        let mut s = Vp8Splitter::default();
        let mut got = Vec::new();
        for c in data.chunks(5) {
            s.push(c, |k, f| got.push((k, f)));
        }
        assert_eq!(got, vec![(true, vec![0x10, 1, 2, 3, 4])]);
    }

    #[test]
    fn jpeg_split() {
        let a = [0xff, 0xd8, 1, 2, 0xff, 0xd9];
        let b = [0xff, 0xd8, 7, 0xff, 0xd9];
        let mut s = JpegSplitter::default();
        let mut got = Vec::new();
        for c in [&a[..], &b[..]].concat().chunks(3) {
            s.push(c, |_, f| got.push(f));
        }
        assert_eq!(got, vec![a.to_vec(), b.to_vec()]);
    }
}
