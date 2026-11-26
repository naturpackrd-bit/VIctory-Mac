//! The `scanf` engine the stdio entry points share: a format string
//! (narrow or wide, [`Spec`]), Win64 variadic arguments (pointers, and the
//! buffer sizes of the secure forms) and characters from a [`Source`].
//! Floating-point numbers go through `string.rs`'s `strtod` scanner
//! (`wcstod`) on the longest prefix the input offers.
//!
//! Narrow input is read as Latin-1 (each byte one character) and a narrow
//! target gets each character below 256 as its byte, anything above as `?`.
//!
//! Limits: integers wrap on overflow (as the UCRT's accumulator does); the
//! return value is EOF only when input ended before any conversion was
//! stored.

use super::format::{Length, Spec, VaList, parse_length, wide_argument};
use super::runtime::{_invalid_parameter_noinfo, EINVAL, set_errno};

/// `_CRT_INTERNAL_SCANF_SECURECRT`: `%c`, `%s` and `%[` take a buffer size.
pub(crate) const SECURECRT: u64 = 1;

/// Where the characters come from.
pub(crate) trait Source {
    /// The next character; none at the end of the input or on a failure.
    fn get(&mut self) -> Option<u16>;
    /// Gives back the character `get` returned last (and, called again,
    /// the one before it).
    fn unget(&mut self, c: u16);
}

/// A source over a string's units: up to `limit` or a NUL.
pub(crate) struct UnitSource<'a> {
    pub(crate) units: &'a [u16],
    pub(crate) at: usize,
}

impl Source for UnitSource<'_> {
    fn get(&mut self) -> Option<u16> {
        let c = *self.units.get(self.at)?;
        if c == 0 {
            return None;
        }
        self.at += 1;
        Some(c)
    }

    fn unget(&mut self, _c: u16) {
        self.at = self.at.saturating_sub(1);
    }
}

/// The input with its count of characters consumed (`%n`) and whether it
/// ended.
struct Input<'a> {
    source: &'a mut dyn Source,
    consumed: usize,
    ended: bool,
}

impl Input<'_> {
    fn get(&mut self) -> Option<u16> {
        let c = self.source.get();
        match c {
            Some(_) => self.consumed += 1,
            None => self.ended = true,
        }
        c
    }

    fn unget(&mut self, c: u16) {
        self.consumed = self.consumed.saturating_sub(1);
        self.source.unget(c);
    }

    /// Skips whitespace.
    fn skip_space(&mut self) {
        while let Some(c) = self.get() {
            if !is_space(c) {
                self.unget(c);
                break;
            }
        }
    }
}

/// A field: the input, read at most `budget` more characters.
struct Field<'a, 'b> {
    input: &'a mut Input<'b>,
    budget: usize,
}

impl Field<'_, '_> {
    /// The next character if `accept` takes it; else it stays in the
    /// input.
    fn take(&mut self, accept: impl Fn(u16) -> bool) -> Option<u16> {
        if self.budget == 0 {
            return None;
        }
        let c = self.input.get()?;
        if accept(c) {
            self.budget -= 1;
            Some(c)
        } else {
            self.input.unget(c);
            None
        }
    }
}

fn is_space(c: u16) -> bool {
    c == 0x20 || (0x09..=0x0D).contains(&c)
}

fn is(c: u16, ascii: u8) -> bool {
    c == u16::from(ascii)
}

fn digit(c: u16, base: u32) -> Option<u32> {
    char::from_u32(u32::from(c))?.to_digit(base)
}

/// Why the scan stopped early.
enum Stop {
    /// The input did not match.
    Mismatch,
    /// The input ended (or failed).
    End,
    /// A bad format or a secure buffer too small: the count so far, or -1
    /// for a bad format.
    Invalid(i32),
}

