//! The math library (`api-ms-win-crt-math`), in `double` and `float` forms,
//! with the CRT's classification helpers and integer division.
//!
//! Most functions are Rust's `f64`/`f32` methods. Not in `std`, and written
//! here: `erf`/`erfc` (the Maclaurin series below 2, a continued fraction
//! for `erfc` above), `tgamma`/`lgamma` (Lanczos, g = 7 with nine
//! coefficients, and the reflection formula below 1/2), `remainder` and
//! `remquo` (exact, by reductions of the quotient), `frexp`, `ldexp` and
//! `nextafter` (on the bits). The `float` forms of those compute in
//! `double`.
//!
//! Limits: the functions set no `errno` and raise no floating-point
//! exception of their own; `rint` and the `lrint` family round to nearest
//! even whatever the rounding mode; the Bessel functions (`_j0` ... `_yn`)
//! are not implemented and return NaN with `EDOM`.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_char;
use std::sync::Once;

use super::runtime::{EDOM, set_errno};
use crate::{HleExport, hle_export};

/// One-argument functions that are `f64`/`f32` methods.
macro_rules! unary {
    ($($name:ident, $namef:ident, $method:ident;)*) => {
        $(
            /// See the C function of this name.
            pub extern "win64" fn $name(x: f64) -> f64 {
                x.$method()
            }

            /// See the C function of this name.
            pub extern "win64" fn $namef(x: f32) -> f32 {
                x.$method()
            }
        )*
    };
}

unary! {
    sin, sinf, sin;
    cos, cosf, cos;
    tan, tanf, tan;
    asin, asinf, asin;
    acos, acosf, acos;
    atan, atanf, atan;
    sinh, sinhf, sinh;
    cosh, coshf, cosh;
    tanh, tanhf, tanh;
    asinh, asinhf, asinh;
    acosh, acoshf, acosh;
    atanh, atanhf, atanh;
    exp, expf, exp;
    exp2, exp2f, exp2;
    expm1, expm1f, exp_m1;
    log, logf, ln;
    log2, log2f, log2;
    log10, log10f, log10;
    log1p, log1pf, ln_1p;
    sqrt, sqrtf, sqrt;
    cbrt, cbrtf, cbrt;
    fabs, fabsf, abs;
    floor, floorf, floor;
    ceil, ceilf, ceil;
    round, roundf, round;
    trunc, truncf, trunc;
    rint, rintf, round_ties_even;
    nearbyint, nearbyintf, round_ties_even;
}

/// Two-argument functions that are `f64`/`f32` methods.
macro_rules! binary {
    ($($name:ident, $namef:ident, $method:ident;)*) => {
        $(
            /// See the C function of this name.
            pub extern "win64" fn $name(x: f64, y: f64) -> f64 {
                x.$method(y)
            }

            /// See the C function of this name.
            pub extern "win64" fn $namef(x: f32, y: f32) -> f32 {
                x.$method(y)
            }
        )*
    };
}

binary! {
    atan2, atan2f, atan2;
    pow, powf, powf;
    hypot, hypotf, hypot;
    fmax, fmaxf, max;
    fmin, fminf, min;
    copysign, copysignf, copysign;
}

/// `x` modulo `y`, with the sign of `x`.
pub extern "win64" fn fmod(x: f64, y: f64) -> f64 {
    x % y
}

/// See [`fmod`].
pub extern "win64" fn fmodf(x: f32, y: f32) -> f32 {
    x % y
}

/// `x - y` if positive, else +0.
pub extern "win64" fn fdim(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        x + y
    } else if x > y {
        x - y
    } else {
        0.0
    }
}

/// See [`fdim`].
pub extern "win64" fn fdimf(x: f32, y: f32) -> f32 {
    fdim(f64::from(x), f64::from(y)) as f32
}

/// `x * y + z`, rounded once.
pub extern "win64" fn fma(x: f64, y: f64, z: f64) -> f64 {
    x.mul_add(y, z)
}

/// See [`fma`].
pub extern "win64" fn fmaf(x: f32, y: f32, z: f32) -> f32 {
    x.mul_add(y, z)
}

/// A value rounded to nearest even as an integer of the range `min..=max`;
/// the "integer indefinite" (`min`) out of range or for NaN, as the
/// processor's conversion gives.
fn to_integer(x: f64, min: i64, max: i64) -> i64 {
    if x.is_nan() || x < min as f64 || x >= -(min as f64) {
        return min;
    }
    (x as i64).clamp(min, max)
}

/// Rounds to nearest even as a `long` (32 bits).
pub extern "win64" fn lrint(x: f64) -> i32 {
    to_integer(x.round_ties_even(), i32::MIN.into(), i32::MAX.into()) as i32
}

/// See [`lrint`].
pub extern "win64" fn lrintf(x: f32) -> i32 {
    lrint(f64::from(x))
}

