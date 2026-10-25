//! TUN on Windows through Wintun (wintun.net), the signed layer-3 driver made for WireGuard.
//!
//! wintun.dll ships next to meshd.exe and is loaded at run time, from the application
//! directory or System32 only (never the current directory or PATH, so a planted DLL is not
//! picked up). The adapter is named "flytoMesh" and keeps a fixed GUID, so Windows sees the
//! same network across restarts.

use std::ffi::c_void;
use std::io;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{GetLastError, HANDLE, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};

pub const NAME: &str = "flytoMesh";
const ADAPTER_GUID: GUID = GUID {
    data1: 0xf1a7e5a0,
    data2: 0x6d6e,
    data3: 0x4b0c,
    data4: [0x9a, 0x3e, 0x3c, 0x2e, 0x5f, 0x1d, 0x0a, 0x49],
};
/// Ring size per direction; a power of two between 128 KiB and 64 MiB.
const RING: u32 = 0x40_0000;
const ERROR_NO_MORE_ITEMS: u32 = 259;

type Handle = *mut c_void;

struct Fns {
    open_adapter: unsafe extern "system" fn(*const u16) -> Handle,
    create_adapter: unsafe extern "system" fn(*const u16, *const u16, *const GUID) -> Handle,
    close_adapter: unsafe extern "system" fn(Handle),
    start_session: unsafe extern "system" fn(Handle, u32) -> Handle,
    end_session: unsafe extern "system" fn(Handle),
    read_wait_event: unsafe extern "system" fn(Handle) -> HANDLE,
    receive: unsafe extern "system" fn(Handle, *mut u32) -> *mut u8,
    release_receive: unsafe extern "system" fn(Handle, *const u8),
    allocate_send: unsafe extern "system" fn(Handle, u32) -> *mut u8,
    send: unsafe extern "system" fn(Handle, *const u8),
}

pub struct Tun {
    f: Fns,
    _lib: HMODULE,
    adapter: Handle,
    session: Handle,
    event: HANDLE,
}

// Wintun sessions are safe to use from several threads: one receives, others send.
unsafe impl Send for Tun {}
unsafe impl Sync for Tun {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error(what: &str) -> io::Error {
    let code = unsafe { GetLastError() };
    io::Error::new(io::Error::from_raw_os_error(code as i32).kind(), format!("{what}: Windows error {code}"))
}

/// Windows `ERROR_ALREADY_EXISTS`.
const ERROR_ALREADY_EXISTS: u32 = 183;
/// How long to wait for a previous run's adapter to go away.
const ADAPTER_SETTLE: std::time::Duration = std::time::Duration::from_secs(10);

pub fn open() -> io::Result<Tun> {
    unsafe {
        let lib = LoadLibraryExW(
            wide("wintun.dll").as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        );
        if lib.is_null() {
            return Err(last_error("loading wintun.dll (it must sit next to meshd.exe)"));
        }
        // Each export is cast to the signature declared in wintun.h (same size: a fn pointer).
        macro_rules! sym {
            ($name:literal) => {{
                let p = GetProcAddress(lib, concat!($name, "\0").as_ptr())
                    .ok_or_else(|| io::Error::other(concat!("wintun.dll has no ", $name)))?;
                std::mem::transmute_copy(&p)
            }};
        }
        let f = Fns {
            open_adapter: sym!("WintunOpenAdapter"),
            create_adapter: sym!("WintunCreateAdapter"),
            close_adapter: sym!("WintunCloseAdapter"),
            start_session: sym!("WintunStartSession"),
            end_session: sym!("WintunEndSession"),
            read_wait_event: sym!("WintunGetReadWaitEvent"),
            receive: sym!("WintunReceivePacket"),
            release_receive: sym!("WintunReleaseReceivePacket"),
            allocate_send: sym!("WintunAllocateSendPacket"),
            send: sym!("WintunSendPacket"),
        };
        let name = wide(NAME);
        // Reuse the adapter a previous run left behind, else create it. Right after a restart
        // (e.g. a self-update) the old adapter can still be going away: open finds nothing and
        // create says it exists (183). Retry for a while instead of failing the start, which
        // would also use up one of the new release's three health checks.
        let deadline = std::time::Instant::now() + ADAPTER_SETTLE;
        let adapter = loop {
            let mut adapter = (f.open_adapter)(name.as_ptr());
            if adapter.is_null() {
                adapter = (f.create_adapter)(name.as_ptr(), name.as_ptr(), &ADAPTER_GUID);
            }
            if !adapter.is_null() {
                break adapter;
            }
            let code = GetLastError();
            if code != ERROR_ALREADY_EXISTS || std::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::Error::from_raw_os_error(code as i32).kind(),
                    format!("creating the Wintun adapter (needs Administrator / SYSTEM): Windows error {code}"),
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        };
        let session = (f.start_session)(adapter, RING);
        if session.is_null() {
            let e = last_error("starting the Wintun session");
            (f.close_adapter)(adapter);
            return Err(e);
        }
        let event = (f.read_wait_event)(session);
        Ok(Tun { f, _lib: lib, adapter, session, event })
    }
}

impl Tun {
    pub fn name(&self) -> &str {
        NAME
    }

    /// Reads one IP packet into `buf` (blocking). Returns its length.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.read_nb(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => unsafe {
                    WaitForSingleObject(self.event, INFINITE);
                },
                r => return r,
            }
        }
    }

    /// Like `read` but never blocks: `WouldBlock` when no packet is queued.
    pub fn read_nb(&self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe {
            let mut size = 0u32;
            let p = (self.f.receive)(self.session, &mut size);
            if p.is_null() {
                let code = GetLastError();
                return Err(if code == ERROR_NO_MORE_ITEMS {
                    io::ErrorKind::WouldBlock.into()
                } else {
                    io::Error::other(format!("Wintun receive: Windows error {code}"))
                });
            }
            let n = (size as usize).min(buf.len());
            std::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), n);
            (self.f.release_receive)(self.session, p);
            Ok(n)
        }
    }

    /// Writes one IP packet. A full ring drops it (the inner TCP recovers).
    pub fn write(&self, pkt: &[u8], _v6: bool) -> io::Result<()> {
        unsafe {
            let p = (self.f.allocate_send)(self.session, pkt.len() as u32);
            if p.is_null() {
                return Ok(());
            }
            std::ptr::copy_nonoverlapping(pkt.as_ptr(), p, pkt.len());
            (self.f.send)(self.session, p);
        }
        Ok(())
    }
}

impl Drop for Tun {
    fn drop(&mut self) {
        unsafe {
            (self.f.end_session)(self.session);
            (self.f.close_adapter)(self.adapter);
        }
    }
}
