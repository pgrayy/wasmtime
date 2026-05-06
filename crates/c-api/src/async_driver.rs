//! C API for the external event loop driver.
//!
//! This module exposes a task-agnostic async runtime bridge through wasmtime's
//! C API. The host monitors a single fd (Unix) or notification socket (Windows)
//! and calls `wasmtime_async_driver_poll_once` to advance the runtime.
//!
//! Tasks are spawned from Rust via `AsyncDriver::spawn_task()`. The C API only
//! provides lifecycle management and result retrieval.

use std::collections::HashMap;
use std::ffi::{c_char, CString};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::runtime::Runtime;

#[cfg(windows)]
type RawHandle = std::os::windows::io::RawSocket;
#[cfg(windows)]
const INVALID_HANDLE: RawHandle = windows_sys::Win32::Networking::WinSock::INVALID_SOCKET;

/// Task completion result.
struct TaskResult {
    data: Result<Vec<u8>, String>,
}

/// Shared state that spawned tasks can access via Arc.
struct Shared {
    #[cfg(windows)]
    notify_write: Mutex<RawHandle>,
    responses: Mutex<HashMap<u64, TaskResult>>,
    next_id: AtomicU64,
    shutdown: AtomicBool,
}

/// The async driver manages a Tokio runtime and a completion registry.
pub struct AsyncDriver {
    runtime: Runtime,
    shared: Arc<Shared>,
}

// C API opaque type alias
type wasmtime_async_driver_t = AsyncDriver;

impl AsyncDriver {
    fn new() -> Option<Box<Self>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;

        Some(Box::new(Self {
            runtime,
            shared: Arc::new(Shared {
                #[cfg(windows)]
                notify_write: Mutex::new(INVALID_HANDLE),
                responses: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                shutdown: AtomicBool::new(false),
            }),
        }))
    }

    /// Spawn a future on the runtime. Returns a task ID (> 0), or 0 if shut down.
    pub fn spawn_task<F>(&self, future: F) -> u64
    where
        F: Future<Output = Result<Vec<u8>, String>> + Send + 'static,
    {
        if self.shared.shutdown.load(Ordering::Relaxed) {
            return 0;
        }

        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let _guard = self.runtime.enter();
        let shared = Arc::clone(&self.shared);

        tokio::spawn(async move {
            let result = future.await;
            let mut responses = shared.responses.lock().unwrap();
            responses.insert(id, TaskResult { data: result });
            drop(responses);

            #[cfg(windows)]
            notify_host(&shared);
        });

        id
    }

    /// Access the runtime for direct use.
    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }
}

#[cfg(windows)]
fn notify_host(shared: &Shared) {
    if shared.shutdown.load(Ordering::Relaxed) {
        return;
    }
    let handle = *shared.notify_write.lock().unwrap();
    if handle != INVALID_HANDLE {
        unsafe {
            windows_sys::Win32::Networking::WinSock::send(
                handle as usize,
                b"!" as *const u8,
                1,
                0,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// C API
// ---------------------------------------------------------------------------

const WASMTIME_ASYNC_TASK_PENDING: i32 = 0;
const WASMTIME_ASYNC_TASK_OK: i32 = 1;
const WASMTIME_ASYNC_TASK_ERROR: i32 = -1;

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_async_driver_new() -> *mut wasmtime_async_driver_t {
    match AsyncDriver::new() {
        Some(driver) => Box::into_raw(driver),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_async_driver_delete(driver: *mut wasmtime_async_driver_t) {
    if !driver.is_null() {
        let driver = unsafe { Box::from_raw(driver) };
        driver.shared.shutdown.store(true, Ordering::SeqCst);

        #[cfg(windows)]
        {
            let mut w = driver.shared.notify_write.lock().unwrap();
            let handle = *w;
            if handle != INVALID_HANDLE {
                unsafe {
                    windows_sys::Win32::Networking::WinSock::closesocket(handle as usize);
                }
                *w = INVALID_HANDLE;
            }
        }

        drop(driver);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_async_driver_io_fd(
    driver: *const wasmtime_async_driver_t,
) -> i64 {
    #[cfg(unix)]
    {
        let driver = unsafe { &*driver };
        driver.runtime.io_fd() as i64
    }
    #[cfg(windows)]
    {
        let _ = driver;
        -1
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_async_driver_set_notify_socket(
    driver: *mut wasmtime_async_driver_t,
    socket_handle: u64,
) {
    #[cfg(windows)]
    {
        let driver = unsafe { &*driver };
        let mut w = driver.shared.notify_write.lock().unwrap();
        *w = socket_handle as RawHandle;
    }
    #[cfg(unix)]
    {
        let _ = (driver, socket_handle);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_async_driver_start_background(
    driver: *mut wasmtime_async_driver_t,
) {
    #[cfg(windows)]
    {
        let driver_ptr = driver;
        std::thread::Builder::new()
            .name("wasmtime-async-driver".into())
            .spawn(move || {
                let driver = unsafe { &*driver_ptr };
                driver.runtime.block_on(std::future::pending::<()>());
            })
            .expect("failed to spawn async driver background thread");
    }
    #[cfg(unix)]
    {
        let _ = driver;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_async_driver_poll_once(
    driver: *mut wasmtime_async_driver_t,
) -> i64 {
    #[cfg(unix)]
    {
        let driver = unsafe { &*driver };
        driver.runtime.poll_once()
    }
    #[cfg(windows)]
    {
        let _ = driver;
        -1
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_async_driver_task_poll(
    driver: *const wasmtime_async_driver_t,
    task_id: u64,
) -> i32 {
    let driver = unsafe { &*driver };
    let map = driver.shared.responses.lock().unwrap();
    match map.get(&task_id) {
        None => WASMTIME_ASYNC_TASK_PENDING,
        Some(r) if r.data.is_ok() => WASMTIME_ASYNC_TASK_OK,
        Some(_) => WASMTIME_ASYNC_TASK_ERROR,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_async_driver_task_get_response(
    driver: *mut wasmtime_async_driver_t,
    task_id: u64,
) -> *mut c_char {
    let driver = unsafe { &*driver };
    let mut map = driver.shared.responses.lock().unwrap();
    match map.remove(&task_id) {
        Some(TaskResult { data: Ok(bytes) }) => {
            let s = String::from_utf8_lossy(&bytes).to_string();
            match CString::new(s) {
                Ok(c_str) => c_str.into_raw(),
                Err(_) => std::ptr::null_mut(),
            }
        }
        other => {
            if let Some(r) = other {
                map.insert(task_id, r);
            }
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_async_driver_task_get_error(
    driver: *mut wasmtime_async_driver_t,
    task_id: u64,
) -> *mut c_char {
    let driver = unsafe { &*driver };
    let mut map = driver.shared.responses.lock().unwrap();
    match map.remove(&task_id) {
        Some(TaskResult { data: Err(msg) }) => {
            match CString::new(msg) {
                Ok(c_str) => c_str.into_raw(),
                Err(_) => std::ptr::null_mut(),
            }
        }
        other => {
            if let Some(r) = other {
                map.insert(task_id, r);
            }
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_async_driver_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            let _ = CString::from_raw(ptr);
        }
    }
}