/// Rounds to nearest even as a `long long`.
pub extern "win64" fn llrint(x: f64) -> i64 {
    to_integer(x.round_ties_even(), i64::MIN, i64::MAX)
}

/// See [`llrint`].
pub extern "win64" fn llrintf(x: f32) -> i64 {
    llrint(f64::from(x))
}

/// Rounds half away from zero as a `long` (32 bits).
pub extern "win64" fn lround(x: f64) -> i32 {
    to_integer(x.round(), i32::MIN.into(), i32::MAX.into()) as i32
}

/// See [`lround`].
pub extern "win64" fn lroundf(x: f32) -> i32 {
    lround(f64::from(x))
}

/// Rounds half away from zero as a `long long`.
pub extern "win64" fn llround(x: f64) -> i64 {
    to_integer(x.round(), i64::MIN, i64::MAX)
}

/// See [`llround`].
pub extern "win64" fn llroundf(x: f32) -> i64 {
    llround(f64::from(x))
}

/// The IEEE remainder `x - n * y` (`n` the quotient rounded to nearest
/// even) and the low three bits of `n` with its sign. Each reduction is
/// exact: by `8y` (`fmod`), then by `4y`, `2y` and `y` (each a subtraction
/// of two values within a factor of two).
fn remainder_quotient(x: f64, y: f64) -> (f64, i32) {
    if x.is_nan() || y.is_nan() || x.is_infinite() || y == 0.0 {
        return (f64::NAN, 0);
    }
    let p = y.abs();
    let mut r = x.abs();
    if (8.0 * p).is_finite() {
        r %= 8.0 * p;
    }
    let mut q = 0;
    for (k, step) in [(4, 4.0 * p), (2, 2.0 * p), (1, p)] {
        if r >= step {
            r -= step;
            q += k;
        }
    }
    // Round to nearest: past half, or at half with an odd quotient.
    let past_half = if p < 2.0 * f64::MIN_POSITIVE {
        r + r > p || (r + r == p && q & 1 == 1)
    } else {
        r > 0.5 * p || (r == 0.5 * p && q & 1 == 1)
    };
    if past_half {
        r -= p;
        q += 1;
    }
    let r = if x.is_sign_negative() { -r } else { r };
    let q = q & 7;
    let q = if (x < 0.0) != (y < 0.0) { -q } else { q };
    (r, q)
}

/// The IEEE remainder of `x / y`.
pub extern "win64" fn remainder(x: f64, y: f64) -> f64 {
    remainder_quotient(x, y).0
}

/// See [`remainder`].
pub extern "win64" fn remainderf(x: f32, y: f32) -> f32 {
    remainder(f64::from(x), f64::from(y)) as f32
}

/// [`remainder`], and the low bits of the quotient in `*quo`.
pub extern "win64" fn remquo(x: f64, y: f64, quo: *mut i32) -> f64 {
    let (r, q) = remainder_quotient(x, y);
    if !quo.is_null() {
        // SAFETY: the title's place for the quotient.
        unsafe { quo.write(q) };
    }
    r
}

/// See [`remquo`].
pub extern "win64" fn remquof(x: f32, y: f32, quo: *mut i32) -> f32 {
    remquo(f64::from(x), f64::from(y), quo) as f32
}

/// The mantissa in [0.5, 1) and the exponent of `x`.
fn split(x: f64) -> (f64, i32) {
    if x == 0.0 || !x.is_finite() {
        return (x, 0);
    }
    let (x, bias) = if x.is_subnormal() {
        (x * f64::from_bits(0x4350_0000_0000_0000), -54) // 2^54
    } else {
        (x, 0)
    };
    let bits = x.to_bits();
    let exponent = ((bits >> 52) & 0x7FF) as i32 - 1022;
    let mantissa = f64::from_bits((bits & !(0x7FF << 52)) | (1022 << 52));
    (mantissa, exponent + bias)
}

/// The mantissa in [0.5, 1) of `x`, its exponent in `*exp`.
pub extern "win64" fn frexp(x: f64, exp: *mut i32) -> f64 {
    let (mantissa, exponent) = split(x);
    if !exp.is_null() {
        // SAFETY: the title's place for the exponent.
        unsafe { exp.write(exponent) };
    }
    mantissa
}

/// See [`frexp`].
pub extern "win64" fn frexpf(x: f32, exp: *mut i32) -> f32 {
    frexp(f64::from(x), exp) as f32
}

/// `x * 2^n`, in steps so that no factor overflows and a subnormal result is
/// rounded once.
fn scale(x: f64, n: i64) -> f64 {
    let mut y = x;
    let mut n = n;
    let big = f64::from_bits(0x7FE0_0000_0000_0000); // 2^1023
    let small = f64::from_bits(0x0360_0000_0000_0000); // 2^-1022 * 2^53
    if n > 1023 {
        y *= big;
        n -= 1023;
        if n > 1023 {
            y *= big;
            n = (n - 1023).min(1023);
        }
    } else if n < -1022 {
        y *= small;
        n += 1022 - 53;
        if n < -1022 {
            y *= small;
            n = (n + 1022 - 53).max(-1022);
        }
    }
    y * f64::from_bits(((0x3FF + n) as u64) << 52)
}

