//! The `printf` engine the stdio entry points share: a format string
//! (narrow or wide) and Win64 variadic arguments in, UTF-16 out through a
//! [`Sink`]. Conversions, flags and lengths are the Universal C Runtime's;
//! `f`, `e` and `g` are rounded exactly (round half to even on the binary
//! value, as the UCRT does with `_CRT_INTERNAL_PRINTF_STANDARD_ROUNDING`)
//! by Rust's own formatting of the `f64`.
//!
//! A narrow format's bytes and a narrow string argument's bytes are read
//! as Latin-1 (the title's ANSI code page, 1252, taken as Latin-1); a sink
//! that writes narrow output maps each unit below 256 back to its byte and
//! anything above to `?`.
//!
//! Limits: an invalid format writes nothing at all (it is checked whole
//! before any output; the UCRT writes up to the bad directive).

use std::sync::atomic::{AtomicBool, Ordering};

use super::runtime::{_invalid_parameter_noinfo, EINVAL};

/// `_CRT_INTERNAL_PRINTF_LEGACY_VSPRINTF_NULL_TERMINATION`.
pub(crate) const LEGACY_VSPRINTF_NULL_TERMINATION: u64 = 1;
/// `_CRT_INTERNAL_PRINTF_STANDARD_SNPRINTF_BEHAVIOR`.
pub(crate) const STANDARD_SNPRINTF_BEHAVIOR: u64 = 2;
/// `_CRT_INTERNAL_PRINTF_LEGACY_WIDE_SPECIFIERS`: in a wide format `%s` and
/// `%c` take wide arguments and `%S` and `%C` narrow ones (MSVC's meaning,
/// the headers' default); without it they follow ISO C (`%s` narrow).
pub(crate) const LEGACY_WIDE_SPECIFIERS: u64 = 4;
/// `_CRT_INTERNAL_PRINTF_LEGACY_THREE_DIGIT_EXPONENTS`.
pub(crate) const LEGACY_THREE_DIGIT_EXPONENTS: u64 = 16;
/// Ours, never passed by a title: positional arguments (`%1$d`) are
/// allowed, as the `_p` functions allow them.
pub(crate) const POSITIONAL: u64 = 1 << 63;

/// Whether `%n` is enabled (`_set_printf_count_output`); off by default.
pub(crate) static COUNT_OUTPUT: AtomicBool = AtomicBool::new(false);

/// Where formatted text goes.
pub(crate) trait Sink {
    /// Takes `text`; false when the output failed (the engine stops calling
    /// it but goes on counting).
    fn put(&mut self, text: &[u16]) -> bool;
}

/// A sink that keeps everything.
#[derive(Default)]
pub(crate) struct VecSink(pub(crate) Vec<u16>);

impl Sink for VecSink {
    fn put(&mut self, text: &[u16]) -> bool {
        self.0.extend_from_slice(text);
        true
    }
}

/// A format string's characters.
#[derive(Clone, Copy)]
pub(crate) enum Spec<'a> {
    Narrow(&'a [u8]),
    Wide(&'a [u16]),
}

impl Spec<'_> {
    pub(crate) fn len(&self) -> usize {
        match self {
            Spec::Narrow(s) => s.len(),
            Spec::Wide(s) => s.len(),
        }
    }

    /// The unit at `i` (a narrow byte as Latin-1); 0 past the end.
    pub(crate) fn at(&self, i: usize) -> u16 {
        match self {
            Spec::Narrow(s) => s.get(i).map_or(0, |&b| u16::from(b)),
            Spec::Wide(s) => s.get(i).copied().unwrap_or(0),
        }
    }

    pub(crate) fn is_wide(&self) -> bool {
        matches!(self, Spec::Wide(_))
    }
}

/// Win64 variadic arguments: a pointer into the caller's argument area,
/// every argument one 8-byte slot (a `double` too; `long double` is
/// `double`).
pub(crate) struct VaList(*const u64);

impl VaList {
    pub(crate) fn new(slots: *const u64) -> Self {
        VaList(slots)
    }

    /// The next slot.
    pub(crate) fn next_u64(&mut self) -> u64 {
        // SAFETY: the caller's argument area holds a slot for each argument
        // its format names.
        let value = unsafe { self.0.read_unaligned() };
        self.0 = self.0.wrapping_add(1);
        value
    }

    /// The next slot's low 32 bits.
    pub(crate) fn next_i32(&mut self) -> i32 {
        self.next_u64() as i32
    }

    /// The next slot's bits as a double.
    pub(crate) fn next_f64(&mut self) -> f64 {
        f64::from_bits(self.next_u64())
    }

    /// The next slot as a pointer.
    pub(crate) fn next_ptr<T>(&mut self) -> *mut T {
        self.next_u64() as usize as *mut T
    }
}

/// A length modifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Length {
    None,
    Hh,
    H,
    L,
    Ll,
    J,
    Z,
    T,
    BigL,
    W,
    I,
    I32,
    I64,
}

