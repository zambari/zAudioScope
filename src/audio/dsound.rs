//! DirectSound capture for Windows. cpal has no DirectSound host, so this
//! opens a looping capture buffer directly and polls it from a thread. The
//! blocks it reads go through the same `Pipe` as cpal callbacks, so the
//! analysers, meter and monitor see no difference.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Media::Audio::DirectSound::{
    DSCBSTART_LOOPING, DSCBUFFERDESC, DirectSoundCaptureCreate8, DirectSoundCaptureEnumerateW,
    IDirectSoundCapture, IDirectSoundCaptureBuffer,
};
use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE};
use windows::core::{BOOL, GUID, IUnknown, PCWSTR};

use super::input::DeviceInfo;

/// Prefix on DirectSound device ids, so they never clash with cpal's ids.
pub const PREFIX: &str = "dsound:";
/// Widest capture we probe for. Interfaces wider than this should use ASIO.
const MAX_CHANNELS: u16 = 8;
/// Capture buffer length, in seconds of audio.
const BUFFER_SECS: u32 = 1;
/// How often the reader checks for new audio.
const POLL: Duration = Duration::from_millis(3);
/// Rates tried after the wanted one, in order.
const FALLBACK_RATES: [u32; 2] = [48_000, 44_100];

/// Sample encodings tried, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    F32,
    I32,
    I24,
    I16,
}

const KINDS: [Kind; 4] = [Kind::F32, Kind::I32, Kind::I24, Kind::I16];

impl Kind {
    fn bytes(self) -> u16 {
        match self {
            Self::F32 | Self::I32 => 4,
            Self::I24 => 3,
            Self::I16 => 2,
        }
    }
}

/// A capture format the device accepted.
#[derive(Clone, Copy, Debug)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
    pub kind: Kind,
}

impl Format {
    /// Bytes per frame.
    fn block(self) -> u32 {
        self.channels as u32 * self.kind.bytes() as u32
    }

    /// Bytes in the capture buffer.
    fn buffer_bytes(self) -> u32 {
        self.rate * self.block() * BUFFER_SECS
    }
}

/// Every DirectSound capture device, as `DeviceInfo`s.
pub fn devices() -> Vec<DeviceInfo> {
    let mut found: Vec<(GUID, String)> = Vec::new();
    // Ignore enumeration failures: the list is just empty.
    let _ = unsafe {
        DirectSoundCaptureEnumerateW(
            Some(enum_cb),
            Some(&mut found as *mut _ as *const c_void),
        )
    };
    found
        .into_iter()
        .map(|(guid, name)| DeviceInfo {
            id: format!("{PREFIX}{}", format_guid(&guid)),
            name: format!("{name} (DirectSound)"),
        })
        .collect()
}

unsafe extern "system" fn enum_cb(
    guid: *mut GUID,
    desc: PCWSTR,
    _module: PCWSTR,
    ctx: *mut c_void,
) -> BOOL {
    // The default device has a null GUID; the cpal list already covers it.
    if !guid.is_null() {
        let found = unsafe { &mut *(ctx as *mut Vec<(GUID, String)>) };
        let name = unsafe { desc.to_string() }.unwrap_or_else(|_| "capture".into());
        found.push((unsafe { *guid }, name));
    }
    BOOL(1)
}

fn format_guid(g: &GUID) -> String {
    let d = g.data4;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    )
}

fn parse_id(id: &str) -> Result<GUID, String> {
    let guid = id.strip_prefix(PREFIX).unwrap_or(id);
    GUID::try_from(guid).map_err(|e| format!("bad DirectSound device id: {e}"))
}

/// A capture buffer the device accepted, not yet running.
pub struct Opened {
    buffer: SendBuffer,
    pub format: Format,
    pub name: String,
}

/// Open a capture on the device `id` at the wanted rate, falling back to
/// standard rates and then fewer channels or narrower samples until the
/// device accepts one.
pub fn open(id: &str, wanted: Option<u32>) -> Result<Opened, String> {
    let guid = parse_id(id)?;
    let mut cap: Option<IDirectSoundCapture> = None;
    unsafe { DirectSoundCaptureCreate8(Some(&guid as *const GUID), &mut cap, None::<&IUnknown>) }
        .map_err(|e| format!("DirectSound open failed: {e}"))?;
    let cap = cap.ok_or("DirectSound returned no capture device")?;
    let name = devices()
        .into_iter()
        .find(|d| d.id == id)
        .map(|d| d.name)
        .unwrap_or_else(|| id.to_owned());

    let rates = wanted.into_iter().chain(FALLBACK_RATES);
    for rate in rates {
        for channels in (1..=MAX_CHANNELS).rev() {
            for kind in KINDS {
                let format = Format { rate, channels, kind };
                if let Some(buffer) = make_buffer(&cap, format) {
                    return Ok(Opened {
                        buffer: SendBuffer(buffer),
                        format,
                        name,
                    });
                }
            }
        }
    }
    Err(format!("{name} accepted no capture format"))
}