/// `x * 2^n` for a float, in steps as [`scale`].
fn scale_f32(x: f32, n: i64) -> f32 {
    let mut y = x;
    let mut n = n;
    let big = f32::from_bits(0x7F00_0000); // 2^127
    let small = f32::from_bits(0x0C80_0000); // 2^-126 * 2^24
    if n > 127 {
        y *= big;
        n -= 127;
        if n > 127 {
            y *= big;
            n = (n - 127).min(127);
        }
    } else if n < -126 {
        y *= small;
        n += 126 - 24;
        if n < -126 {
            y *= small;
            n = (n + 126 - 24).max(-126);
        }
    }
    y * f32::from_bits(((0x7F + n) as u32) << 23)
}

/// `x * 2^n`.
pub extern "win64" fn ldexp(x: f64, n: i32) -> f64 {
    scale(x, n.into())
}

/// See [`ldexp`].
pub extern "win64" fn ldexpf(x: f32, n: i32) -> f32 {
    scale_f32(x, n.into())
}

/// See [`ldexp`].
pub extern "win64" fn scalbn(x: f64, n: i32) -> f64 {
    scale(x, n.into())
}

/// See [`ldexp`].
pub extern "win64" fn scalbnf(x: f32, n: i32) -> f32 {
    scale_f32(x, n.into())
}

/// See [`ldexp`] (`long` is 32 bits).
pub extern "win64" fn scalbln(x: f64, n: i32) -> f64 {
    scale(x, n.into())
}

/// See [`scalbln`].
pub extern "win64" fn scalblnf(x: f32, n: i32) -> f32 {
    scale_f32(x, n.into())
}

/// See [`ldexp`].
pub extern "win64" fn _scalb(x: f64, n: i32) -> f64 {
    scale(x, n.into())
}

/// See [`ldexp`].
pub extern "win64" fn _scalbf(x: f32, n: i32) -> f32 {
    scale_f32(x, n.into())
}

/// The fractional part of `x`, its integral part in `*int`.
pub extern "win64" fn modf(x: f64, int: *mut f64) -> f64 {
    let whole = x.trunc();
    if !int.is_null() {
        // SAFETY: the title's place for the integral part.
        unsafe { int.write(whole) };
    }
    if x.is_infinite() {
        0.0f64.copysign(x)
    } else {
        (x - whole).copysign(x)
    }
}

/// See [`modf`].
pub extern "win64" fn modff(x: f32, int: *mut f32) -> f32 {
    let whole = x.trunc();
    if !int.is_null() {
        // SAFETY: the title's place for the integral part.
        unsafe { int.write(whole) };
    }
    if x.is_infinite() {
        0.0f32.copysign(x)
    } else {
        (x - whole).copysign(x)
    }
}

/// A quiet NaN (the tag is not used).
pub extern "win64" fn nan(_tag: *const c_char) -> f64 {
    f64::NAN
}

/// See [`nan`].
pub extern "win64" fn nanf(_tag: *const c_char) -> f32 {
    f32::NAN
}

/// The next double after `x` toward `y`.
pub extern "win64" fn nextafter(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    if x == y {
        return y;
    }
    if x == 0.0 {
        return f64::from_bits(1).copysign(y);
    }
    let bits = x.to_bits();
    let away = (y > x) == (x > 0.0);
    f64::from_bits(if away { bits + 1 } else { bits - 1 })
}

/// The next float after `x` toward `y`.
pub extern "win64" fn nextafterf(x: f32, y: f32) -> f32 {
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    if x == y {
        return y;
    }
    if x == 0.0 {
        return f32::from_bits(1).copysign(y);
    }
    let bits = x.to_bits();
    let away = (y > x) == (x > 0.0);
    f32::from_bits(if away { bits + 1 } else { bits - 1 })
}

/// See [`nextafter`] (`long double` is `double`).
pub extern "win64" fn nexttoward(x: f64, y: f64) -> f64 {
    nextafter(x, y)
}

/// The next float after `x` toward the double `y`.
pub extern "win64" fn nexttowardf(x: f32, y: f64) -> f32 {
    if f64::from(x) == y {
        return y as f32;
    }
    let target = if y > f64::from(x) {
        f32::INFINITY
    } else {
        f32::NEG_INFINITY
    };
    if y.is_nan() {
        return f32::NAN;
    }
    nextafterf(x, target)
}

/// See [`nextafter`].
pub extern "win64" fn _nextafter(x: f64, y: f64) -> f64 {
    nextafter(x, y)
}