impl Length {
    /// The size in bytes of an integer of this length (`long` is 4 bytes on
    /// Windows; `size_t`, `ptrdiff_t`, `intmax_t` and `L` 8).
    pub(crate) fn integer_bytes(self) -> usize {
        match self {
            Length::Hh => 1,
            Length::H => 2,
            Length::None | Length::L | Length::W | Length::I32 => 4,
            Length::Ll
            | Length::J
            | Length::Z
            | Length::T
            | Length::BigL
            | Length::I
            | Length::I64 => 8,
        }
    }
}

/// Reads a length modifier at `*i`, moving past it.
pub(crate) fn parse_length(spec: &Spec, i: &mut usize) -> Length {
    let c = spec.at(*i);
    let next = spec.at(*i + 1);
    let (length, used) = match c {
        0x68 if next == 0x68 => (Length::Hh, 2),
        0x68 => (Length::H, 1),
        0x6C if next == 0x6C => (Length::Ll, 2),
        0x6C => (Length::L, 1),
        0x6A => (Length::J, 1),
        0x7A => (Length::Z, 1),
        0x74 => (Length::T, 1),
        0x4C => (Length::BigL, 1),
        0x77 => (Length::W, 1),
        0x49 if next == u16::from(b'3') && spec.at(*i + 2) == u16::from(b'2') => (Length::I32, 3),
        0x49 if next == u16::from(b'6') && spec.at(*i + 2) == u16::from(b'4') => (Length::I64, 3),
        0x49 => (Length::I, 1),
        _ => (Length::None, 0),
    };
    *i += used;
    length
}

/// Whether a character or string conversion takes a wide argument, as the
/// UCRT has it: `h` narrow, `l`/`w` wide; else, with `legacy` (MSVC's wide
/// specifiers), `c`/`s`/`Z`/`[` the format's width and `C`/`S` the other,
/// and without it (ISO C) `c`/`s`/`Z`/`[` narrow and `C`/`S` wide in every
/// format. A narrow format reads the same either way.
pub(crate) fn wide_argument(
    conversion: u16,
    length: Length,
    wide_format: bool,
    legacy: bool,
) -> bool {
    let other = conversion == u16::from(b'C') || conversion == u16::from(b'S');
    match length {
        Length::H | Length::Hh => false,
        Length::L | Length::W => true,
        _ if legacy => other != wide_format,
        _ => other,
    }
}

const LEFT: u8 = 1;
const PLUS: u8 = 2;
const SPACE: u8 = 4;
const ALTERNATE: u8 = 8;
const ZERO: u8 = 16;

/// A width or a precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Count {
    None,
    Fixed(usize),
    /// From an argument: the next one, or the one at a position.
    Star(Option<usize>),
}

#[derive(Debug, Clone, Copy)]
struct Directive {
    flags: u8,
    width: Count,
    precision: Count,
    length: Length,
    conversion: u16,
    /// The argument's position (from 1), for a positional format.
    position: Option<usize>,
}

enum Piece {
    /// Format characters from `start` to `end`, written as they are.
    Text(usize, usize),
    Directive(Directive),
}

const CONVERSIONS: &[u8] = b"diuoxXcCsSZpnfFeEgGaA%";

/// Reads decimal digits at `*i`; none read is `None`.
fn digits(spec: &Spec, i: &mut usize) -> Option<usize> {
    let start = *i;
    let mut value: usize = 0;
    while let Some(d) = char::from_u32(u32::from(spec.at(*i))).and_then(|c| c.to_digit(10)) {
        value = value.saturating_mul(10).saturating_add(d as usize);
        *i += 1;
    }
    (*i > start).then_some(value)
}

/// `n$` at `*i`, moving past it; else nothing moves.
fn position(spec: &Spec, i: &mut usize) -> Option<usize> {
    let mut j = *i;
    let n = digits(spec, &mut j)?;
    if n == 0 || spec.at(j) != u16::from(b'$') {
        return None;
    }
    *i = j + 1;
    Some(n)
}

/// A width or precision at `*i`: digits, or `*` with an optional `n$`.
fn count(spec: &Spec, i: &mut usize) -> Count {
    if spec.at(*i) == u16::from(b'*') {
        *i += 1;
        return Count::Star(position(spec, i));
    }
    digits(spec, i).map_or(Count::None, Count::Fixed)
}

/// The format's pieces; `None` for a bad format.
fn parse(spec: &Spec) -> Option<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut i = 0;
    let len = spec.len();
    while i < len {
        let start = i;
        while i < len && spec.at(i) != u16::from(b'%') {
            i += 1;
        }
        if i > start {
            pieces.push(Piece::Text(start, i));
        }
        if i >= len {
            break;
        }
        i += 1;
        if spec.at(i) == u16::from(b'%') {
            pieces.push(Piece::Text(i, i + 1));
            i += 1;
            continue;
        }
        let position = position(spec, &mut i);
        let mut flags = 0;
        loop {
            flags |= match spec.at(i) {
                0x2D => LEFT,
                0x2B => PLUS,
                0x20 => SPACE,
                0x23 => ALTERNATE,
                0x30 => ZERO,
                _ => break,
            };
            i += 1;
        }
        let width = count(spec, &mut i);
        let precision = if spec.at(i) == u16::from(b'.') {
            i += 1;
            match count(spec, &mut i) {
                Count::None => Count::Fixed(0),
                other => other,
            }
        } else {
            Count::None
        };
        let length = parse_length(spec, &mut i);
        let conversion = spec.at(i);
        if i >= len || !CONVERSIONS.iter().any(|&c| u16::from(c) == conversion) {
            return None;
        }
        i += 1;
        pieces.push(Piece::Directive(Directive {
            flags,
            width,
            precision,
            length,
            conversion,
            position,
        }));
    }
    Some(pieces)
}