/// Reads an integer in `base` (0: by its prefix, as `%i`): a sign, then
/// digits; the value wraps. `None` when no digit was read.
fn read_integer(field: &mut Field, mut base: u32) -> Option<u64> {
    let mut negative = false;
    if let Some(sign) = field.take(|c| is(c, b'-') || is(c, b'+')) {
        negative = is(sign, b'-');
    }
    let mut digits = 0;
    let mut value: u64 = 0;
    if base == 0 || base == 16 {
        if field.take(|c| is(c, b'0')).is_some() {
            digits = 1;
            if field.take(|c| is(c, b'x') || is(c, b'X')).is_some() {
                base = 16;
            } else if base == 0 {
                base = 8;
            }
        } else if base == 0 {
            base = 10;
        }
    }
    while let Some(c) = field.take(|c| digit(c, base).is_some()) {
        let d = digit(c, base).unwrap_or(0);
        value = value
            .wrapping_mul(u64::from(base))
            .wrapping_add(u64::from(d));
        digits += 1;
    }
    (digits > 0).then(|| {
        if negative {
            value.wrapping_neg()
        } else {
            value
        }
    })
}

/// Whether `text` (ASCII, lower-case) could still grow into a number
/// `strtod` reads: a sign, then `inf`/`infinity`, `nan`/`nan(chars)`, a
/// hexadecimal `0x` number with a `p` exponent, or a decimal one with an
/// `e` exponent.
fn viable(text: &[u8]) -> bool {
    let rest = match text.first() {
        Some(b'+' | b'-') => &text[1..],
        _ => text,
    };
    if rest.is_empty() {
        return true;
    }
    if b"infinity".starts_with(rest) {
        return true;
    }
    if b"nan".starts_with(rest) {
        return true;
    }
    if let Some(inside) = rest.strip_prefix(b"nan(") {
        let (body, closed) = match inside.iter().position(|&c| c == b')') {
            Some(at) => (&inside[..at], at + 1 == inside.len()),
            None => (inside, true),
        };
        return closed && body.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_');
    }
    let (hex, body) = match rest.strip_prefix(b"0x") {
        Some(body) => (true, body),
        None => (false, rest),
    };
    let is_digit = |c: u8| {
        if hex {
            c.is_ascii_hexdigit()
        } else {
            c.is_ascii_digit()
        }
    };
    let marker = if hex { b'p' } else { b'e' };
    let mut i = 0;
    let mut digits = 0;
    while i < body.len() && is_digit(body[i]) {
        i += 1;
        digits += 1;
    }
    if i < body.len() && body[i] == b'.' {
        i += 1;
        while i < body.len() && is_digit(body[i]) {
            i += 1;
            digits += 1;
        }
    }
    if i == body.len() {
        return true;
    }
    if body[i] != marker || (digits == 0 && !hex) {
        return false;
    }
    if hex && digits == 0 {
        return false;
    }
    i += 1;
    if i < body.len() && (body[i] == b'+' || body[i] == b'-') {
        i += 1;
    }
    body[i..].iter().all(u8::is_ascii_digit)
}

/// Reads a floating-point number: the longest viable prefix, then what
/// `wcstod` takes of it; the rest goes back to the input.
fn read_float(field: &mut Field) -> Option<f64> {
    let mut text: Vec<u16> = Vec::new();
    let mut ascii: Vec<u8> = Vec::new();
    loop {
        let next = field.take(|c| {
            let Ok(b) = u8::try_from(c) else { return false };
            if !b.is_ascii() {
                return false;
            }
            let mut candidate = ascii.clone();
            candidate.push(b.to_ascii_lowercase());
            viable(&candidate)
        });
        let Some(c) = next else { break };
        text.push(c);
        ascii.push((c as u8).to_ascii_lowercase());
    }
    let mut terminated = text.clone();
    terminated.push(0);
    let mut end: *mut u16 = std::ptr::null_mut();
    let value = super::string::wcstod(terminated.as_ptr(), &mut end);
    let used = (end as usize).saturating_sub(terminated.as_ptr() as usize) / 2;
    for &c in text[used.min(text.len())..].iter().rev() {
        field.input.unget(c);
    }
    (used > 0).then_some(value)
}

/// A `%[` set: whether it is negated, and its ranges.
struct Set {
    negated: bool,
    ranges: Vec<(u16, u16)>,
}

