//! Audio endpoint changes: a counter that moves whenever a device is added,
//! removed, enabled, disabled or becomes the default. The input and monitor
//! compare it with the value they last saw, to retry a lost device the
//! moment it is back, and the app refreshes its device lists on a change.
//!
//! On Windows the counter is fed by an `IMMNotificationClient`. Its
//! callbacks run on a thread of the audio service; they only bump the
//! counter. Opening or closing a stream from inside one can deadlock.
//! Elsewhere the counter never moves and lost devices are retried on a timer.

use std::sync::atomic::{AtomicU64, Ordering};

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Changes seen so far. Starts the watcher on first use.
pub fn generation() -> u64 {
    #[cfg(windows)]
    win::ensure_started();
    GENERATION.load(Ordering::Relaxed)
}

/// Whether device changes are reported at all (otherwise poll).
pub fn watched() -> bool {
    #[cfg(windows)]
    return win::ensure_started();
    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
mod win {
    use std::sync::OnceLock;
    use std::sync::atomic::Ordering;

    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
        IMMNotificationClient_Impl, MMDeviceEnumerator, eConsole,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::core::{PCWSTR, implement};

    use super::GENERATION;

    fn bump() {
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }

    #[implement(IMMNotificationClient)]
    struct Client;

    impl IMMNotificationClient_Impl for Client_Impl {
        fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
            bump();
            Ok(())
        }

        fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
            bump();
            Ok(())
        }

        fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
            bump();
            Ok(())
        }

        fn OnDefaultDeviceChanged(
            &self,
            _: EDataFlow,
            role: ERole,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            // Each change fires once per role; the console role is enough.
            if role == eConsole {
                bump();
            }
            Ok(())
        }

        fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows::core::Result<()> {
            // Fires often (names, formats, jack info); nothing to act on.
            Ok(())
        }
    }

    /// Registers the client once, from a thread of its own in the
    /// multithreaded apartment, so it never meets the UI thread's OLE
    /// apartment. The thread holds the registration for the life of the
    /// process; the OS drops it at exit.
    pub fn ensure_started() -> bool {
        static STARTED: OnceLock<bool> = OnceLock::new();
        *STARTED.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("audio endpoints".into())
                .spawn(move || {
                    let registered = unsafe { register() };
                    let ok = registered.is_ok();
                    let _ = tx.send(ok);
                    if let Ok(keep) = registered {
                        // Park forever, keeping the enumerator and client alive.
                        let _keep = keep;
                        loop {
                            std::thread::park();
                        }
                    }
                });
            spawned.is_ok() && rx.recv().unwrap_or(false)
        })
    }

    unsafe fn register() -> windows::core::Result<(IMMDeviceEnumerator, IMMNotificationClient)> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let client: IMMNotificationClient = Client.into();
            en.RegisterEndpointNotificationCallback(&client)?;
            Ok((en, client))
        }
    }
}