/// Where the arguments come from: the list in order, or slots read up
/// front for a positional format.
enum Args<'a> {
    Sequential(&'a mut VaList),
    Positional(Vec<u64>),
}

impl Args<'_> {
    fn take(&mut self, position: Option<usize>) -> u64 {
        match self {
            Args::Sequential(list) => list.next_u64(),
            Args::Positional(slots) => position
                .and_then(|p| slots.get(p.wrapping_sub(1)))
                .copied()
                .unwrap_or(0),
        }
    }

    fn take_f64(&mut self, position: Option<usize>) -> f64 {
        match self {
            Args::Sequential(list) => list.next_f64(),
            Args::Positional(_) => f64::from_bits(self.take(position)),
        }
    }
}

/// Checks a positional format (every directive and `*` positional, every
/// position from 1 to the highest used) and reads its slots; `None` for a
/// bad one. A format without positions is `Some(None)`.
fn positional_slots(pieces: &[Piece], options: u64, list: &mut VaList) -> Option<Option<Vec<u64>>> {
    let directives: Vec<&Directive> = pieces
        .iter()
        .filter_map(|p| match p {
            Piece::Directive(d) => Some(d),
            Piece::Text(..) => None,
        })
        .collect();
    let stars = |d: &Directive| {
        [d.width, d.precision].into_iter().filter_map(|c| match c {
            Count::Star(p) => Some(p),
            _ => None,
        })
    };
    let any = directives
        .iter()
        .any(|d| d.position.is_some() || stars(d).any(|p| p.is_some()));
    if !any {
        return Some(None);
    }
    if options & POSITIONAL == 0 {
        return None;
    }
    let mut used = Vec::new();
    for d in &directives {
        used.push(d.position?);
        for star in stars(d) {
            used.push(star?);
        }
    }
    let highest = used.iter().copied().max().unwrap_or(0);
    if (1..=highest).any(|p| !used.contains(&p)) {
        return None;
    }
    Some(Some((0..highest).map(|_| list.next_u64()).collect()))
}

/// What the engine writes: the sink, the count, whether the sink failed.
struct Out<'a> {
    sink: &'a mut dyn Sink,
    total: usize,
    ok: bool,
}

impl Out<'_> {
    fn put(&mut self, text: &[u16]) {
        self.total += text.len();
        if self.ok && !text.is_empty() {
            self.ok = self.sink.put(text);
        }
    }

    fn repeat(&mut self, c: u16, n: usize) {
        if n > 0 {
            self.put(&vec![c; n]);
        }
    }

    /// `prefix` and `body` padded to `width`: spaces on the left, or on the
    /// right with `-`, or zeros between them with `0` where it applies.
    fn padded(&mut self, prefix: &[u16], body: &[u16], width: usize, flags: u8, zeros: bool) {
        let pad = width.saturating_sub(prefix.len() + body.len());
        if flags & LEFT != 0 {
            self.put(prefix);
            self.put(body);
            self.repeat(u16::from(b' '), pad);
        } else if zeros && flags & ZERO != 0 {
            self.put(prefix);
            self.repeat(u16::from(b'0'), pad);
            self.put(body);
        } else {
            self.repeat(u16::from(b' '), pad);
            self.put(prefix);
            self.put(body);
        }
    }
}

fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// The decimal point (`localeconv`'s; `.` in the one locale there is).
fn decimal_point() -> u16 {
    // SAFETY: `localeconv` returns its own conventions, whose first field is
    // the decimal point, a NUL-terminated string.
    let point = unsafe { **super::locale::localeconv().cast::<*const u8>() };
    if point == 0 {
        u16::from(b'.')
    } else {
        u16::from(point)
    }
}

/// Formats `spec` with the arguments of `args` into `sink`: the number of
/// characters the whole output has, whether or not the sink took them all,
/// or `Err(EINVAL)` for a bad format, after `_invalid_parameter_noinfo`.
pub(crate) fn format(
    options: u64,
    spec: &Spec,
    args: &mut VaList,
    sink: &mut dyn Sink,
) -> Result<usize, i32> {
    let invalid = || {
        _invalid_parameter_noinfo();
        Err(EINVAL)
    };
    let Some(pieces) = parse(spec) else {
        return invalid();
    };
    let Some(slots) = positional_slots(&pieces, options, args) else {
        return invalid();
    };
    let mut args = match slots {
        Some(slots) => Args::Positional(slots),
        None => Args::Sequential(args),
    };
    if !COUNT_OUTPUT.load(Ordering::Relaxed)
        && pieces
            .iter()
            .any(|p| matches!(p, Piece::Directive(d) if d.conversion == u16::from(b'n')))
    {
        return invalid();
    }
    let mut out = Out {
        sink,
        total: 0,
        ok: true,
    };
    for piece in &pieces {
        match piece {
            Piece::Text(start, end) => {
                let text: Vec<u16> = (*start..*end).map(|i| spec.at(i)).collect();
                out.put(&text);
            }
            Piece::Directive(d) => directive(options, spec.is_wide(), d, &mut args, &mut out),
        }
    }
    Ok(out.total)
}

