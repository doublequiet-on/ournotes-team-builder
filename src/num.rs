//! Native-game binary32 steps and JavaScript-compatible integer conversions.
pub fn f(x: f64) -> f64 {
    x as f32 as f64
}
pub fn i32(x: f64) -> i32 {
    // Inside the Int32 range the wrapping conversion is plain truncation.
    if x > -2_147_483_649.0 && x < 2_147_483_648.0 {
        return x as i32;
    }
    if !x.is_finite() || x == 0.0 {
        return 0;
    }
    x.trunc().rem_euclid(4_294_967_296.0) as u32 as i32
}
pub fn imul(a: i32, b: i32) -> i32 {
    a.wrapping_mul(b)
}
pub fn floor_i32(x: f64) -> i32 {
    i32(x.floor())
}
pub fn round(x: f64) -> f64 {
    (x + 0.5).floor()
}
pub fn score_delta(value: i32) -> i32 {
    (f(f(f(value as f64) / f(10000.0)) * f(100000.0))).floor() as i32
}
pub fn judgement_delta(value: i32) -> i32 {
    let scaled = f(f(f(value as f64) / f(10000.0)) * 100000.0);
    let lower = scaled.floor() as i32;
    let fraction = scaled - lower as f64;
    if fraction < 0.5 {
        lower
    } else if fraction > 0.5 || lower % 2 != 0 {
        lower + 1
    } else {
        lower
    }
}
pub fn window_ms(seconds: f64, extension: f64) -> i32 {
    f(f(f(seconds) * f(1000.0)) + extension).ceil() as i32
}