/// See [`nextafterf`].
pub extern "win64" fn _nextafterf(x: f32, y: f32) -> f32 {
    nextafterf(x, y)
}

/// The binary exponent of `x` as a double: -inf for 0, inf for inf.
pub extern "win64" fn logb(x: f64) -> f64 {
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if !x.is_finite() {
        return x * x;
    }
    f64::from(split(x).1 - 1)
}

/// See [`logb`].
pub extern "win64" fn logbf(x: f32) -> f32 {
    logb(f64::from(x)) as f32
}

/// See [`logb`].
pub extern "win64" fn _logb(x: f64) -> f64 {
    logb(x)
}

/// See [`logb`].
pub extern "win64" fn _logbf(x: f32) -> f32 {
    logbf(x)
}

/// `FP_ILOGB0` and `FP_ILOGBNAN` as MSVC has them.
const ILOGB0: i32 = i32::MIN;
const ILOGBNAN: i32 = i32::MAX;

/// The binary exponent of `x` as an integer.
pub extern "win64" fn ilogb(x: f64) -> i32 {
    if x == 0.0 {
        ILOGB0
    } else if x.is_nan() || x.is_infinite() {
        ILOGBNAN
    } else {
        split(x).1 - 1
    }
}

/// See [`ilogb`].
pub extern "win64" fn ilogbf(x: f32) -> i32 {
    ilogb(f64::from(x))
}

/// erf by its Maclaurin series: `2/sqrt(pi) * sum (-1)^n x^(2n+1) / (n!
/// (2n+1))`, for |x| up to 2.
fn erf_series(x: f64) -> f64 {
    let x2 = x * x;
    let mut term = x;
    let mut sum = x;
    for n in 1..60 {
        term *= -x2 / f64::from(n);
        let add = term / f64::from(2 * n + 1);
        sum += add;
        if add.abs() < 1e-17 * sum.abs() {
            break;
        }
    }
    sum * std::f64::consts::FRAC_2_SQRT_PI
}

/// erfc by its continued fraction, for x above 2: `exp(-x^2)/sqrt(pi) *
/// 1/(x + (1/2)/(x + 1/(x + (3/2)/(x + 2/(x + ...)))))`, evaluated from
/// its far end.
fn erfc_fraction(x: f64) -> f64 {
    let mut tail = x;
    for k in (1..80).rev() {
        tail = x + (f64::from(k) / 2.0) / tail;
    }
    (-x * x).exp() / std::f64::consts::PI.sqrt() / tail
}

/// The error function.
pub extern "win64" fn erf(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x.abs() <= 2.0 {
        erf_series(x)
    } else {
        (1.0 - erfc_fraction(x.abs())).copysign(x)
    }
}

/// See [`erf`].
pub extern "win64" fn erff(x: f32) -> f32 {
    erf(f64::from(x)) as f32
}

/// The complementary error function, `1 - erf(x)`.
pub extern "win64" fn erfc(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x > 2.0 {
        if x > 27.3 { 0.0 } else { erfc_fraction(x) }
    } else if x < -2.0 {
        2.0 - erfc_fraction(-x)
    } else {
        1.0 - erf_series(x)
    }
}

/// See [`erfc`].
pub extern "win64" fn erfcf(x: f32) -> f32 {
    erfc(f64::from(x)) as f32
}

/// Lanczos's coefficients for g = 7.
const LANCZOS: [f64; 9] = [
    0.999_999_999_999_809_9,
    676.520_368_121_885_1,
    -1_259.139_216_722_402_8,
    771.323_428_777_653_1,
    -176.615_029_162_140_6,
    12.507_343_278_686_905,
    -0.138_571_095_265_720_12,
    9.984_369_578_019_572e-6,
    1.505_632_735_149_311_6e-7,
];
const LANCZOS_G: f64 = 7.0;

/// Lanczos's sum and `t = x + g - 1/2` for Γ(x) = sqrt(2π) t^(x-1/2) e^-t
/// sum, x at least 1/2.
fn lanczos(x: f64) -> (f64, f64) {
    let x = x - 1.0;
    let sum = LANCZOS[1..]
        .iter()
        .enumerate()
        .fold(LANCZOS[0], |sum, (i, c)| sum + c / (x + i as f64 + 1.0));
    (sum, x + LANCZOS_G + 0.5)
}

/// The gamma function.
pub extern "win64" fn tgamma(x: f64) -> f64 {
    if x.is_nan() || x == f64::INFINITY {
        return x;
    }
    if x == 0.0 {
        return f64::INFINITY.copysign(x);
    }
    if x < 0.0 && x == x.floor() {
        set_errno(EDOM);
        return f64::NAN;
    }
    if x < 0.5 {
        // Reflection: Γ(x) Γ(1-x) = π / sin(πx).
        return std::f64::consts::PI / ((std::f64::consts::PI * x).sin() * tgamma(1.0 - x));
    }
    if x > 171.7 {
        return f64::INFINITY;
    }
    let (sum, t) = lanczos(x);
    // t^(x-1/2) as a square, so that it does not overflow before the e^-t.
    let half = t.powf((x - 0.5) / 2.0);
    (2.0 * std::f64::consts::PI).sqrt() * half * ((-t).exp() * half) * sum
}