/// Formats one directive.
fn directive(options: u64, wide_format: bool, d: &Directive, args: &mut Args, out: &mut Out) {
    let legacy = options & LEGACY_WIDE_SPECIFIERS != 0;
    let mut flags = d.flags;
    let width = match d.width {
        Count::None => 0,
        Count::Fixed(n) => n,
        Count::Star(p) => {
            let value = args.take(p) as i32;
            if value < 0 {
                flags |= LEFT;
            }
            value.unsigned_abs() as usize
        }
    };
    let precision = match d.precision {
        Count::None => None,
        Count::Fixed(n) => Some(n),
        Count::Star(p) => usize::try_from(args.take(p) as i32).ok(),
    };
    let c = d.conversion as u8;
    match c {
        b'%' => out.put(&[u16::from(b'%')]),
        b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'p' => {
            let raw = args.take(d.position);
            integer(c, d.length, raw, flags, width, precision, out);
        }
        b'c' | b'C' => {
            let raw = args.take(d.position);
            let unit = if wide_argument(d.conversion, d.length, wide_format, legacy) {
                raw as u16
            } else {
                u16::from(raw as u8)
            };
            out.padded(&[], &[unit], width, flags, true);
        }
        b's' | b'S' | b'Z' => {
            let raw = args.take(d.position) as usize;
            let wide = wide_argument(d.conversion, d.length, wide_format, legacy);
            let text = if c == b'Z' {
                counted_string(raw, wide, precision)
            } else {
                string(raw, wide, precision)
            };
            out.padded(&[], &text, width, flags, true);
        }
        b'n' => {
            let target = args.take(d.position) as usize;
            if target != 0 {
                let count = out.total;
                // SAFETY: the title's place for the count, of the size its
                // length names.
                unsafe {
                    match d.length.integer_bytes() {
                        1 => (target as *mut u8).write_unaligned(count as u8),
                        2 => (target as *mut u16).write_unaligned(count as u16),
                        4 => (target as *mut u32).write_unaligned(count as u32),
                        _ => (target as *mut u64).write_unaligned(count as u64),
                    }
                }
            }
        }
        _ => {
            let value = args.take_f64(d.position);
            let three = options & LEGACY_THREE_DIGIT_EXPONENTS != 0;
            let (prefix, body, finite) = float(c, value, flags, precision, three);
            out.padded(&prefix, &body, width, flags, finite);
        }
    }
}

/// An integer conversion (and `p`).
fn integer(
    c: u8,
    length: Length,
    raw: u64,
    flags: u8,
    width: usize,
    precision: Option<usize>,
    out: &mut Out,
) {
    if c == b'p' {
        let body = units(&format!("{raw:016X}"));
        out.padded(&[], &body, width, flags, true);
        return;
    }
    let bytes = length.integer_bytes();
    let signed = c == b'd' || c == b'i';
    let (negative, magnitude) = if signed {
        let value = match bytes {
            1 => i64::from(raw as i8),
            2 => i64::from(raw as i16),
            4 => i64::from(raw as i32),
            _ => raw as i64,
        };
        (value < 0, value.unsigned_abs())
    } else {
        let value = match bytes {
            1 => u64::from(raw as u8),
            2 => u64::from(raw as u16),
            4 => u64::from(raw as u32),
            _ => raw,
        };
        (false, value)
    };
    let mut text = match (precision, magnitude) {
        (Some(0), 0) => String::new(),
        _ => match c {
            b'o' => format!("{magnitude:o}"),
            b'x' => format!("{magnitude:x}"),
            b'X' => format!("{magnitude:X}"),
            _ => magnitude.to_string(),
        },
    };
    if let Some(p) = precision
        && text.len() < p
    {
        text = format!("{}{text}", "0".repeat(p - text.len()));
    }
    if c == b'o' && flags & ALTERNATE != 0 && !text.starts_with('0') {
        text.insert(0, '0');
    }
    let mut prefix = String::new();
    if signed {
        if negative {
            prefix.push('-');
        } else if flags & PLUS != 0 {
            prefix.push('+');
        } else if flags & SPACE != 0 {
            prefix.push(' ');
        }
    }
    if flags & ALTERNATE != 0 && magnitude != 0 {
        match c {
            b'x' => prefix.push_str("0x"),
            b'X' => prefix.push_str("0X"),
            _ => {}
        }
    }
    out.padded(
        &units(&prefix),
        &units(&text),
        width,
        flags,
        precision.is_none(),
    );
}

