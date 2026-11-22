//! DirectSound capture for Windows. cpal has no DirectSound host, so this
//! opens a looping capture buffer directly and polls it from a thread. The
//! blocks it reads go through the same `Pipe` as cpal callbacks, so the
//! analysers, meter and monitor see no difference.

use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::DirectSound::{
    DSCBSTART_LOOPING, DSCBUFFERDESC, DirectSoundCaptureCreate8, DirectSoundCaptureEnumerateW,
    IDirectSoundCapture, IDirectSoundCaptureBuffer,
};
use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
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
/// Away from the buffer this long (a fraction of `BUFFER_SECS`), the
/// hardware may have lapped the read point: resync instead of reading.
const OVERRUN_FRACTION: f32 = 0.75;

/// Names seen by the last `devices()` call, so `open` doesn't enumerate again.
static NAMES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
/// Format each device last accepted, tried first on the next open so a
/// reconnect doesn't probe dozens of formats again.
static LAST_FORMAT: Mutex<Option<HashMap<String, Format>>> = Mutex::new(None);

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
    let list: Vec<DeviceInfo> = found
        .into_iter()
        .map(|(guid, name)| DeviceInfo {
            id: format!("{PREFIX}{}", format_guid(&guid)),
            name: format!("{name} (DirectSound)"),
        })
        .collect();
    if let Ok(mut names) = NAMES.lock() {
        *names = list.iter().map(|d| (d.id.clone(), d.name.clone())).collect();
    }
    list
}

fn cached_name(id: &str) -> Option<String> {
    let names = NAMES.lock().ok()?;
    names.iter().find(|(i, _)| i == id).map(|(_, n)| n.clone())
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
    /// The device object, kept alive as long as its buffer: some drivers
    /// don't hold their own reference to it.
    _device: IDirectSoundCapture,
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
    let name = cached_name(id).unwrap_or_else(|| id.to_owned());

    let last = LAST_FORMAT
        .lock()
        .ok()
        .and_then(|m| m.as_ref()?.get(id).copied())
        .filter(|f| wanted.is_none_or(|r| r == f.rate));
    let rates = wanted.into_iter().chain(FALLBACK_RATES);
    let probe = rates.flat_map(|rate| {
        (1..=MAX_CHANNELS)
            .rev()
            .flat_map(move |channels| KINDS.map(|kind| Format { rate, channels, kind }))
    });
    for format in last.into_iter().chain(probe) {
        if let Some(buffer) = make_buffer(&cap, format) {
            if let Ok(mut m) = LAST_FORMAT.lock() {
                m.get_or_insert_default().insert(id.to_owned(), format);
            }
            return Ok(Opened {
                buffer: SendBuffer(buffer),
                _device: cap,
                format,
                name,
            });
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
/// in [-1, 1]; `on_xrun` when audio was lost (the reader fell a buffer
/// behind); `on_error` once if the buffer stops working, after which the
/// reader exits. A device that silently stops moving its cursor (unplugged,
/// on some drivers) just stops calling `on_block`: the input's stall
/// watchdog catches that.
pub fn start(
    opened: Opened,
    on_block: impl FnMut(&[f32]) + Send + 'static,
    on_xrun: impl Fn() + Send + 'static,
    on_error: impl Fn(String) + Send + Sync + 'static,
) -> Capture {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let owned = SendOpen(opened);
    let on_error = Arc::new(on_error);
    let thread_err = on_error.clone();
    let spawned = thread::Builder::new()
        .name("dsound capture".into())
        .spawn(move || {
            // DirectSound works without it, but other COM calls here would not.
            let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
            let mut on_block = on_block;
            let run = catch_unwind(AssertUnwindSafe(|| {
                reader(owned.into_inner(), &flag, &mut on_block, &on_xrun, &*thread_err)
            }));
            if run.is_err() {
                thread_err("DirectSound capture thread panicked".into());
            }
            if com {
                unsafe { CoUninitialize() };
            }
        });
    let thread = match spawned {
        Ok(t) => Some(t),
        Err(e) => {
            on_error(format!("DirectSound capture thread failed to start: {e}"));
            None
        }
    };
    Capture { stop, thread }
}

fn reader(
    open: Opened,
    stop: &AtomicBool,
    on_block: &mut dyn FnMut(&[f32]),
    on_xrun: &dyn Fn(),
    on_error: &dyn Fn(String),
) {
    let buffer = &open.buffer.0;
    let f = open.format;
    let size = f.buffer_bytes();
    let block = f.block();
    let bytes_per_sec = f.rate as f32 * block as f32;
    if let Err(e) = unsafe { buffer.Start(DSCBSTART_LOOPING) } {
        on_error(format!("DirectSound start failed: {e}"));
        return;
    }
    // Begin at the read cursor, not at stale bytes.
    let mut pos = match unsafe { read_cursor(buffer) } {
        Ok(c) => c - c % block,
        Err(e) => {
            on_error(format!("DirectSound cursor failed: {e}"));
            return;
        }
    };
    let mut last_read = Instant::now();
    let mut samples = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let cur = match unsafe { read_cursor(buffer) } {
            Ok(c) => c,
            Err(e) => {
                on_error(format!("DirectSound cursor failed: {e}"));
                break;
            }
        };
        // Away long enough that the hardware may have lapped us: the bytes
        // between `pos` and the cursor mix two passes. Skip them.
        let away = last_read.elapsed().as_secs_f32();
        if away * bytes_per_sec >= size as f32 * OVERRUN_FRACTION {
            pos = cur - cur % block;
            last_read = Instant::now();
            on_xrun();
            continue;
        }
        // Bytes between our read point and the read cursor, wrapping, in
        // whole frames so the channels never shift.
        let mut lag = (cur + size - pos) % size;
        lag -= lag % block;
        if lag == 0 {
            thread::sleep(POLL);
            continue;
        }
        if let Err(e) = unsafe { read(buffer, pos, lag, f.kind, &mut samples) } {
            on_error(format!("DirectSound read failed: {e}"));
            break;
        }
        pos = (pos + lag) % size;
        last_read = Instant::now();
        on_block(&samples);
    }
    let _ = unsafe { buffer.Stop() };
}

struct SendOpen(Opened);
unsafe impl Send for SendOpen {}

impl SendOpen {
    fn into_inner(self) -> Opened {
        self.0
    }
}

/// The read cursor: everything before it is captured and safe to copy. The
/// capture cursor runs ahead of it, and the bytes between are not valid yet.
unsafe fn read_cursor(buffer: &IDirectSoundCaptureBuffer) -> windows::core::Result<u32> {
    let mut read = 0u32;
    unsafe { buffer.GetCurrentPosition(None, Some(&mut read as *mut u32))? };
    Ok(read)
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
