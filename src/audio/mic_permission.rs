//! macOS microphone permission (TCC).
//!
//! When access is missing, CoreAudio still opens input streams without an
//! error, it just delivers silence. So we ask AVFoundation for the status and
//! tell the user. TCC charges the permission to the *responsible* app: for a
//! binary started from a shell that's the terminal (Terminal, iTerm, an IDE),
//! not this program, so we name that app too.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicAuth {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
}

#[cfg(target_os = "macos")]
mod imp {
    use super::MicAuth;
    use std::ffi::{c_char, c_int, c_void};
    use std::sync::atomic::{AtomicU8, Ordering};

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        static AVMediaTypeAudio: *const c_void;
    }

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    #[link(name = "Foundation", kind = "framework")]
    unsafe extern "C" {
        static _NSConcreteGlobalBlock: c_void;
    }

    unsafe extern "C" {
        fn responsibility_get_pid_responsible_for_pid(pid: c_int) -> c_int;
        fn proc_pidpath(pid: c_int, buf: *mut c_void, size: u32) -> c_int;
    }

    fn av_capture_device() -> *mut c_void {
        unsafe { objc_getClass(c"AVCaptureDevice".as_ptr()) }
    }

    pub fn status() -> MicAuth {
        let cls = av_capture_device();
        if cls.is_null() {
            return MicAuth::Authorized;
        }
        let send: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void) -> isize =
            unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        let v = unsafe {
            send(cls, sel_registerName(c"authorizationStatusForMediaType:".as_ptr()), AVMediaTypeAudio)
        };
        match v {
            0 => MicAuth::NotDetermined,
            1 => MicAuth::Restricted,
            2 => MicAuth::Denied,
            _ => MicAuth::Authorized,
        }
    }

    // A global (static, never copied) Objective-C block for the completion
    // handler, so we need no block runtime crate.
    #[repr(C)]
    struct BlockDescriptor {
        reserved: usize,
        size: usize,
    }
    #[repr(C)]
    struct Block {
        isa: *const c_void,
        flags: i32,
        reserved: i32,
        invoke: unsafe extern "C" fn(*const Block, bool),
        descriptor: *const BlockDescriptor,
    }
    unsafe impl Sync for Block {}

    const BLOCK_IS_GLOBAL: i32 = 1 << 28;
    static DESCRIPTOR: BlockDescriptor = BlockDescriptor { reserved: 0, size: size_of::<Block>() };

    /// 0: no answer yet, 1: granted, 2: refused.
    static ANSWER: AtomicU8 = AtomicU8::new(0);

    unsafe extern "C" fn on_answer(_: *const Block, granted: bool) {
        ANSWER.store(if granted { 1 } else { 2 }, Ordering::Relaxed);
    }

    /// Show the system prompt (only does anything while `NotDetermined`).
    pub fn request() {
        let cls = av_capture_device();
        if cls.is_null() {
            return;
        }
        // Built at call time: a static can't take the address of an extern static.
        let block = Box::leak(Box::new(Block {
            isa: &raw const _NSConcreteGlobalBlock,
            flags: BLOCK_IS_GLOBAL,
            reserved: 0,
            invoke: on_answer,
            descriptor: &DESCRIPTOR,
        }));
        let send: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void, *const Block) =
            unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        unsafe {
            send(
                cls,
                sel_registerName(c"requestAccessForMediaType:completionHandler:".as_ptr()),
                AVMediaTypeAudio,
                block,
            )
        };
    }

    /// The app macOS holds responsible for this process, e.g. "Terminal".
    pub fn responsible_app() -> Option<String> {
        let pid = unsafe { responsibility_get_pid_responsible_for_pid(std::process::id() as c_int) };
        if pid <= 0 {
            return None;
        }
        let mut buf = [0u8; 4096];
        let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if n <= 0 {
            return None;
        }
        let path = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
        // ".../Terminal.app/Contents/MacOS/Terminal" -> "Terminal"
        let app = path.split('/').find(|c| c.ends_with(".app")).map(|c| c.trim_end_matches(".app"));
        Some(app.unwrap_or_else(|| path.rsplit('/').next().unwrap_or(&path)).to_owned())
    }

    pub fn open_settings() {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
            .spawn();
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::MicAuth;
    pub fn status() -> MicAuth {
        MicAuth::Authorized
    }
    pub fn request() {}
    pub fn responsible_app() -> Option<String> {
        None
    }
    pub fn open_settings() {}
}

pub use imp::{open_settings, request, responsible_app, status};