/// A NUL-terminated string at `address` (or `(null)`), at most `precision`
/// characters, read no further than that.
fn string(address: usize, wide: bool, precision: Option<usize>) -> Vec<u16> {
    let limit = precision.unwrap_or(usize::MAX);
    if address == 0 {
        return units("(null)").into_iter().take(limit).collect();
    }
    let mut text = Vec::new();
    while text.len() < limit {
        // SAFETY: the title's NUL-terminated string, read up to its NUL or
        // the precision.
        let unit = unsafe {
            if wide {
                (address as *const u16).add(text.len()).read_unaligned()
            } else {
                u16::from(*(address as *const u8).add(text.len()))
            }
        };
        if unit == 0 {
            break;
        }
        text.push(unit);
    }
    text
}

/// An `ANSI_STRING` or `UNICODE_STRING` at `address`: `{ u16 length (in
/// bytes), u16 maximum, buffer }`; `(null)` for a null one or a null
/// buffer.
fn counted_string(address: usize, wide: bool, precision: Option<usize>) -> Vec<u16> {
    let limit = precision.unwrap_or(usize::MAX);
    let null = || units("(null)").into_iter().take(limit).collect();
    if address == 0 {
        return null();
    }
    // SAFETY: the title's counted string.
    let (length, buffer) = unsafe {
        (
            (address as *const u16).read_unaligned(),
            ((address + 8) as *const usize).read_unaligned(),
        )
    };
    if buffer == 0 {
        return null();
    }
    let count = if wide { length / 2 } else { length };
    (0..usize::from(count).min(limit))
        .map(|i| {
            // SAFETY: within the counted string's `length` bytes.
            unsafe {
                if wide {
                    (buffer as *const u16).add(i).read_unaligned()
                } else {
                    u16::from(*(buffer as *const u8).add(i))
                }
            }
        })
        .collect()
}

/// A floating conversion: the sign, the rest, and whether the value is
/// finite (zero padding applies to finite values only).
fn float(
    c: u8,
    value: f64,
    flags: u8,
    precision: Option<usize>,
    three: bool,
) -> (Vec<u16>, Vec<u16>, bool) {
    let upper = c.is_ascii_uppercase();
    let negative = value.is_sign_negative();
    let mut sign = String::new();
    if negative {
        sign.push('-');
    } else if flags & PLUS != 0 {
        sign.push('+');
    } else if flags & SPACE != 0 {
        sign.push(' ');
    }
    let case = |text: String| if upper { text.to_uppercase() } else { text };
    if value.is_nan() {
        let bits = value.to_bits();
        let quiet = bits & (1 << 51) != 0;
        let payload = bits & ((1 << 51) - 1);
        let text = if !quiet {
            "nan(snan)"
        } else if negative && payload == 0 {
            "nan(ind)"
        } else {
            "nan"
        };
        return (units(&sign), units(&case(text.to_owned())), false);
    }
    if value.is_infinite() {
        return (units(&sign), units(&case("inf".to_owned())), false);
    }
    let magnitude = value.abs();
    let alternate = flags & ALTERNATE != 0;
    let point = decimal_point();
    let text = match c.to_ascii_lowercase() {
        b'f' => fixed(magnitude, precision.unwrap_or(6), alternate),
        b'e' => exponential(magnitude, precision.unwrap_or(6), alternate, three),
        b'g' => general(magnitude, precision.unwrap_or(6), alternate, three),
        _ => {
            let text = hexadecimal(magnitude, precision, alternate);
            sign.push_str("0x");
            text
        }
    };
    let mut body: Vec<u16> = units(&case(text));
    for unit in &mut body {
        if *unit == u16::from(b'.') {
            *unit = point;
        }
    }
    (units(&case(sign)), body, true)
}

/// `%f` of a non-negative finite value.
fn fixed(value: f64, precision: usize, alternate: bool) -> String {
    let mut text = format!("{value:.precision$}");
    if precision == 0 && alternate {
        text.push('.');
    }
    text
}

/// The exponent written as C has it: a sign and at least two digits (three
/// with the legacy option).
fn exponent_text(exponent: i32, three: bool) -> String {
    let sign = if exponent < 0 { '-' } else { '+' };
    let digits = if three { 3 } else { 2 };
    format!("{sign}{:0digits$}", exponent.unsigned_abs())
}

/// Rust's `{:.*e}` of a non-negative finite value: mantissa and exponent.
fn split_exponential(value: f64, precision: usize) -> (String, i32) {
    let text = format!("{value:.precision$e}");
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    (mantissa.to_owned(), exponent.parse().unwrap_or(0))
}

/// `%e` of a non-negative finite value.
fn exponential(value: f64, precision: usize, alternate: bool, three: bool) -> String {
    let (mut mantissa, exponent) = split_exponential(value, precision);
    if precision == 0 && alternate {
        mantissa.push('.');
    }
    format!("{mantissa}e{}", exponent_text(exponent, three))
}

/// Drops a fraction's trailing zeros, and the point if nothing is left
/// after it.
fn trim_fraction(text: &mut String) {
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
}