/// See [`tgamma`].
pub extern "win64" fn tgammaf(x: f32) -> f32 {
    tgamma(f64::from(x)) as f32
}

/// The logarithm of |Γ(x)|.
pub extern "win64" fn lgamma(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x.is_infinite() {
        return f64::INFINITY;
    }
    if x <= 0.0 && x == x.floor() {
        return f64::INFINITY;
    }
    if x < 0.5 {
        let s = (std::f64::consts::PI * x).sin().abs();
        return (std::f64::consts::PI / s).ln() - lgamma(1.0 - x);
    }
    let (sum, t) = lanczos(x);
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x - 0.5) * t.ln() - t + sum.ln()
}

/// See [`lgamma`].
pub extern "win64" fn lgammaf(x: f32) -> f32 {
    lgamma(f64::from(x)) as f32
}

/// MSVC's classes (`fpclassify`).
const FP_INFINITE: i16 = 1;
const FP_NAN: i16 = 2;
const FP_NORMAL: i16 = -1;
const FP_SUBNORMAL: i16 = -2;
const FP_ZERO: i16 = 0;

fn category(category: std::num::FpCategory) -> i16 {
    use std::num::FpCategory::*;
    match category {
        Infinite => FP_INFINITE,
        Nan => FP_NAN,
        Normal => FP_NORMAL,
        Subnormal => FP_SUBNORMAL,
        Zero => FP_ZERO,
    }
}

/// The class of a double (`FP_*`).
pub extern "win64" fn _dclass(x: f64) -> i16 {
    category(x.classify())
}

/// The class of a float (`FP_*`).
pub extern "win64" fn _fdclass(x: f32) -> i16 {
    category(x.classify())
}

/// The sign bit of a double.
pub extern "win64" fn _dsign(x: f64) -> i32 {
    i32::from(x.is_sign_negative())
}

/// The sign bit of a float.
pub extern "win64" fn _fdsign(x: f32) -> i32 {
    i32::from(x.is_sign_negative())
}

/// The class of the double at `p`.
pub extern "win64" fn _dtest(p: *mut f64) -> i16 {
    if p.is_null() {
        return FP_NAN;
    }
    // SAFETY: the title's double.
    _dclass(unsafe { p.read_unaligned() })
}

/// The class of the `long double` (a double) at `p`.
pub extern "win64" fn _ldtest(p: *mut f64) -> i16 {
    _dtest(p)
}

/// The class of the float at `p`.
pub extern "win64" fn _fdtest(p: *mut f32) -> i16 {
    if p.is_null() {
        return FP_NAN;
    }
    // SAFETY: the title's float.
    _fdclass(unsafe { p.read_unaligned() })
}

/// `_FP_LT` 1, `_FP_EQ` 2, `_FP_GT` 4; 0 unordered.
fn ordering(order: Option<std::cmp::Ordering>) -> i32 {
    match order {
        Some(std::cmp::Ordering::Less) => 1,
        Some(std::cmp::Ordering::Equal) => 2,
        Some(std::cmp::Ordering::Greater) => 4,
        None => 0,
    }
}

/// How two doubles compare.
pub extern "win64" fn _dpcomp(a: f64, b: f64) -> i32 {
    ordering(a.partial_cmp(&b))
}

/// How two floats compare.
pub extern "win64" fn _fdpcomp(a: f32, b: f32) -> i32 {
    ordering(a.partial_cmp(&b))
}

/// Whether `x` is finite.
pub extern "win64" fn _finite(x: f64) -> i32 {
    i32::from(x.is_finite())
}

/// Whether `x` is finite.
pub extern "win64" fn _finitef(x: f32) -> i32 {
    i32::from(x.is_finite())
}

/// Whether `x` is a NaN.
pub extern "win64" fn _isnan(x: f64) -> i32 {
    i32::from(x.is_nan())
}

/// Whether `x` is a NaN.
pub extern "win64" fn _isnanf(x: f32) -> i32 {
    i32::from(x.is_nan())
}

/// `x` with its sign changed.
pub extern "win64" fn _chgsign(x: f64) -> f64 {
    -x
}

/// `x` with its sign changed.
pub extern "win64" fn _chgsignf(x: f32) -> f32 {
    -x
}

/// See [`copysign`].
pub extern "win64" fn _copysign(x: f64, y: f64) -> f64 {
    x.copysign(y)
}

/// See [`copysignf`].
pub extern "win64" fn _copysignf(x: f32, y: f32) -> f32 {
    x.copysign(y)
}

