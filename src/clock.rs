//! Runtime clock adapter; algorithms accept an injected clock.
#[cfg(not(target_arch = "wasm32"))]
pub fn milliseconds() -> f64 {
    use std::{
        sync::OnceLock,
        time::{Instant, SystemTime, UNIX_EPOCH},
    };
    static ORIGIN: OnceLock<(f64, Instant)> = OnceLock::new();
    let (epoch, steady) = ORIGIN.get_or_init(|| {
        (
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64()
                * 1000.0,
            Instant::now(),
        )
    });
    epoch + steady.elapsed().as_secs_f64() * 1000.0
}
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
extern "C" {
    fn haneoka_now_ms() -> f64;
}
#[cfg(target_arch = "wasm32")]
pub fn milliseconds() -> f64 {
    unsafe { haneoka_now_ms() }
}