/// `%g` of a non-negative finite value: `%e` or `%f` by the exponent, with
/// trailing zeros dropped unless `#`.
fn general(value: f64, precision: usize, alternate: bool, three: bool) -> String {
    let p = precision.max(1);
    let x = if value == 0.0 {
        0
    } else {
        split_exponential(value, p - 1).1
    };
    if (p as i64) > i64::from(x) && x >= -4 {
        let mut text = fixed(value, (p as i64 - 1 - i64::from(x)) as usize, alternate);
        if !alternate {
            trim_fraction(&mut text);
        } else if !text.contains('.') {
            text.push('.');
        }
        text
    } else {
        let (mut mantissa, exponent) = split_exponential(value, p - 1);
        if !alternate {
            trim_fraction(&mut mantissa);
        } else if !mantissa.contains('.') {
            mantissa.push('.');
        }
        format!("{mantissa}e{}", exponent_text(exponent, three))
    }
}

/// `%a` of a non-negative finite value, after its `0x`: the lead digit,
/// the fraction (rounded to the precision, half to even), `p` and the
/// binary exponent.
fn hexadecimal(value: f64, precision: Option<usize>, alternate: bool) -> String {
    let bits = value.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i64;
    let mantissa = bits & ((1 << 52) - 1);
    let (lead, exponent) = match (value == 0.0, biased) {
        (true, _) => (0u64, 0i64),
        (false, 0) => (0, -1022),
        (false, _) => (1, biased - 1023),
    };
    // The lead digit and the 13 fraction digits, as one number.
    let all = lead << 52 | mantissa;
    let (lead, fraction) = match precision {
        Some(p) if p < 13 => {
            let shift = (13 - p) * 4;
            let mut keep = all >> shift;
            let rest = all & ((1u64 << shift) - 1);
            let half = 1u64 << (shift - 1);
            if rest > half || (rest == half && keep & 1 == 1) {
                keep += 1;
            }
            let digits = p * 4;
            let fraction = if p == 0 {
                String::new()
            } else {
                format!("{:0p$x}", keep & ((1u64 << digits) - 1))
            };
            (keep >> digits, fraction)
        }
        Some(p) => (lead, format!("{mantissa:013x}{}", "0".repeat(p - 13))),
        // No precision: all thirteen digits, as the UCRT prints them (C99
        // would trim the trailing zeros).
        None => (lead, format!("{mantissa:013x}")),
    };
    let point = if fraction.is_empty() && !alternate {
        ""
    } else {
        "."
    };
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{lead}{point}{fraction}p{sign}{}", exponent.unsigned_abs())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A test argument.
    pub(crate) enum Arg<'a> {
        I(i64),
        U(u64),
        F(f64),
        P(usize),
        S(&'a str),
        W(&'a [u16]),
    }

    /// The slots of `args`, strings leaked as NUL-terminated copies.
    pub(crate) fn slots(args: &[Arg]) -> Vec<u64> {
        args.iter()
            .map(|a| match a {
                Arg::I(v) => *v as u64,
                Arg::U(v) => *v,
                Arg::F(v) => v.to_bits(),
                Arg::P(v) => *v as u64,
                Arg::S(s) => {
                    let mut bytes = s.as_bytes().to_vec();
                    bytes.push(0);
                    Box::leak(bytes.into_boxed_slice()).as_ptr() as u64
                }
                Arg::W(w) => {
                    let mut units = w.to_vec();
                    units.push(0);
                    Box::leak(units.into_boxed_slice()).as_ptr() as u64
                }
            })
            .collect()
    }

    pub(crate) fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn run(options: u64, format_text: &str, args: &[Arg]) -> Result<(String, usize), i32> {
        let slots = slots(args);
        let mut list = VaList::new(slots.as_ptr());
        let mut sink = VecSink::default();
        let spec = Spec::Narrow(format_text.as_bytes());
        let n = format(options, &spec, &mut list, &mut sink)?;
        Ok((String::from_utf16_lossy(&sink.0), n))
    }

    fn printf(format_text: &str, args: &[Arg]) -> String {
        let (text, n) = run(0, format_text, args).unwrap();
        assert_eq!(n, text.encode_utf16().count(), "{format_text}");
        text
    }

    extern "win64" fn ignore(_: *const u16, _: *const u16, _: *const u16, _: u32, _: usize) {}

    #[test]
    fn conversions_and_flags_match_the_ucrt() {
        use Arg::*;
        let w = wide("wide");
        let cases: &[(&str, &[Arg], &str)] = &[
            ("%5.2f|", &[F(4.56789)], " 4.57|"),
            ("%-8s|", &[S("ab")], "ab      |"),
            ("%08.3e", &[F(-12345.678)], "-1.235e+04"),
            ("%012.3e", &[F(-12345.678)], "-001.235e+04"),
            ("%+d % d %d", &[I(5), I(5), I(-5)], "+5  5 -5"),
            (
                "%#x %#X %#o %#x %#o",
                &[U(255), U(255), U(8), U(0), U(0)],
                "0xff 0XFF 010 0 0",
            ),
            (
                "%llu %zu %I64d %I32d",
                &[U(u64::MAX), U(7), I(-1), U(0xFFFF_FFFF)],
                "18446744073709551615 7 -1 -1",
            ),
            (
                "%hhd %hd %hhu",
                &[I(0x1FF), I(0x18000), I(-1)],
                "-1 -32768 255",
            ),
            ("%lu %ld", &[U(0x1_0000_0001), I(0xFFFF_FFFF)], "1 -1"),
            (
                "%c%lc%C%wc",
                &[U(u64::from(b'a')), U(0x3A9), U(0x3A9), U(u64::from(b'z'))],
                "aΩΩz",
            ),
            (
                "%ls|%S|%ws|%hs",
                &[W(&w), W(&w), W(&w), S("n")],
                "wide|wide|wide|n",
            ),
            ("%p", &[P(0x1234_ABCD)], "000000001234ABCD"),
            ("%%|%5%|", &[], "%|%|"),
            (
                "%g %g %g %g",
                &[F(100000.0), F(1e6), F(0.0001), F(0.00001)],
                "100000 1e+06 0.0001 1e-05",
            ),
            (
                "%G %g %#g",
                &[F(1e-10), F(2.5), F(2.5)],
                "1E-10 2.5 2.50000",
            ),
            (
                "%a %A %a %.1a %.0a",
                &[F(3.0), F(-1.0), F(0.0), F(1.96875), F(1.5)],
                "0x1.8000000000000p+1 -0X1.0000000000000P+0 0x0.0000000000000p+0 0x2.0p+0 0x2p+0",
            ),
            ("%a", &[F(f64::from_bits(1))], "0x0.0000000000001p-1022"),
            (
                "%.0f %.0f %.0f %.2f %#.0f",
                &[F(2.5), F(3.5), F(0.5), F(0.125), F(2.0)],
                "2 4 0 0.12 2.",
            ),
            (
                "%.10g %.3g %.0e",
                &[F(1.0 / 3.0), F(1234.5), F(2.5)],
                "0.3333333333 1.23e+03 2e+00",
            ),
            ("%e %e", &[F(0.0), F(-0.0)], "0.000000e+00 -0.000000e+00"),
            ("%f %f", &[F(0.0), F(-0.0)], "0.000000 -0.000000"),
            ("%e", &[F(1e300)], "1.000000e+300"),
            ("%e", &[F(1e-300)], "1.000000e-300"),
            ("%g %g", &[F(1e300), F(1e-300)], "1e+300 1e-300"),
            (
                "%f|%F|%f|%E|%5f|%-6f|%+f",
                &[
                    F(f64::NAN),
                    F(f64::NAN),
                    F(-f64::NAN),
                    F(f64::INFINITY),
                    F(f64::NEG_INFINITY),
                    F(f64::INFINITY),
                    F(f64::INFINITY),
                ],
                "nan|NAN|-nan(ind)|INF| -inf|inf   |+inf",
            ),
            (
                "%f %F",
                &[
                    F(f64::from_bits(0x7FF0_0000_0000_0001)),
                    F(f64::from_bits(0xFFF8_0000_0000_0001)),
                ],
                "nan(snan) -NAN",
            ),
            (
                "%*d|%-*d|%*d|",
                &[I(5), I(42), I(4), I(7), I(-4), I(1)],
                "   42|7   |1   |",
            ),
            (
                "%.*s|%.*f",
                &[I(2), S("abcdef"), I(-1), F(1.5)],
                "ab|1.500000",
            ),
            ("%s|%.3s|%ls", &[P(0), P(0), P(0)], "(null)|(nu|(null)"),
            (
                "%.3d|%.0d|%5.3d|%05d|%-05d|%05.1d",
                &[I(7), I(0), I(-7), I(-42), I(3), I(9)],
                "007|| -007|-0042|3    |    9",
            ),
            (
                "%05s|%3c|%-3c|",
                &[S("ab"), U(u64::from(b'x')), U(u64::from(b'y'))],
                "000ab|  x|y  |",
            ),
            (
                "%x %X %o %u",
                &[I(-1), U(0xABC), U(8), I(-1)],
                "ffffffff ABC 10 4294967295",
            ),
            ("%jd %td %Ld", &[I(-3), I(-4), I(-5)], "-3 -4 -5"),
        ];
        for (format_text, args, expected) in cases {
            assert_eq!(printf(format_text, args), *expected, "{format_text}");
        }
        let three = run(LEGACY_THREE_DIGIT_EXPONENTS, "%e %g", &[F(1e5), F(1e-7)]).unwrap();
        assert_eq!(three.0, "1.000000e+005 1e-007");
    }

    #[test]
    fn counted_strings_print_their_length() {
        #[repr(C)]
        struct Counted {
            length: u16,
            maximum: u16,
            buffer: *const u8,
        }
        let text = b"hello world";
        let ansi = Counted {
            length: 5,
            maximum: 11,
            buffer: text.as_ptr(),
        };
        let units = wide("wide text");
        let unicode = Counted {
            length: 8,
            maximum: 18,
            buffer: units.as_ptr().cast(),
        };
        let null = Counted {
            length: 4,
            maximum: 4,
            buffer: std::ptr::null(),
        };
        let out = printf(
            "%Z|%wZ|%Z|%Z",
            &[
                Arg::P(&raw const ansi as usize),
                Arg::P(&raw const unicode as usize),
                Arg::P(&raw const null as usize),
                Arg::P(0),
            ],
        );
        assert_eq!(out, "hello|wide|(null)|(null)");
    }

    #[test]
    fn positional_arguments_need_the_option_and_every_position() {
        let _ = super::super::runtime::_set_thread_local_invalid_parameter_handler(Some(ignore));
        let args = [Arg::I(42), Arg::S("x")];
        assert_eq!(run(POSITIONAL, "%2$s %1$d", &args).unwrap().0, "x 42");
        assert_eq!(
            run(POSITIONAL, "%1$d %1$d %2$s", &args).unwrap().0,
            "42 42 x"
        );
        assert_eq!(
            run(POSITIONAL, "%2$*1$d|", &[Arg::I(4), Arg::I(7)])
                .unwrap()
                .0,
            "   7|"
        );
        assert_eq!(run(0, "%2$s %1$d", &args), Err(EINVAL));
        assert_eq!(run(POSITIONAL, "%2$s %d", &args), Err(EINVAL));
        assert_eq!(run(POSITIONAL, "%2$s", &args), Err(EINVAL));
        assert_eq!(run(0, "%y", &[]), Err(EINVAL));
        assert_eq!(run(0, "abc%", &[]), Err(EINVAL));
        assert_eq!(run(0, "%n", &[Arg::P(0)]), Err(EINVAL));
        // With the count enabled, `%n` stores the characters so far.
        let mut count: i32 = -1;
        let mut short: i16 = -1;
        COUNT_OUTPUT.store(true, Ordering::Relaxed);
        let target = [
            Arg::P(&raw mut count as usize),
            Arg::P(&raw mut short as usize),
        ];
        let printed = run(0, "abc%nde%hn", &target);
        COUNT_OUTPUT.store(false, Ordering::Relaxed);
        assert_eq!(printed.unwrap().0, "abcde");
        assert_eq!((count, short), (3, 5));
        let _ = super::super::runtime::_set_thread_local_invalid_parameter_handler(None);
    }

    #[test]
    fn wide_formats_swap_the_character_widths() {
        let narrow = slots(&[
            Arg::S("nar"),
            Arg::W(&wide("wid")),
            Arg::U(0x41),
            Arg::U(0x3A9),
        ]);
        let mut list = VaList::new(narrow.as_ptr());
        let mut sink = VecSink::default();
        let format_units = wide("%S %s %C %c");
        let legacy = LEGACY_WIDE_SPECIFIERS;
        format(legacy, &Spec::Wide(&format_units), &mut list, &mut sink).unwrap();
        assert_eq!(String::from_utf16_lossy(&sink.0), "nar wid A Ω");
    }

    #[test]
    fn wide_formats_follow_iso_without_the_legacy_bit() {
        let args = slots(&[
            Arg::W(&wide("wid")),
            Arg::S("nar"),
            Arg::U(0x3A9),
            Arg::U(0x41),
            Arg::S("hs"),
            Arg::W(&wide("ls")),
        ]);
        let mut list = VaList::new(args.as_ptr());
        let mut sink = VecSink::default();
        let format_units = wide("%S %s %C %c %hs %ls");
        format(0, &Spec::Wide(&format_units), &mut list, &mut sink).unwrap();
        assert_eq!(String::from_utf16_lossy(&sink.0), "wid nar Ω A hs ls");
    }

    #[test]
    fn narrow_formats_read_the_same_either_way() {
        for options in [0, LEGACY_WIDE_SPECIFIERS] {
            let args = slots(&[Arg::S("nar"), Arg::W(&wide("wid")), Arg::U(0x41)]);
            let mut list = VaList::new(args.as_ptr());
            let mut sink = VecSink::default();
            format(options, &Spec::Narrow(b"%s %S %c"), &mut list, &mut sink).unwrap();
            assert_eq!(String::from_utf16_lossy(&sink.0), "nar wid A");
        }
    }

    /// A sink that takes `room` units, then fails.
    struct Short {
        room: usize,
        taken: Vec<u16>,
    }

    impl Sink for Short {
        fn put(&mut self, text: &[u16]) -> bool {
            let n = text.len().min(self.room - self.taken.len());
            self.taken.extend_from_slice(&text[..n]);
            n == text.len()
        }
    }

    #[test]
    fn the_count_goes_on_after_the_sink_fails() {
        let args = slots(&[Arg::I(12345)]);
        let mut list = VaList::new(args.as_ptr());
        let mut sink = Short {
            room: 3,
            taken: Vec::new(),
        };
        let n = format(0, &Spec::Narrow(b"ab%dcd"), &mut list, &mut sink).unwrap();
        assert_eq!((n, sink.taken.as_slice()), (9, &wide("ab1")[..]));
    }
}