/// See [`hypot`].
pub extern "win64" fn _hypot(x: f64, y: f64) -> f64 {
    x.hypot(y)
}

/// See [`hypotf`].
pub extern "win64" fn _hypotf(x: f32, y: f32) -> f32 {
    x.hypot(y)
}

/// The Bessel functions are not implemented: NaN and `EDOM`, with a
/// warning once.
fn bessel() -> f64 {
    static WARNED: Once = Once::new();
    WARNED.call_once(|| tracing::warn!("ucrt: the Bessel functions are not implemented"));
    set_errno(EDOM);
    f64::NAN
}

/// Not implemented: see [`bessel`].
pub extern "win64" fn _j0(_x: f64) -> f64 {
    bessel()
}

/// Not implemented: see [`bessel`].
pub extern "win64" fn _j1(_x: f64) -> f64 {
    bessel()
}

/// Not implemented: see [`bessel`].
pub extern "win64" fn _jn(_n: i32, _x: f64) -> f64 {
    bessel()
}

/// Not implemented: see [`bessel`].
pub extern "win64" fn _y0(_x: f64) -> f64 {
    bessel()
}

/// Not implemented: see [`bessel`].
pub extern "win64" fn _y1(_x: f64) -> f64 {
    bessel()
}

/// Not implemented: see [`bessel`].
pub extern "win64" fn _yn(_n: i32, _x: f64) -> f64 {
    bessel()
}

/// |n| (wrapping for the most negative value).
pub extern "win64" fn abs(n: i32) -> i32 {
    n.wrapping_abs()
}

/// See [`abs`] (`long` is 32 bits).
pub extern "win64" fn labs(n: i32) -> i32 {
    n.wrapping_abs()
}

/// See [`abs`].
pub extern "win64" fn llabs(n: i64) -> i64 {
    n.wrapping_abs()
}

/// See [`abs`].
pub extern "win64" fn _abs64(n: i64) -> i64 {
    n.wrapping_abs()
}

/// The quotient and remainder of `a / b` (0 and 0 for `b` 0, where the
/// processor would fault).
fn divide(a: i64, b: i64) -> (i64, i64) {
    if b == 0 {
        return (0, 0);
    }
    (a.wrapping_div(b), a.wrapping_rem(b))
}

/// `div_t` (quotient, remainder), returned packed in a register: the
/// quotient in the low 32 bits.
pub extern "win64" fn div(a: i32, b: i32) -> i64 {
    let (q, r) = divide(a.into(), b.into());
    (r << 32) | i64::from(q as u32)
}

/// See [`div`] (`long` is 32 bits).
pub extern "win64" fn ldiv(a: i32, b: i32) -> i64 {
    div(a, b)
}

/// `lldiv_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lldiv {
    pub quot: i64,
    pub rem: i64,
}

/// `lldiv`: a 16-byte structure, returned through the hidden pointer the
/// Win64 convention passes first.
pub extern "win64" fn lldiv(out: *mut Lldiv, a: i64, b: i64) -> *mut Lldiv {
    let (quot, rem) = divide(a, b);
    if !out.is_null() {
        // SAFETY: the caller's place for the result.
        unsafe { out.write(Lldiv { quot, rem }) };
    }
    out
}