impl Set {
    fn contains(&self, c: u16) -> bool {
        self.ranges.iter().any(|&(a, b)| (a..=b).contains(&c)) != self.negated
    }
}

/// Reads a `%[` set after its `[` at `*i`, moving past its `]`; `None` if
/// it is not closed.
fn parse_set(spec: &Spec, i: &mut usize) -> Option<Set> {
    let len = spec.len();
    let negated = spec.at(*i) == u16::from(b'^');
    if negated {
        *i += 1;
    }
    let mut members: Vec<u16> = Vec::new();
    if spec.at(*i) == u16::from(b']') {
        members.push(u16::from(b']'));
        *i += 1;
    }
    while *i < len && spec.at(*i) != u16::from(b']') {
        members.push(spec.at(*i));
        *i += 1;
    }
    if *i >= len {
        return None;
    }
    *i += 1;
    let mut ranges = Vec::new();
    let mut k = 0;
    while k < members.len() {
        let c = members[k];
        if k + 2 < members.len() && members[k + 1] == u16::from(b'-') {
            let (a, b) = (c, members[k + 2]);
            ranges.push((a.min(b), a.max(b)));
            k += 3;
        } else {
            ranges.push((c, c));
            k += 1;
        }
    }
    Some(Set { negated, ranges })
}

/// Stores `value` at the next pointer, in the size `length` names.
fn store_integer(args: &mut VaList, length: Length, value: u64) {
    let target = args.next_ptr::<u8>() as usize;
    if target == 0 {
        return;
    }
    // SAFETY: the title's place for the value, of the size its length
    // names.
    unsafe {
        match length.integer_bytes() {
            1 => (target as *mut u8).write_unaligned(value as u8),
            2 => (target as *mut u16).write_unaligned(value as u16),
            4 => (target as *mut u32).write_unaligned(value as u32),
            _ => (target as *mut u64).write_unaligned(value),
        }
    }
}

/// Stores characters at the next pointer (with the next slot its size in
/// characters, for the secure forms), and a NUL if asked; false when a
/// secure buffer is too small (an empty string is stored then).
fn store_text(args: &mut VaList, secure: bool, wide: bool, text: &[u16], nul: bool) -> bool {
    let target = args.next_ptr::<u8>() as usize;
    let size = if secure {
        args.next_i32() as u32 as usize
    } else {
        usize::MAX
    };
    if target == 0 {
        return true;
    }
    let needed = text.len() + usize::from(nul);
    if needed > size {
        if size > 0 {
            // SAFETY: the title's buffer of `size` characters.
            unsafe {
                if wide {
                    (target as *mut u16).write_unaligned(0);
                } else {
                    *(target as *mut u8) = 0;
                }
            }
        }
        return false;
    }
    let terminator = nul.then_some(0);
    for (k, &c) in text.iter().chain(terminator.iter()).enumerate() {
        // SAFETY: the title's buffer, which holds `needed` characters.
        unsafe {
            if wide {
                (target as *mut u16).add(k).write_unaligned(c);
            } else {
                *(target as *mut u8).add(k) = u8::try_from(c).unwrap_or(b'?');
            }
        }
    }
    true
}

/// Scans `source` as `spec` says, storing through the pointers in `args`:
/// the number of conversions stored, or -1 (EOF) if the input ended before
/// one was (or the format is bad: `_invalid_parameter_noinfo`, `EINVAL`).
/// A secure buffer too small stores an empty string and stops with the
/// count so far, after `_invalid_parameter_noinfo`.
pub(crate) fn scan(options: u64, spec: &Spec, args: &mut VaList, source: &mut dyn Source) -> i32 {
    let mut input = Input {
        source,
        consumed: 0,
        ended: false,
    };
    let mut stored = 0;
    let stop = run(options, spec, args, &mut input, &mut stored);
    match stop {
        None | Some(Stop::Mismatch) => stored,
        Some(Stop::End) if stored == 0 => -1,
        Some(Stop::End) => stored,
        Some(Stop::Invalid(result)) => {
            if result < 0 {
                set_errno(EINVAL);
            }
            _invalid_parameter_noinfo();
            result
        }
    }
}