fn make_buffer(cap: &IDirectSoundCapture, f: Format) -> Option<IDirectSoundCaptureBuffer> {
    let block = f.block() as u16;
    // WAVEFORMATEXTENSIBLE, which drivers want for more than two channels
    // and for float. The cast to WAVEFORMATEX is what the API expects.
    let mut wfx = WAVEFORMATEXTENSIBLE::default();
    wfx.Format = WAVEFORMATEX {
        wFormatTag: 0xFFFE, // WAVE_FORMAT_EXTENSIBLE
        nChannels: f.channels,
        nSamplesPerSec: f.rate,
        nAvgBytesPerSec: f.rate * f.block(),
        nBlockAlign: block,
        wBitsPerSample: f.kind.bytes() * 8,
        cbSize: 22,
    };
    wfx.Samples.wValidBitsPerSample = f.kind.bytes() * 8;
    wfx.dwChannelMask = (1u32 << f.channels) - 1;
    wfx.SubFormat = match f.kind {
        Kind::F32 => GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71),
        _ => GUID::from_u128(0x0000_0001_0000_0010_8000_00aa_0038_9b71),
    };
    let desc = DSCBUFFERDESC {
        dwSize: std::mem::size_of::<DSCBUFFERDESC>() as u32,
        dwFlags: 0,
        dwBufferBytes: f.buffer_bytes(),
        dwReserved: 0,
        lpwfxFormat: &mut wfx as *mut WAVEFORMATEXTENSIBLE as *mut WAVEFORMATEX,
        dwFXCount: 0,
        lpDSCFXDesc: null_mut(),
    };
    let mut buffer: Option<IDirectSoundCaptureBuffer> = None;
    unsafe { cap.CreateCaptureBuffer(&desc, &mut buffer, None::<&IUnknown>) }.ok()?;
    buffer
}

/// Wrapper so a COM buffer can move to the reader thread. DirectSound
/// buffers are free-threaded and the reader is the only user after `start`.
struct SendBuffer(IDirectSoundCaptureBuffer);
unsafe impl Send for SendBuffer {}

/// A running capture. Dropping it stops the reader thread.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Start reading. `on_block` gets each run of interleaved samples as `f32`
/// in [-1, 1]; `on_error` is called once if the buffer stops working.
pub fn start(
    opened: Opened,
    mut on_block: impl FnMut(&[f32]) + Send + 'static,
    on_error: impl FnOnce(String) + Send + 'static,
) -> Capture {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let owned = SendOpen(opened);
    let thread = thread::spawn(move || {
        let open = owned.into_inner();
        let buffer = open.buffer.0;
        let f = open.format;
        let size = f.buffer_bytes();
        let block = f.block();
        if let Err(e) = unsafe { buffer.Start(DSCBSTART_LOOPING) } {
            on_error(format!("DirectSound start failed: {e}"));
            return;
        }
        // Begin at wherever the hardware cursor is, not at stale bytes.
        let mut pos = match unsafe { cursor(&buffer) } {
            Ok(c) => c,
            Err(e) => {
                on_error(format!("DirectSound cursor failed: {e}"));
                return;
            }
        };
        let mut samples = Vec::new();
        while !flag.load(Ordering::Relaxed) {
            let cap = match unsafe { cursor(&buffer) } {
                Ok(c) => c,
                Err(e) => {
                    on_error(format!("DirectSound cursor failed: {e}"));
                    break;
                }
            };
            // Bytes between our read point and the hardware cursor, wrapping.
            let lag = (cap + size - pos) % size;
            if lag < block {
                thread::sleep(POLL);
                continue;
            }
            if let Err(e) = unsafe { read(&buffer, pos, lag, f.kind, &mut samples) } {
                on_error(format!("DirectSound read failed: {e}"));
                break;
            }
            pos = cap;
            on_block(&samples);
        }
        let _ = unsafe { buffer.Stop() };
    });
    Capture {
        stop,
        thread: Some(thread),
    }
}

struct SendOpen(Opened);
unsafe impl Send for SendOpen {}

impl SendOpen {
    fn into_inner(self) -> Opened {
        self.0
    }
}

unsafe fn cursor(buffer: &IDirectSoundCaptureBuffer) -> windows::core::Result<u32> {
    let mut capture = 0u32;
    unsafe { buffer.GetCurrentPosition(Some(&mut capture as *mut u32), None)? };
    Ok(capture)
}

/// Copy `len` bytes starting at `at` into `out` as `f32`, wrapping at the
/// end of the buffer.
unsafe fn read(
    buffer: &IDirectSoundCaptureBuffer,
    at: u32,
    len: u32,
    kind: Kind,
    out: &mut Vec<f32>,
) -> windows::core::Result<()> {
    let mut p1: *mut c_void = null_mut();
    let mut n1 = 0u32;
    let mut p2: *mut c_void = null_mut();
    let mut n2 = 0u32;
    unsafe {
        buffer.Lock(
            at,
            len,
            &mut p1,
            &mut n1,
            Some(&mut p2 as *mut *mut c_void),
            Some(&mut n2 as *mut u32),
            0,
        )?;
    }
    out.clear();
    unsafe {
        push_samples(std::slice::from_raw_parts(p1 as *const u8, n1 as usize), kind, out);
        if n2 > 0 {
            push_samples(std::slice::from_raw_parts(p2 as *const u8, n2 as usize), kind, out);
        }
        buffer.Unlock(
            p1,
            n1,
            if n2 > 0 { Some(p2 as *const c_void) } else { None },
            n2,
        )?;
    }
    Ok(())
}

fn push_samples(bytes: &[u8], kind: Kind, out: &mut Vec<f32>) {
    match kind {
        Kind::F32 => out.extend(
            bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        ),
        Kind::I32 => out.extend(
            bytes
                .chunks_exact(4)
                .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0),
        ),
        Kind::I24 => out.extend(bytes.chunks_exact(3).map(|b| {
            // Sign-extend the top byte.
            let v = (i32::from(b[2] as i8) << 16) | (i32::from(b[1]) << 8) | i32::from(b[0]);
            v as f32 / 8_388_608.0
        })),
        Kind::I16 => out.extend(
            bytes
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0),
        ),
    }
}