/// The SSE2 code paths are always used: 1.
pub extern "win64" fn _set_SSE2_enable(_flag: i32) -> i32 {
    1
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(sin),
        hle_export!(sinf),
        hle_export!(cos),
        hle_export!(cosf),
        hle_export!(tan),
        hle_export!(tanf),
        hle_export!(asin),
        hle_export!(asinf),
        hle_export!(acos),
        hle_export!(acosf),
        hle_export!(atan),
        hle_export!(atanf),
        hle_export!(atan2),
        hle_export!(atan2f),
        hle_export!(sinh),
        hle_export!(sinhf),
        hle_export!(cosh),
        hle_export!(coshf),
        hle_export!(tanh),
        hle_export!(tanhf),
        hle_export!(asinh),
        hle_export!(asinhf),
        hle_export!(acosh),
        hle_export!(acoshf),
        hle_export!(atanh),
        hle_export!(atanhf),
        hle_export!(exp),
        hle_export!(expf),
        hle_export!(exp2),
        hle_export!(exp2f),
        hle_export!(expm1),
        hle_export!(expm1f),
        hle_export!(log),
        hle_export!(logf),
        hle_export!(log2),
        hle_export!(log2f),
        hle_export!(log10),
        hle_export!(log10f),
        hle_export!(log1p),
        hle_export!(log1pf),
        hle_export!(pow),
        hle_export!(powf),
        hle_export!(sqrt),
        hle_export!(sqrtf),
        hle_export!(cbrt),
        hle_export!(cbrtf),
        hle_export!(hypot),
        hle_export!(hypotf),
        hle_export!(fabs),
        hle_export!(fabsf),
        hle_export!(floor),
        hle_export!(floorf),
        hle_export!(ceil),
        hle_export!(ceilf),
        hle_export!(round),
        hle_export!(roundf),
        hle_export!(trunc),
        hle_export!(truncf),
        hle_export!(rint),
        hle_export!(rintf),
        hle_export!(nearbyint),
        hle_export!(nearbyintf),
        hle_export!(lrint),
        hle_export!(lrintf),
        hle_export!(llrint),
        hle_export!(llrintf),
        hle_export!(lround),
        hle_export!(lroundf),
        hle_export!(llround),
        hle_export!(llroundf),
        hle_export!(fmod),
        hle_export!(fmodf),
        hle_export!(remainder),
        hle_export!(remainderf),
        hle_export!(remquo),
        hle_export!(remquof),
        hle_export!(frexp),
        hle_export!(frexpf),
        hle_export!(ldexp),
        hle_export!(ldexpf),
        hle_export!(scalbn),
        hle_export!(scalbnf),
        hle_export!(scalbln),
        hle_export!(scalblnf),
        hle_export!(modf),
        hle_export!(modff),
        hle_export!(fmax),
        hle_export!(fmaxf),
        hle_export!(fmin),
        hle_export!(fminf),
        hle_export!(fdim),
        hle_export!(fdimf),
        hle_export!(fma),
        hle_export!(fmaf),
        hle_export!(copysign),
        hle_export!(copysignf),
        hle_export!(nan),
        hle_export!(nanf),
        hle_export!(nextafter),
        hle_export!(nextafterf),
        hle_export!(nexttoward),
        hle_export!(nexttowardf),
        hle_export!(erf),
        hle_export!(erff),
        hle_export!(erfc),
        hle_export!(erfcf),
        hle_export!(tgamma),
        hle_export!(tgammaf),
        hle_export!(lgamma),
        hle_export!(lgammaf),
        hle_export!(logb),
        hle_export!(logbf),
        hle_export!(ilogb),
        hle_export!(ilogbf),
        hle_export!(_dclass),
        hle_export!(_fdclass),
        hle_export!(_dsign),
        hle_export!(_fdsign),
        hle_export!(_dtest),
        hle_export!(_fdtest),
        hle_export!(_ldtest),
        hle_export!(_dpcomp),
        hle_export!(_fdpcomp),
        hle_export!(_finite),
        hle_export!(_finitef),
        hle_export!(_isnan),
        hle_export!(_isnanf),
        hle_export!(_chgsign),
        hle_export!(_chgsignf),
        hle_export!(_copysign),
        hle_export!(_copysignf),
        hle_export!(_hypot),
        hle_export!(_hypotf),
        hle_export!(_logb),
        hle_export!(_logbf),
        hle_export!(_nextafter),
        hle_export!(_nextafterf),
        hle_export!(_scalb),
        hle_export!(_scalbf),
        hle_export!(_j0),
        hle_export!(_j1),
        hle_export!(_jn),
        hle_export!(_y0),
        hle_export!(_y1),
        hle_export!(_yn),
        hle_export!(abs),
        hle_export!(labs),
        hle_export!(llabs),
        hle_export!(_abs64),
        hle_export!(div),
        hle_export!(ldiv),
        hle_export!(lldiv),
        hle_export!(_set_SSE2_enable),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_agree_with_rust() {
        for x in [-2.5f64, -0.75, 0.0, 0.3, 1.0, 2.5, 10.0] {
            assert_eq!(sin(x), x.sin());
            assert_eq!(atan2(x, 1.5), x.atan2(1.5));
            assert_eq!(cbrt(x), x.cbrt());
            assert_eq!(round(x), x.round());
            assert_eq!(rint(x), x.round_ties_even());
            assert_eq!(floorf(x as f32), (x as f32).floor());
            assert_eq!(fmod(x, 0.7), x % 0.7);
        }
        assert_eq!(lrint(2.5), 2);
        assert_eq!(lround(2.5), 3);
        assert_eq!(lround(-2.5), -3);
        assert_eq!(lrint(1e20), i32::MIN);
        assert_eq!(llround(f64::NAN), i64::MIN);
        assert_eq!(fdim(3.0, 5.0), 0.0);
        assert_eq!(remainder(5.0, 2.0), 1.0);
        assert_eq!(remainder(7.0, 2.0), -1.0);
        assert_eq!(remainder(-7.5, 2.0), 0.5);
        let mut q = 0;
        assert_eq!(remquo(29.0, 3.0, &mut q), -1.0);
        assert_eq!(q, 10 & 7);
        assert_eq!(remquo(-29.0, 3.0, &mut q), 1.0);
        assert_eq!(q, -2);
        assert!(remainder(1.0, 0.0).is_nan());
        let mut whole = 0.0;
        assert_eq!(modf(-3.25, &mut whole), -0.25);
        assert_eq!(whole, -3.0);
        assert_eq!(nextafter(1.0, 2.0), 1.0 + f64::EPSILON);
        assert_eq!(nextafter(0.0, -1.0), -f64::from_bits(1));
        assert_eq!(logb(8.0), 3.0);
        assert_eq!(ilogb(f64::MIN_POSITIVE / 4.0), -1024);
        assert_eq!(ilogb(0.0), ILOGB0);
    }

    #[test]
    fn frexp_and_ldexp_round_trip() {
        for x in [1.0f64, -3.5, 1e300, 1e-310, f64::MIN_POSITIVE, 123.456] {
            let mut e = 0;
            let m = frexp(x, &mut e);
            assert!((0.5..1.0).contains(&m.abs()), "{x}");
            assert_eq!(ldexp(m, e), x, "{x}");
        }
        let mut e = 7;
        assert_eq!(frexp(0.0, &mut e), 0.0);
        assert_eq!(e, 0);
        assert_eq!(ldexp(1.0, 1024), f64::INFINITY);
        assert_eq!(ldexp(1.0, -1074), f64::from_bits(1));
        assert_eq!(ldexp(f64::MAX, -2098), f64::from_bits(1));
        assert_eq!(ldexpf(1.0, -149), f32::from_bits(1));
        assert_eq!(scalbn(3.0, 4), 48.0);
    }

    #[test]
    fn classes_and_comparisons() {
        assert_eq!(_dclass(f64::NAN), FP_NAN);
        assert_eq!(_dclass(f64::INFINITY), FP_INFINITE);
        assert_eq!(_dclass(1.0), FP_NORMAL);
        assert_eq!(_dclass(1e-310), FP_SUBNORMAL);
        assert_eq!(_dclass(-0.0), FP_ZERO);
        assert_eq!(_fdclass(1e-40), FP_SUBNORMAL);
        assert_eq!(_dsign(-0.0), 1);
        assert_eq!(_dpcomp(1.0, 2.0), 1);
        assert_eq!(_dpcomp(2.0, 2.0), 2);
        assert_eq!(_dpcomp(3.0, 2.0), 4);
        assert_eq!(_dpcomp(f64::NAN, 2.0), 0);
        let mut x = 5.0;
        assert_eq!(_dtest(&mut x), FP_NORMAL);
    }

    #[test]
    fn integer_division_packs_as_the_abi_has_it() {
        let packed = div(-7, 2);
        assert_eq!(packed as i32, -3);
        assert_eq!((packed >> 32) as i32, -1);
        assert_eq!(div(1, 0), 0);
        assert_eq!(div(i32::MIN, -1) as i32, i32::MIN);
        let mut out = Lldiv { quot: 0, rem: 0 };
        assert_eq!(lldiv(&mut out, 100, 7), &raw mut out);
        assert_eq!(out, Lldiv { quot: 14, rem: 2 });
        assert_eq!(abs(-5), 5);
        assert_eq!(llabs(i64::MIN), i64::MIN);
    }

    #[test]
    fn erf_and_gamma_at_known_points() {
        let close = |a: f64, b: f64, tolerance: f64| (a - b).abs() <= tolerance * b.abs().max(1.0);
        let pi = std::f64::consts::PI;
        let cases = [
            (erf(0.5), 0.520_499_877_813_046_5),
            (erf(1.0), 0.842_700_792_949_714_9),
            (erf(2.0), 0.995_322_265_018_952_7),
            (erf(3.0), 0.999_977_909_503_001_4),
            (erf(-1.5), -0.966_105_146_475_310_7),
            (erfc(3.0), 2.209_049_699_858_544e-5),
            (erfc(0.25), 0.723_673_609_831_763_1),
            (tgamma(5.0), 24.0),
            (tgamma(0.5), pi.sqrt()),
            (tgamma(10.0), 362_880.0),
            (tgamma(-0.5), -2.0 * pi.sqrt()),
            (tgamma(21.0), 2_432_902_008_176_640_000.0),
            (tgamma(170.5) / tgamma(169.5), 169.5),
            (lgamma(100.0), 359.134_205_369_575_4),
            (lgamma(0.5), pi.sqrt().ln()),
        ];
        for (i, (got, want)) in cases.iter().enumerate() {
            assert!(close(*got, *want, 1e-13), "case {i}: {got} against {want}");
        }
        // erfc's relative accuracy far out.
        assert!(close(erfc(6.0) / 2.151_973_671_249_891_3e-17, 1.0, 1e-12));
        assert!(tgamma(-2.0).is_nan());
        assert_eq!(tgamma(-0.0), f64::NEG_INFINITY);
        assert_eq!(lgamma(0.0), f64::INFINITY);
        assert!(_j0(1.0).is_nan());
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 153);
    }
}