/// The scan's body; why it stopped early, if it did.
fn run(
    options: u64,
    spec: &Spec,
    args: &mut VaList,
    input: &mut Input,
    stored: &mut i32,
) -> Option<Stop> {
    let secure = options & SECURECRT != 0;
    let wide_format = spec.is_wide();
    let len = spec.len();
    let mut i = 0;
    while i < len {
        let c = spec.at(i);
        if is_space(c) {
            while i < len && is_space(spec.at(i)) {
                i += 1;
            }
            input.skip_space();
            continue;
        }
        if c != u16::from(b'%') {
            i += 1;
            match input.get() {
                Some(x) if x == c => continue,
                Some(x) => {
                    input.unget(x);
                    return Some(Stop::Mismatch);
                }
                None => return Some(Stop::End),
            }
        }
        i += 1;
        let suppress = spec.at(i) == u16::from(b'*');
        if suppress {
            i += 1;
        }
        let mut width = 0usize;
        while let Some(d) = digit(spec.at(i), 10) {
            width = width.saturating_mul(10).saturating_add(d as usize);
            i += 1;
        }
        let length = parse_length(spec, &mut i);
        if i >= len {
            return Some(Stop::Invalid(-1));
        }
        let conversion = spec.at(i);
        i += 1;
        let budget = if width == 0 { usize::MAX } else { width };
        let conversion = u8::try_from(conversion).unwrap_or(0);
        if !matches!(conversion, b'c' | b'[' | b'n') {
            input.skip_space();
        }
        let wide = wide_argument(u16::from(conversion), length, wide_format);
        match conversion {
            b'%' => match input.get() {
                Some(x) if is(x, b'%') => {}
                Some(x) => {
                    input.unget(x);
                    return Some(Stop::Mismatch);
                }
                None => return Some(Stop::End),
            },
            b'n' => {
                if !suppress {
                    store_integer(args, length, input.consumed as u64);
                }
            }
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'p' => {
                let base = match conversion {
                    b'd' | b'u' => 10,
                    b'i' => 0,
                    b'o' => 8,
                    _ => 16,
                };
                let mut field = Field {
                    input: &mut *input,
                    budget,
                };
                let Some(value) = read_integer(&mut field, base) else {
                    return Some(if input.ended {
                        Stop::End
                    } else {
                        Stop::Mismatch
                    });
                };
                if !suppress {
                    let length = if conversion == b'p' {
                        Length::Ll
                    } else {
                        length
                    };
                    store_integer(args, length, value);
                    *stored += 1;
                }
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A' => {
                let mut field = Field {
                    input: &mut *input,
                    budget,
                };
                let Some(value) = read_float(&mut field) else {
                    return Some(if input.ended {
                        Stop::End
                    } else {
                        Stop::Mismatch
                    });
                };
                if !suppress {
                    let target = args.next_ptr::<u8>() as usize;
                    if target != 0 {
                        // SAFETY: the title's float or double, as its length
                        // names.
                        unsafe {
                            match length {
                                Length::L | Length::Ll | Length::BigL => {
                                    (target as *mut f64).write_unaligned(value)
                                }
                                _ => (target as *mut f32).write_unaligned(value as f32),
                            }
                        }
                    }
                    *stored += 1;
                }
            }
            b'c' | b'C' | b's' | b'S' | b'[' => {
                let set = if conversion == b'[' {
                    match parse_set(spec, &mut i) {
                        Some(set) => Some(set),
                        None => return Some(Stop::Invalid(-1)),
                    }
                } else {
                    None
                };
                let budget = if width == 0 && matches!(conversion, b'c' | b'C') {
                    1
                } else {
                    budget
                };
                let mut field = Field {
                    input: &mut *input,
                    budget,
                };
                let mut text = Vec::new();
                while let Some(c) = field.take(|c| match conversion {
                    b'c' | b'C' => true,
                    b'[' => set.as_ref().is_some_and(|s| s.contains(c)),
                    _ => !is_space(c),
                }) {
                    text.push(c);
                }
                if text.is_empty() {
                    return Some(if input.ended {
                        Stop::End
                    } else {
                        Stop::Mismatch
                    });
                }
                if !suppress {
                    let nul = !matches!(conversion, b'c' | b'C');
                    if !store_text(args, secure, wide, &text, nul) {
                        return Some(Stop::Invalid(*stored));
                    }
                    *stored += 1;
                }
            }
            _ => return Some(Stop::Invalid(-1)),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::format::tests::{Arg, slots, wide};
    use super::*;

    fn sscanf(options: u64, input: &str, format: &str, args: &[Arg]) -> i32 {
        let units = wide(input);
        let mut source = UnitSource {
            units: &units,
            at: 0,
        };
        let slots = slots(args);
        let mut list = VaList::new(slots.as_ptr());
        scan(
            options,
            &Spec::Narrow(format.as_bytes()),
            &mut list,
            &mut source,
        )
    }

    fn at<T>(target: &mut T) -> Arg<'static> {
        Arg::P(target as *mut T as usize)
    }

    extern "win64" fn ignore(_: *const u16, _: *const u16, _: *const u16, _: u32, _: usize) {}

    #[test]
    fn numbers_words_and_literals_are_read() {
        let (mut a, mut b, mut c, mut d) = (0i32, [0u8; 8], 0f64, 0i32);
        let mut e = 0u32;
        let n = sscanf(
            0,
            "12 ab 3.5 -7 0x1F %",
            "%d %s %lf %i %x %%",
            &[at(&mut a), at(&mut b), at(&mut c), at(&mut d), at(&mut e)],
        );
        assert_eq!(n, 5);
        assert_eq!((a, &b[..3], c, d, e), (12, &b"ab\0"[..], 3.5, -7, 0x1F));

        let (mut octal, mut hex, mut dec, mut f) = (0i32, 0i32, 0i32, 0f32);
        let args = [at(&mut octal), at(&mut hex), at(&mut dec), at(&mut f)];
        assert_eq!(sscanf(0, "010 0x10 +10 -2.5e1", "%i %i %i %f", &args), 4);
        assert_eq!((octal, hex, dec, f), (8, 16, 10, -25.0));

        let (mut small, mut short, mut long, mut unsigned) = (0i8, 0i16, 0i64, 0u32);
        let args = [
            at(&mut small),
            at(&mut short),
            at(&mut long),
            at(&mut unsigned),
        ];
        assert_eq!(
            sscanf(0, "-5 300 -9000000000 17", "%hhd %hd %lld %o", &args),
            4
        );
        assert_eq!(
            (small, short, long, unsigned),
            (-5, 300, -9_000_000_000, 15)
        );

        let mut pointer = 0usize;
        assert_eq!(sscanf(0, "00000000DEADBEEF", "%p", &[at(&mut pointer)]), 1);
        assert_eq!(pointer, 0xDEAD_BEEF);

        let (mut inf, mut nan, mut hexf) = (0f64, 0f64, 0f64);
        let args = [at(&mut inf), at(&mut nan), at(&mut hexf)];
        assert_eq!(sscanf(0, "-inf nan 0x1.8p1", "%lf %lf %lf", &args), 3);
        assert!(inf == f64::NEG_INFINITY && nan.is_nan() && hexf == 3.0);

        // `1e` is a number `1` followed by `e`.
        let (mut one, mut letter) = (0f64, [0u8; 4]);
        assert_eq!(
            sscanf(0, "1ex", "%lf%s", &[at(&mut one), at(&mut letter)]),
            2
        );
        assert_eq!((one, &letter[..3]), (1.0, &b"ex\0"[..]));
    }

    #[test]
    fn widths_sets_counts_and_suppression() {
        let (mut a, mut b, mut count) = (0i32, 0i32, 0i32);
        let args = [at(&mut a), at(&mut b), at(&mut count)];
        assert_eq!(sscanf(0, "12345", "%2d%*1d%d%n", &args), 2);
        assert_eq!((a, b, count), (12, 45, 5));

        let (mut set, mut rest) = ([0u8; 8], [0u8; 8]);
        assert_eq!(
            sscanf(0, "abcabd,x", "%[a-c]%[^,]", &[at(&mut set), at(&mut rest)]),
            2
        );
        assert_eq!((&set[..6], &rest[..2]), (&b"abcab\0"[..], &b"d\0"[..]));

        let (mut bracket, mut tail) = ([0u8; 8], [0u8; 8]);
        assert_eq!(
            sscanf(0, "]]-a!", "%[]-]%s", &[at(&mut bracket), at(&mut tail)]),
            2
        );
        assert_eq!((&bracket[..4], &tail[..3]), (&b"]]-\0"[..], &b"a!\0"[..]));

        let mut chars = [b'.'; 4];
        assert_eq!(sscanf(0, " xyz", "%3c", &[at(&mut chars)]), 1);
        assert_eq!(&chars, b" xy.");

        let mut word = [0u16; 8];
        assert_eq!(sscanf(0, "  héllo w", "%ls", &[at(&mut word)]), 1);
        assert_eq!(String::from_utf16_lossy(&word[..5]), "héllo");
        let mut narrow = [0u8; 8];
        assert_eq!(sscanf(0, "Ωx", "%s", &[at(&mut narrow)]), 1);
        assert_eq!(&narrow[..3], b"?x\0");
    }

    #[test]
    fn mismatches_and_early_ends() {
        let (mut a, mut b) = (0i32, 0i32);
        assert_eq!(sscanf(0, "", "%d", &[at(&mut a)]), -1);
        assert_eq!(sscanf(0, "   ", "%d", &[at(&mut a)]), -1);
        assert_eq!(sscanf(0, "abc", "%d", &[at(&mut a)]), 0);
        assert_eq!(sscanf(0, "7", "%d %d", &[at(&mut a), at(&mut b)]), 1);
        assert_eq!(sscanf(0, "7,8", "%d;%d", &[at(&mut a), at(&mut b)]), 1);
        assert_eq!(sscanf(0, "7,8", "%d,%d", &[at(&mut a), at(&mut b)]), 2);
        assert_eq!((a, b), (7, 8));
        assert_eq!(sscanf(0, "", "x", &[]), -1);
    }

    #[test]
    fn secure_forms_take_sizes() {
        let _ = super::super::runtime::_set_thread_local_invalid_parameter_handler(Some(ignore));
        let (mut a, mut word, mut chars) = (0i32, [b'#'; 4], [b'#'; 2]);
        let args = [
            at(&mut a),
            at(&mut word),
            Arg::U(4),
            at(&mut chars),
            Arg::U(2),
        ];
        assert_eq!(sscanf(SECURECRT, "5 abc de", "%d %s %2c", &args), 3);
        assert_eq!((a, &word, &chars), (5, b"abc\0", b"de"));
        let mut small = [b'#'; 4];
        let args = [at(&mut a), at(&mut small), Arg::U(3)];
        assert_eq!(sscanf(SECURECRT, "6 abc", "%d %s", &args), 1);
        assert_eq!(small, *b"\0###");
        assert_eq!(sscanf(0, "1", "%y", &[]), -1);
        assert_eq!(sscanf(0, "1", "%[a", &[at(&mut small)]), -1);
        let _ = super::super::runtime::_set_thread_local_invalid_parameter_handler(None);
    }

    #[test]
    fn wide_formats_store_wide_strings() {
        let units = wide("zz 42");
        let mut source = UnitSource {
            units: &units,
            at: 0,
        };
        let (mut word, mut n) = ([0u16; 4], 0i32);
        let slots = slots(&[at(&mut word), at(&mut n)]);
        let mut list = VaList::new(slots.as_ptr());
        let format = wide("%s %d");
        assert_eq!(scan(0, &Spec::Wide(&format), &mut list, &mut source), 2);
        assert_eq!((&word[..3], n), (&wide("zz\0")[..], 42));
    }
}
