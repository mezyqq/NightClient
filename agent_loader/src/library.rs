//! Client-library lifecycle: load, hot-reload and drop.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use libloading::{Library, Symbol};
use log::{error, info, warn};

/// Internal result type — agent-side errors are only ever logged.
type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// The currently loaded client library, if any.
static CLIENT_LIBRARY: OnceLock<Mutex<Option<Library>>> = OnceLock::new();

/// How long a temporary library copy is kept before deletion.
const TEMP_CLEANUP_DELAY: Duration = Duration::from_secs(5);

/// Locks the client-library slot, recovering from a poisoned mutex rather
/// than panicking — a poisoned lock here would otherwise wedge the agent.
fn lock() -> MutexGuard<'static, Option<Library>> {
    CLIENT_LIBRARY
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| {
            error!("client-library mutex was poisoned; recovering");
            poisoned.into_inner()
        })
}

/// Hot-reloads the client library from `source`.
///
/// The file is copied to a uniquely named temporary path first, so the
/// original can be rebuilt while a copy stays mapped (this matters on
/// Windows, where a loaded DLL is locked on disk).
pub fn reload(source: &Path) -> Result<()> {
    info!("reloading client library from {}", source.display());
    let temp = copy_to_temp(source)?;
    schedule_temp_cleanup(temp.clone());
    load(&temp)
}

/// Drops the client library without calling `cleanup_client`. Used during
/// agent teardown, where the JVM is already going away.
pub fn shutdown() {
    if lock().take().is_some() {
        info!("client library dropped");
    }
}

/// Replaces the loaded client library with the one at `path`.
///
/// The previous client is cleaned up and dropped *before* the new one is
/// initialized, so the old client's hooks are gone before the new client
/// installs its own. The whole swap holds the lock, so concurrent reloads
/// serialize safely.
fn load(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(format!("client library not found at {}", path.display()).into());
    }

    let mut slot = lock();

    if let Some(old) = slot.take() {
        // SAFETY: `cleanup_client`, if exported, is an `extern "C" fn()`.
        unsafe { call_export(&old, b"cleanup_client") };
        drop(old);
        info!("previous client library unloaded");
    }

    info!("loading client library {}", path.display());
    // SAFETY: loading a trusted shared library built by this project.
    let library = unsafe { Library::new(path)? };
    // SAFETY: `initialize_client`, if exported, is an `extern "C" fn()`.
    unsafe { call_export(&library, b"initialize_client") };

    *slot = Some(library);
    info!("client library loaded");
    Ok(())
}

/// Calls an exported `extern "C" fn() -> bool` on the loaded client — a
/// request the client reports success for. Fails when no client is loaded,
/// the symbol is missing (an older client), or the client returns `false`.
pub fn call_status(symbol: &[u8]) -> Result<()> {
    let slot = lock();
    let library = slot.as_ref().ok_or("клиент не загружен — сначала инжект")?;
    let name = String::from_utf8_lossy(symbol).into_owned();
    // SAFETY: the client exports these symbols as `extern "C" fn() -> bool`.
    let func = unsafe { library.get::<Symbol<extern "C" fn() -> bool>>(symbol) }
        .map_err(|_| format!("{name} не экспортирован — переинжектите клиент"))?;
    if func() {
        Ok(())
    } else {
        Err(format!("{name} не выполнен — подробности в dark_client.log").into())
    }
}

/// Calls an exported `extern "C" fn()` by name, if the library exports it.
///
/// # Safety
/// The named symbol, if present, must be an `extern "C" fn()`.
unsafe fn call_export(library: &Library, symbol: &[u8]) {
    match library.get::<Symbol<extern "C" fn()>>(symbol) {
        Ok(func) => {
            let name = String::from_utf8_lossy(symbol);
            info!("calling {name}");
            func();
        }
        Err(_) => {
            let name = String::from_utf8_lossy(symbol);
            info!("{name} not exported; skipping");
        }
    }
}

/// Copies `source` to a uniquely named file in the system temp directory.
fn copy_to_temp(source: &Path) -> Result<PathBuf> {
    let file_name = source
        .file_name()
        .ok_or("client library path has no file name")?
        .to_string_lossy()
        .into_owned();

    // Nanosecond stamp keeps reloads within the same second from colliding.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let mut temp = std::env::temp_dir();
    temp.push(format!("dark_client_{stamp}_{file_name}"));

    std::fs::copy(source, &temp)?;
    info!("client library copied to {}", temp.display());
    Ok(temp)
}

/// Spawns a thread that deletes a temporary library copy after a short delay.
fn schedule_temp_cleanup(temp: PathBuf) {
    thread::spawn(move || {
        thread::sleep(TEMP_CLEANUP_DELAY);
        if let Err(e) = std::fs::remove_file(&temp) {
            warn!("could not remove temporary library {}: {e}", temp.display());
        }
    });
}
