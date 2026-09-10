mod app;
pub mod background_ml;
pub mod calibration;
mod cut;
pub mod export;
mod icons;
pub mod jobs;
pub mod path_edit;
mod peel_tab;
mod protocol;
pub mod raw_usb;
pub mod shapes;
mod studio;
mod theme;
pub mod toolpath;
mod transports;
mod views;

use futures::Stream;
#[cfg(not(target_arch = "wasm32"))]
use futures::StreamExt;
use std::time::Duration;

pub use app::SapodillaApp;

#[cfg(target_arch = "wasm32")]
type Rc<T> = std::rc::Rc<T>;
#[cfg(not(target_arch = "wasm32"))]
type Rc<T> = std::sync::Arc<T>;

/// Whether this browser context can safely share the application's WebAssembly
/// memory with a worker. `wasm_thread` panics internally instead of returning
/// an error when `postMessage` rejects a `SharedArrayBuffer`, so callers must
/// enforce this precondition before entering the dependency.
#[cfg(all(target_arch = "wasm32", feature = "web-workers"))]
#[doc(hidden)]
pub fn browser_workers_ready() -> bool {
    let global = js_sys::global();
    let isolated = js_sys::Reflect::get(
        &global,
        &wasm_bindgen::JsValue::from_str("crossOriginIsolated"),
    )
    .ok()
    .and_then(|value| value.as_bool())
    .unwrap_or(false);
    let shared_array_buffer = js_sys::Reflect::has(
        &global,
        &wasm_bindgen::JsValue::from_str("SharedArrayBuffer"),
    )
    .unwrap_or(false);
    isolated && shared_array_buffer
}

#[cfg(all(target_arch = "wasm32", not(feature = "web-workers")))]
#[doc(hidden)]
pub fn browser_workers_ready() -> bool {
    true
}

#[cfg(target_arch = "wasm32")]
#[inline]
fn spawn<F>(future: F)
where
    F: Future<Output = ()> + 'static,
{
    wasm_bindgen_futures::spawn_local(future);
}

#[cfg(target_arch = "wasm32")]
#[inline]
fn spawn_blocking<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    if let Err(error) = try_spawn_blocking(f) {
        tracing::error!(%error, "could not start background worker");
    }
}

#[cfg(target_arch = "wasm32")]
#[inline]
fn try_spawn_blocking<F>(f: F) -> Result<(), String>
where
    F: FnOnce() + Send + 'static,
{
    #[cfg(feature = "web-workers")]
    {
        if !browser_workers_ready() {
            return Err(
                "browser workers require a cross-origin-isolated page; reload Sapodilla".into(),
            );
        }
        wasm_thread::Builder::new()
            .spawn(f)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    #[cfg(not(feature = "web-workers"))]
    {
        f();
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[inline]
fn spawn<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::task::spawn(future);
}

#[cfg(not(target_arch = "wasm32"))]
#[inline]
fn spawn_blocking<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    let _ = try_spawn_blocking(f);
}

#[cfg(not(target_arch = "wasm32"))]
#[inline]
fn try_spawn_blocking<F>(f: F) -> Result<(), String>
where
    F: FnOnce() + Send + 'static,
{
    tokio::task::spawn_blocking(f);
    Ok(())
}

/// Create a stream that resolves every given interval.
///
/// Will panic on WASM targets if `duration`'s milliseconds is greater than
/// `u32::MAX`.
fn interval(duration: Duration) -> impl Stream<Item = ()> {
    #[cfg(target_arch = "wasm32")]
    let s = gloo_timers::future::IntervalStream::new(u32::try_from(duration.as_millis()).unwrap());

    #[cfg(not(target_arch = "wasm32"))]
    let s =
        tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(duration)).map(|_| ());

    s
}
