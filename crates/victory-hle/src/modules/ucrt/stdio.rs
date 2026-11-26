//! `FILE` streams and the `printf`/`scanf` entry points
//! (`api-ms-win-crt-stdio`): buffered streams over `lowio.rs`'s
//! descriptors, and the `__stdio_common_*` functions the headers' inline
//! `printf` and `scanf` families call, over `format.rs` and `scan.rs`.
//!
//! A `FILE*` is a pointer to one of our [`Stream`]s, which are leaked
//! boxes kept in a list for the process's life (a closed one is marked and
//! used again by a later `fopen`), so a pointer stays valid while the list
//! grows and a second `fclose` of it is `EBADF`, not a double free.
//!
//! The descriptor translates text (`lowio.rs`); a stream buffers what the
//! descriptor gives. A wide function on an ANSI stream writes each
//! character as one byte (Latin-1, `?` above 255) and reads each byte as
//! one character; on a Unicode stream (`ccs=`) characters are two bytes in
//! the buffer, which the descriptor converts.
//!
//! Limits: `_IOLBF` is full buffering (as in the UCRT); `setvbuf` is
//! refused (`EINVAL`) once the stream has been read, written or moved (the
//! UCRT allows it later); `ftell` in text mode counts each `\n` the buffer
//! holds as the `\r\n` it was or will be (a lone `\n` in the file reads as
//! two bytes); a printf to a stream formats whole before it writes; the
//! scanf of a stream can give back one character per read only through
//! the stream's own push-back.
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// The UCRT's signatures, as they are.
#![allow(clippy::too_many_arguments)]

use std::cell::UnsafeCell;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, Once, OnceLock};

use super::super::win32::files::{GetFileAttributesW, GetTempFileNameW, GetTempPathW};
use super::format::{
    self, COUNT_OUTPUT, LEGACY_VSPRINTF_NULL_TERMINATION, POSITIONAL, STANDARD_SNPRINTF_BEHAVIOR,
    Sink, Spec, VaList, VecSink,
};
use super::heap::malloc;
use super::lowio::{
    self, O_APPEND, O_BINARY, O_CREAT, O_EXCL, O_NOINHERIT, O_RDONLY, O_RDWR, O_TEMPORARY, O_TEXT,
    O_TRUNC, O_U8TEXT, O_U16TEXT, O_WRONLY, O_WTEXT, S_IREAD, S_IWRITE, SH_DENYNO, TextMode,
};
use super::runtime::{
    __p__commode, _crt_atexit, _getpid, _invalid_parameter_noinfo, EBADF, EINVAL, ERANGE,
    environment, errno, lock, set_errno,
};
use super::scan::{self, Source, UnitSource};
use super::string::{narrow_bytes, wide_units};
use crate::{HleExport, hle_export};

/// What `fgetc` and its family return at the end or on failure.
const EOF: i32 = -1;
/// The wide `EOF`.
const WEOF: u16 = 0xFFFF;
/// The size of a stream's own buffer (the UCRT's `_INTERNAL_BUFSIZ`).
const BUFFER_SIZE: usize = 4096;
/// `BUFSIZ`, the size `setbuf` gives a title's buffer.
const BUFSIZ: usize = 512;
/// `setvbuf`'s modes.
const IOFBF: i32 = 0x0000;
const IOLBF: i32 = 0x0040;
const IONBF: i32 = 0x0004;
/// What `feof` and `ferror` answer (the UCRT's `_IOEOF`, `_IOERROR`).
const PUBLIC_EOF: i32 = 0x0010;
const PUBLIC_ERROR: i32 = 0x0020;
/// `_commode`'s commit bit.
const COMMIT_MODE: i32 = 0x4000;
/// `_TRUNCATE`.
const TRUNCATE: usize = usize::MAX;
/// `L_tmpnam`.
const TMPNAM_SIZE: usize = 260;

// A stream's flags.
const READ: u32 = 0x1;
const WRITE: u32 = 0x2;
const AT_EOF: u32 = 0x8;
const ERROR: u32 = 0x10;
const COMMIT: u32 = 0x20;
/// Made by `tmpfile` (closed by `_rmtmp`).
const TEMPORARY: u32 = 0x40;
/// Read, written or moved: `setvbuf` comes too late.
const STARTED: u32 = 0x200;

/// How a stream buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BufferMode {
    Full,
    /// As `Full` (the UCRT's `_IOLBF`).
    Line,
    None,
}

/// A stream's buffer: not made yet, ours, or the title's (`setvbuf`).
enum Buffer {
    Unmade,
    Own(Vec<u8>),
    Title(*mut u8, usize),
}

/// What a stream holds, behind its mutex.
struct State {
    open: bool,
    fd: i32,
    flags: u32,
    mode: BufferMode,
    buffer: Buffer,
    /// The buffer's size once made.
    size: usize,
    /// Read ahead: `buffer[pos..valid]` is not read yet.
    pos: usize,
    valid: usize,
    /// Written: `buffer[..pending]` is not on the descriptor yet.
    pending: usize,
    /// Pushed back (`ungetc`), the next byte last.
    unget: Vec<u8>,
}

// SAFETY: a title's buffer is the title's to share; everything else is
// owned data, used under the stream's mutex.
unsafe impl Send for State {}

impl State {
    fn closed() -> State {
        State {
            open: false,
            fd: -1,
            flags: 0,
            mode: BufferMode::Full,
            buffer: Buffer::Unmade,
            size: BUFFER_SIZE,
            pos: 0,
            valid: 0,
            pending: 0,
            unget: Vec::new(),
        }
    }

    fn opened(fd: i32, flags: u32, mode: BufferMode) -> State {
        State {
            open: true,
            fd,
            flags,
            mode,
            ..State::closed()
        }
    }

    /// Whether the descriptor is in a Unicode text mode.
    fn unicode(&self) -> bool {
        matches!(
            lowio::text_mode(self.fd),
            Some(TextMode::Utf8 | TextMode::Utf16)
        )
    }

    /// The buffer, made (of `size` bytes, 2 unbuffered) on first use.
    fn buffer(&mut self) -> &mut [u8] {
        if matches!(self.buffer, Buffer::Unmade) {
            let size = if self.mode == BufferMode::None {
                2
            } else {
                self.size
            };
            self.buffer = Buffer::Own(vec![0; size]);
        }
        match &mut self.buffer {
            Buffer::Own(bytes) => bytes,
            // SAFETY: the title's buffer of this size, given to `setvbuf`.
            Buffer::Title(p, n) => unsafe { std::slice::from_raw_parts_mut(*p, *n) },
            Buffer::Unmade => &mut [],
        }
    }

    /// What the buffer holds, without making it.
    fn data(&self) -> &[u8] {
        match &self.buffer {
            Buffer::Own(bytes) => bytes,
            // SAFETY: as in `buffer`.
            Buffer::Title(p, n) => unsafe { std::slice::from_raw_parts(*p, *n) },
            Buffer::Unmade => &[],
        }
    }

    /// The most a write buffers (0 unbuffered).
    fn write_capacity(&self) -> usize {
        if self.mode == BufferMode::None {
            0
        } else {
            self.size
        }
    }

    /// The most a read takes ahead: the buffer, or unbuffered one
    /// character.
    fn read_capacity(&self) -> usize {
        match self.mode {
            BufferMode::None if self.unicode() => 2,
            BufferMode::None => 1,
            _ => self.size,
        }
    }

    /// Writes what the buffer holds to the descriptor.
    fn flush(&mut self) -> bool {
        if self.pending == 0 {
            return true;
        }
        let n = self.pending;
        self.pending = 0;
        let fd = self.fd;
        let data = self.buffer().as_ptr();
        if write_all(fd, data, n) != n {
            self.flags |= ERROR;
            return false;
        }
        true
    }

    /// [`flush`](Self::flush), and to disk for a committing stream.
    fn flush_commit(&mut self) -> bool {
        let ok = self.flush();
        if ok && self.flags & COMMIT != 0 {
            return lowio::_commit(self.fd) == 0;
        }
        ok
    }

    /// Bytes read ahead (pushed back first, in the order they come), as the
    /// descriptor had them.
    fn unread_raw(&self) -> i64 {
        let mut bytes: Vec<u8> = self.unget.iter().rev().copied().collect();
        bytes.extend_from_slice(&self.data()[self.pos..self.valid]);
        raw_length(lowio::text_mode(self.fd), &bytes)
    }

    /// Stops reading ahead: the descriptor moves back over what was not
    /// read.
    fn end_reading(&mut self) {
        if self.valid > self.pos || !self.unget.is_empty() {
            let back = self.unread_raw();
            let _ = lowio::seek(self.fd, -back, 1);
        }
        self.pos = 0;
        self.valid = 0;
        self.unget.clear();
    }

    /// Writes `data` through the buffer; the bytes taken.
    fn write(&mut self, data: &[u8]) -> usize {
        if self.flags & WRITE == 0 {
            self.flags |= ERROR;
            set_errno(EBADF);
            return 0;
        }
        self.end_reading();
        self.flags |= STARTED;
        let capacity = self.write_capacity();
        if capacity == 0 || data.len() >= capacity {
            if !self.flush() {
                return 0;
            }
            let done = write_all(self.fd, data.as_ptr(), data.len());
            if done != data.len() {
                self.flags |= ERROR;
            }
            return done;
        }
        if self.pending + data.len() > capacity && !self.flush() {
            return 0;
        }
        let at = self.pending;
        self.buffer()[at..at + data.len()].copy_from_slice(data);
        self.pending += data.len();
        data.len()
    }

    /// Ready to read: a readable stream, its writes flushed.
    fn begin_read(&mut self) -> bool {
        if self.flags & READ == 0 {
            self.flags |= ERROR;
            set_errno(EBADF);
            return false;
        }
        if self.pending > 0 && !self.flush() {
            return false;
        }
        self.flags |= STARTED;
        true
    }

    /// Fills the buffer from the descriptor; false at the end or on
    /// failure (the flags say which).
    fn fill(&mut self) -> bool {
        let capacity = self.read_capacity();
        let fd = self.fd;
        let target = self.buffer().as_mut_ptr();
        let n = lowio::_read(fd, target.cast(), capacity as u32);
        match n {
            0 => {
                self.flags |= AT_EOF;
                false
            }
            n if n < 0 => {
                self.flags |= ERROR;
                false
            }
            n => {
                self.pos = 0;
                self.valid = n as usize;
                true
            }
        }
    }

    fn read_byte(&mut self) -> Option<u8> {
        if !self.begin_read() {
            return None;
        }
        if let Some(b) = self.unget.pop() {
            return Some(b);
        }
        if self.pos >= self.valid && !self.fill() {
            return None;
        }
        let b = self.data()[self.pos];
        self.pos += 1;
        Some(b)
    }

    /// Reads into `out`; the bytes read. A request at least the buffer's
    /// size goes to the descriptor directly, as in the UCRT.
    fn read_into(&mut self, out: &mut [u8]) -> usize {
        if !self.begin_read() {
            return 0;
        }
        let mut done = 0;
        while done < out.len() {
            if let Some(b) = self.unget.pop() {
                out[done] = b;
                done += 1;
                continue;
            }
            if self.pos < self.valid {
                let n = (self.valid - self.pos).min(out.len() - done);
                out[done..done + n].copy_from_slice(&self.data()[self.pos..self.pos + n]);
                self.pos += n;
                done += n;
                continue;
            }
            let mut rest = (out.len() - done).min(1 << 30);
            if self.unicode() {
                rest &= !1;
            }
            let capacity = self.read_capacity();
            if rest >= capacity.max(2) {
                let n = lowio::_read(self.fd, out[done..].as_mut_ptr().cast(), rest as u32);
                match n {
                    0 => {
                        self.flags |= AT_EOF;
                        break;
                    }
                    n if n < 0 => {
                        self.flags |= ERROR;
                        break;
                    }
                    n => done += n as usize,
                }
            } else if !self.fill() {
                break;
            }
        }
        done
    }

    /// The next character: two bytes on a Unicode stream, one otherwise.
    fn get_unit(&mut self, unicode: bool) -> Option<u16> {
        let low = self.read_byte()?;
        if !unicode {
            return Some(u16::from(low));
        }
        let high = self.read_byte()?;
        Some(u16::from_le_bytes([low, high]))
    }

    /// Writes one character (see [`get_unit`](Self::get_unit)).
    fn put_unit(&mut self, unit: u16, unicode: bool) -> bool {
        let bytes = encode(&[unit], unicode);
        self.write(&bytes) == bytes.len()
    }

    /// Pushes a character back (see [`get_unit`](Self::get_unit)).
    fn push_back(&mut self, unit: u16, unicode: bool) {
        if unicode {
            let [low, high] = unit.to_le_bytes();
            self.unget.push(high);
            self.unget.push(low);
        } else {
            self.unget.push(u8::try_from(unit).unwrap_or(b'?'));
        }
    }

    /// The stream's position: the descriptor's, less what was read ahead,
    /// plus what is buffered to write.
    fn tell(&mut self) -> Result<i64, i32> {
        let base = lowio::seek(self.fd, 0, 1)?;
        if self.pending > 0 {
            let pending = &self.data()[..self.pending];
            return Ok(base + raw_length(lowio::text_mode(self.fd), pending));
        }
        Ok(base - self.unread_raw())
    }

    /// Moves the stream: writes flushed, what was read ahead dropped, the
    /// end cleared.
    fn seek(&mut self, offset: i64, origin: i32) -> Result<i32, Fail> {
        if !(0..=2).contains(&origin) {
            return Err(Fail::Invalid(EINVAL));
        }
        let (offset, origin) = if origin == 1 {
            (self.tell().map_err(Fail::Errno)? + offset, 0)
        } else {
            (offset, origin)
        };
        self.flags |= STARTED;
        if !self.flush() {
            return Ok(-1);
        }
        self.pos = 0;
        self.valid = 0;
        self.unget.clear();
        self.flags &= !AT_EOF;
        Ok(match lowio::seek(self.fd, offset, origin) {
            Ok(_) => 0,
            Err(_) => -1,
        })
    }

    /// Flushes and closes: 0 or `EOF`.
    fn close(&mut self) -> i32 {
        let mut result = 0;
        if !self.flush() {
            result = EOF;
        }
        if lowio::_close(self.fd) != 0 {
            result = EOF;
        }
        *self = State::closed();
        result
    }
}

/// The bytes `bytes` were (or will be) on a descriptor of `mode`: each
/// `\n` of a text one counts as `\r\n`; a UTF-8 one holds UTF-16.
fn raw_length(mode: Option<TextMode>, bytes: &[u8]) -> i64 {
    let units = || -> Vec<u16> {
        bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect()
    };
    let n = match mode {
        None => bytes.len(),
        Some(TextMode::Ansi) => bytes.len() + bytes.iter().filter(|&&b| b == b'\n').count(),
        Some(TextMode::Utf16) => {
            bytes.len() + 2 * units().iter().filter(|&&u| u == u16::from(b'\n')).count()
        }
        Some(TextMode::Utf8) => {
            let units = units();
            let newlines = units.iter().filter(|&&u| u == u16::from(b'\n')).count();
            String::from_utf16_lossy(&units).len() + newlines
        }
    };
    n as i64
}

/// Writes `n` bytes at `data` to a descriptor; the bytes written.
fn write_all(fd: i32, data: *const u8, n: usize) -> usize {
    let mut done = 0;
    while done < n {
        let chunk = (n - done).min(1 << 30);
        // SAFETY: `data` holds `n` bytes.
        let wrote = lowio::_write(fd, unsafe { data.add(done) }.cast(), chunk as u32);
        if wrote <= 0 {
            break;
        }
        done += wrote as usize;
    }
    done
}

/// Characters as a stream's bytes: two each on a Unicode stream, else one
/// (Latin-1, `?` above 255).
fn encode(units: &[u16], unicode: bool) -> Vec<u8> {
    if unicode {
        units.iter().flat_map(|u| u.to_le_bytes()).collect()
    } else {
        units
            .iter()
            .map(|&u| u8::try_from(u).unwrap_or(b'?'))
            .collect()
    }
}

/// `_lock_file`'s recursive lock: the owning thread and its depth.
struct FileLock {
    owner: Mutex<(u64, u32)>,
    free: Condvar,
}

/// A number for the calling thread, from 1.
fn thread_number() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    thread_local! {
        static NUMBER: u64 = NEXT.fetch_add(1, Ordering::Relaxed);
    }
    NUMBER.try_with(|n| *n).unwrap_or(0)
}

impl FileLock {
    fn new() -> Self {
        FileLock {
            owner: Mutex::new((0, 0)),
            free: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let me = thread_number();
        let mut owner = lock(&self.owner);
        loop {
            if owner.1 == 0 {
                *owner = (me, 1);
                return;
            }
            if owner.0 == me {
                owner.1 += 1;
                return;
            }
            owner = self
                .free
                .wait(owner)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn release(&self) {
        let me = thread_number();
        let mut owner = lock(&self.owner);
        if owner.1 > 0 && owner.0 == me {
            owner.1 -= 1;
            if owner.1 == 0 {
                self.free.notify_one();
            }
        }
    }
}

/// What a `FILE*` points at: a placeholder (the UCRT's `struct _iobuf`),
/// then ours.
#[repr(C)]
pub struct Stream {
    _placeholder: *mut c_void,
    lock: FileLock,
    state: Mutex<State>,
}

// SAFETY: the placeholder is never read; the rest is behind locks.
unsafe impl Sync for Stream {}
// SAFETY: as above.
unsafe impl Send for Stream {}

/// The title's `FILE`.
pub type FILE = Stream;

/// Every stream made, open or closed.
static STREAMS: Mutex<Vec<&'static Stream>> = Mutex::new(Vec::new());

fn new_stream(state: State) -> &'static Stream {
    Box::leak(Box::new(Stream {
        _placeholder: std::ptr::null_mut(),
        lock: FileLock::new(),
        state: Mutex::new(state),
    }))
}

/// The standard streams over descriptors 0, 1 and 2, made once: stdout and
/// stderr unbuffered on a device, stdin buffered.
fn standard() -> &'static [&'static Stream; 3] {
    static STANDARD: OnceLock<[&'static Stream; 3]> = OnceLock::new();
    STANDARD.get_or_init(|| {
        let made = [0, 1, 2].map(|fd| {
            let flags = if fd == 0 { READ } else { WRITE };
            let mode = if fd != 0 && lowio::is_device(fd) {
                BufferMode::None
            } else {
                BufferMode::Full
            };
            new_stream(State::opened(fd, flags, mode))
        });
        lock(&STREAMS).extend(made);
        made
    })
}

fn pointer(stream: &'static Stream) -> *mut FILE {
    (stream as *const Stream).cast_mut()
}

/// Our stream at `stream`, if it is one.
fn find(stream: *mut FILE) -> Option<&'static Stream> {
    standard();
    lock(&STREAMS)
        .iter()
        .find(|s| std::ptr::eq(**s, stream))
        .copied()
}

/// Every stream, for the functions that go over them all.
fn all_streams() -> Vec<&'static Stream> {
    standard();
    lock(&STREAMS).clone()
}

/// Flushes every stream at exit, registered with the first stream opened.
extern "win64" fn flush_at_exit() {
    flush_all();
}

/// A stream for `state`: a closed one used again, or a new one.
fn take_stream(state: State) -> &'static Stream {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| {
        _crt_atexit(Some(flush_at_exit));
    });
    standard();
    let mut streams = lock(&STREAMS);
    for stream in streams.iter().skip(3) {
        let mut slot = lock(&stream.state);
        if !slot.open {
            *slot = state;
            return stream;
        }
    }
    let stream = new_stream(state);
    streams.push(stream);
    stream
}

/// Why a stream function failed.
enum Fail {
    /// `errno` to set.
    Errno(i32),
    /// `errno` to set, then an invalid parameter.
    Invalid(i32),
}

fn report(fail: Fail) {
    match fail {
        Fail::Errno(code) => set_errno(code),
        Fail::Invalid(code) => {
            set_errno(code);
            _invalid_parameter_noinfo();
        }
    }
}

/// Runs `body` on an open stream's state, the stream's lock held (unless
/// `nolock`): its result, or `failure` with the failure reported after the
/// locks are let go. A null or unknown stream is an invalid parameter, a
/// closed one `EBADF`.
fn with_stream<R>(
    stream: *mut FILE,
    nolock: bool,
    failure: R,
    body: impl FnOnce(&mut State) -> Result<R, Fail>,
) -> R {
    let Some(s) = find(stream) else {
        report(Fail::Invalid(EINVAL));
        return failure;
    };
    if !nolock {
        s.lock.acquire();
    }
    let result = {
        let mut state = lock(&s.state);
        if state.open {
            body(&mut state)
        } else {
            Err(Fail::Errno(EBADF))
        }
    };
    if !nolock {
        s.lock.release();
    }
    result.unwrap_or_else(|fail| {
        report(fail);
        failure
    })
}

/// Narrow functions refuse a Unicode stream (the UCRT asserts).
fn narrow_only(state: &State) -> Result<(), Fail> {
    if state.unicode() {
        return Err(Fail::Invalid(EINVAL));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Opening and closing.

/// A parsed `fopen` mode.
struct Mode {
    oflag: i32,
    flags: u32,
}

/// Parses an `fopen` mode as the UCRT does; `None` for a bad one.
fn parse_mode(mode: &[u16]) -> Option<Mode> {
    let chars: Vec<char> = mode
        .iter()
        .map(|&u| char::from_u32(u32::from(u)).unwrap_or('\u{FFFD}'))
        .collect();
    let mut i = 0;
    while chars.get(i) == Some(&' ') {
        i += 1;
    }
    let first = *chars.get(i)?;
    let (mut oflag, mut flags) = match first {
        'r' => (O_RDONLY, READ),
        'w' => (O_WRONLY | O_CREAT | O_TRUNC, WRITE),
        'a' => (O_WRONLY | O_CREAT | O_APPEND, WRITE),
        _ => return None,
    };
    // SAFETY: the runtime's own `_commode`.
    if unsafe { *__p__commode() } & COMMIT_MODE != 0 {
        flags |= COMMIT;
    }
    i += 1;
    let (mut plus, mut translation, mut commit, mut access, mut temporary, mut life) =
        (false, false, false, false, false, false);
    while i < chars.len() {
        match chars[i] {
            '+' => {
                seen(&mut plus)?;
                oflag = (oflag & !(O_RDONLY | O_WRONLY | O_RDWR)) | O_RDWR;
                flags |= READ | WRITE;
            }
            'b' => {
                seen(&mut translation)?;
                oflag |= O_BINARY;
            }
            't' => {
                seen(&mut translation)?;
                oflag |= O_TEXT;
            }
            'c' => {
                seen(&mut commit)?;
                flags |= COMMIT;
            }
            'n' => {
                seen(&mut commit)?;
                flags &= !COMMIT;
            }
            'S' | 'R' => seen(&mut access)?,
            'T' => seen(&mut life)?,
            'D' => {
                seen(&mut temporary)?;
                oflag |= O_TEMPORARY;
            }
            'N' => oflag |= O_NOINHERIT,
            'x' if first == 'w' => oflag |= O_EXCL,
            ' ' => {}
            ',' => {
                oflag |= encoding(&chars[i + 1..])?;
                break;
            }
            _ => return None,
        }
        i += 1;
    }
    Some(Mode { oflag, flags })
}

/// Marks a mode character seen; `None` the second time.
fn seen(flag: &mut bool) -> Option<()> {
    if *flag {
        return None;
    }
    *flag = true;
    Some(())
}

/// The `ccs=` part of a mode, after its `,`: `_O_U8TEXT`, `_O_U16TEXT` or
/// `_O_WTEXT`.
fn encoding(rest: &[char]) -> Option<i32> {
    let text: String = rest.iter().collect();
    let text = text.trim_start();
    let text = text.strip_prefix("ccs")?.trim_start();
    let name = text.strip_prefix('=')?.trim();
    match name.to_ascii_uppercase().as_str() {
        "UTF-8" => Some(O_U8TEXT),
        "UTF-16LE" => Some(O_U16TEXT),
        "UNICODE" => Some(O_WTEXT),
        _ => None,
    }
}

/// A title's narrow string as UTF-16 (each byte a character: a mode); or,
/// with `utf8`, read as UTF-8 (a path). NUL-terminated.
fn narrow_units(text: *const c_char, utf8: bool) -> Option<Vec<u16>> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the title's NUL-terminated string.
    let bytes = unsafe { narrow_bytes(text) };
    let units: Vec<u16> = if utf8 {
        String::from_utf8_lossy(bytes).encode_utf16().collect()
    } else {
        bytes.iter().map(|&b| u16::from(b)).collect()
    };
    Some(units.into_iter().chain([0]).collect())
}

/// A title's wide string, NUL-terminated.
fn wide_string(text: *const u16) -> Option<Vec<u16>> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the title's NUL-terminated string.
    Some(
        unsafe { wide_units(text) }
            .iter()
            .copied()
            .chain([0])
            .collect(),
    )
}

/// Opens `path` as `mode` says into `target` (or a new stream).
fn open_stream(
    path: Option<Vec<u16>>,
    mode: Option<Vec<u16>>,
    shflag: i32,
    target: Option<&'static Stream>,
) -> Result<&'static Stream, Fail> {
    let (Some(path), Some(mode)) = (path, mode) else {
        return Err(Fail::Invalid(EINVAL));
    };
    if path.len() <= 1 {
        return Err(Fail::Invalid(EINVAL));
    }
    let mode = parse_mode(&mode[..mode.len() - 1]).ok_or(Fail::Invalid(EINVAL))?;
    let fd =
        lowio::open(&path, mode.oflag, shflag, S_IREAD | S_IWRITE, false).map_err(Fail::Errno)?;
    let state = State::opened(fd, mode.flags, BufferMode::Full);
    Ok(match target {
        Some(stream) => {
            *lock(&stream.state) = state;
            stream
        }
        None => take_stream(state),
    })
}

/// `fopen`'s body: the stream, or null with the failure reported.
fn fopen_with(path: Option<Vec<u16>>, mode: Option<Vec<u16>>, shflag: i32) -> *mut FILE {
    match open_stream(path, mode, shflag, None) {
        Ok(stream) => pointer(stream),
        Err(fail) => {
            report(fail);
            std::ptr::null_mut()
        }
    }
}

/// `fopen_s`'s body: the stream to `*out`; 0 or the `errno` value.
fn fopen_s_with(out: *mut *mut FILE, path: Option<Vec<u16>>, mode: Option<Vec<u16>>) -> i32 {
    if out.is_null() {
        report(Fail::Invalid(EINVAL));
        return EINVAL;
    }
    let stream = fopen_with(path, mode, SH_DENYNO);
    // SAFETY: the title's place for the stream.
    unsafe { out.write(stream) };
    if stream.is_null() { errno() } else { 0 }
}

/// Opens a file as a stream (`r`, `w`, `a`, `+`, `b`, `t`, `c`, `n`, `S`,
/// `R`, `T`, `D`, `N`, `x`, `ccs=`).
pub extern "win64" fn fopen(path: *const c_char, mode: *const c_char) -> *mut FILE {
    fopen_with(
        narrow_units(path, true),
        narrow_units(mode, false),
        SH_DENYNO,
    )
}

/// See [`fopen`].
pub extern "win64" fn _wfopen(path: *const u16, mode: *const u16) -> *mut FILE {
    fopen_with(wide_string(path), wide_string(mode), SH_DENYNO)
}

/// See [`fopen`]: the stream to `*out`; 0 or the `errno` value.
pub extern "win64" fn fopen_s(
    out: *mut *mut FILE,
    path: *const c_char,
    mode: *const c_char,
) -> i32 {
    fopen_s_with(out, narrow_units(path, true), narrow_units(mode, false))
}

/// See [`fopen_s`].
pub extern "win64" fn _wfopen_s(out: *mut *mut FILE, path: *const u16, mode: *const u16) -> i32 {
    fopen_s_with(out, wide_string(path), wide_string(mode))
}

/// See [`fopen`], with a sharing mode.
pub extern "win64" fn _fsopen(path: *const c_char, mode: *const c_char, shflag: i32) -> *mut FILE {
    fopen_with(narrow_units(path, true), narrow_units(mode, false), shflag)
}

/// See [`_fsopen`].
pub extern "win64" fn _wfsopen(path: *const u16, mode: *const u16, shflag: i32) -> *mut FILE {
    fopen_with(wide_string(path), wide_string(mode), shflag)
}

/// `freopen`'s body: the stream closed and opened again on `path`.
fn freopen_with(path: Option<Vec<u16>>, mode: Option<Vec<u16>>, stream: *mut FILE) -> *mut FILE {
    let Some(s) = find(stream) else {
        report(Fail::Invalid(EINVAL));
        return std::ptr::null_mut();
    };
    s.lock.acquire();
    {
        let mut state = lock(&s.state);
        if state.open {
            state.close();
        }
    }
    let result = open_stream(path, mode, SH_DENYNO, Some(s));
    s.lock.release();
    match result {
        Ok(stream) => pointer(stream),
        Err(fail) => {
            report(fail);
            std::ptr::null_mut()
        }
    }
}

/// Closes a stream and opens it again on another file (the standard ones
/// too).
pub extern "win64" fn freopen(
    path: *const c_char,
    mode: *const c_char,
    stream: *mut FILE,
) -> *mut FILE {
    freopen_with(narrow_units(path, true), narrow_units(mode, false), stream)
}

/// See [`freopen`].
pub extern "win64" fn _wfreopen(
    path: *const u16,
    mode: *const u16,
    stream: *mut FILE,
) -> *mut FILE {
    freopen_with(wide_string(path), wide_string(mode), stream)
}

/// See [`freopen`]: the stream to `*out`; 0 or the `errno` value.
pub extern "win64" fn freopen_s(
    out: *mut *mut FILE,
    path: *const c_char,
    mode: *const c_char,
    stream: *mut FILE,
) -> i32 {
    if out.is_null() {
        report(Fail::Invalid(EINVAL));
        return EINVAL;
    }
    let result = freopen(path, mode, stream);
    // SAFETY: the title's place for the stream.
    unsafe { out.write(result) };
    if result.is_null() { errno() } else { 0 }
}

/// See [`freopen_s`].
pub extern "win64" fn _wfreopen_s(
    out: *mut *mut FILE,
    path: *const u16,
    mode: *const u16,
    stream: *mut FILE,
) -> i32 {
    if out.is_null() {
        report(Fail::Invalid(EINVAL));
        return EINVAL;
    }
    let result = _wfreopen(path, mode, stream);
    // SAFETY: the title's place for the stream.
    unsafe { out.write(result) };
    if result.is_null() { errno() } else { 0 }
}

/// `_fdopen`'s body.
fn fdopen_with(fd: i32, mode: Option<Vec<u16>>) -> *mut FILE {
    let Some(mode) = mode else {
        report(Fail::Invalid(EINVAL));
        return std::ptr::null_mut();
    };
    let Some(parsed) = parse_mode(&mode[..mode.len() - 1]) else {
        report(Fail::Invalid(EINVAL));
        return std::ptr::null_mut();
    };
    if lowio::_get_osfhandle(fd) == -1 {
        return std::ptr::null_mut();
    }
    pointer(take_stream(State::opened(
        fd,
        parsed.flags,
        BufferMode::Full,
    )))
}

/// A stream over an open descriptor.
pub extern "win64" fn _fdopen(fd: i32, mode: *const c_char) -> *mut FILE {
    fdopen_with(fd, narrow_units(mode, false))
}

/// See [`_fdopen`].
pub extern "win64" fn _wfdopen(fd: i32, mode: *const u16) -> *mut FILE {
    fdopen_with(fd, wide_string(mode))
}

/// Flushes and closes a stream (its descriptor too); 0 or `EOF`. A closed
/// stream is `EBADF`.
pub extern "win64" fn fclose(stream: *mut FILE) -> i32 {
    with_stream(stream, false, EOF, |state| Ok(state.close()))
}

/// See [`fclose`], without the stream's lock.
pub extern "win64" fn _fclose_nolock(stream: *mut FILE) -> i32 {
    with_stream(stream, true, EOF, |state| Ok(state.close()))
}

/// Closes every stream but the standard ones; the number closed.
pub extern "win64" fn _fcloseall() -> i32 {
    let mut closed = 0;
    for stream in all_streams().into_iter().skip(3) {
        stream.lock.acquire();
        {
            let mut state = lock(&stream.state);
            if state.open {
                state.close();
                closed += 1;
            }
        }
        stream.lock.release();
    }
    closed
}

/// Closes the streams `tmpfile` opened; the number closed.
pub extern "win64" fn _rmtmp() -> i32 {
    let mut closed = 0;
    for stream in all_streams() {
        stream.lock.acquire();
        {
            let mut state = lock(&stream.state);
            if state.open && state.flags & TEMPORARY != 0 {
                state.close();
                closed += 1;
            }
        }
        stream.lock.release();
    }
    closed
}

/// Flushes every open stream; the number open, and whether every flush
/// worked.
fn flush_all() -> (i32, bool) {
    let mut open = 0;
    let mut ok = true;
    for stream in all_streams() {
        stream.lock.acquire();
        {
            let mut state = lock(&stream.state);
            if state.open {
                open += 1;
                ok &= state.flush_commit();
            }
        }
        stream.lock.release();
    }
    (open, ok)
}

/// `fflush`'s body: a null stream flushes them all.
fn fflush_with(stream: *mut FILE, nolock: bool) -> i32 {
    if stream.is_null() {
        return if flush_all().1 { 0 } else { EOF };
    }
    with_stream(stream, nolock, EOF, |state| {
        Ok(if state.flush_commit() { 0 } else { EOF })
    })
}

/// Writes a stream's buffer (to disk too, for a committing one); 0 or
/// `EOF`.
pub extern "win64" fn fflush(stream: *mut FILE) -> i32 {
    fflush_with(stream, false)
}

/// See [`fflush`], without the stream's lock.
pub extern "win64" fn _fflush_nolock(stream: *mut FILE) -> i32 {
    fflush_with(stream, true)
}

/// Flushes every open stream; the number open.
pub extern "win64" fn _flushall() -> i32 {
    flush_all().0
}

// ---------------------------------------------------------------------------
// Reading and writing.

/// `fread`'s body.
fn fread_with(
    buffer: *mut c_void,
    size: usize,
    count: usize,
    stream: *mut FILE,
    nolock: bool,
) -> usize {
    if size == 0 || count == 0 {
        return 0;
    }
    let Some(total) = size.checked_mul(count).filter(|_| !buffer.is_null()) else {
        report(Fail::Invalid(EINVAL));
        return 0;
    };
    with_stream(stream, nolock, 0, |state| {
        // SAFETY: the title's buffer of `size * count` bytes.
        let out = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), total) };
        Ok(state.read_into(out) / size)
    })
}

/// Reads `count` items of `size` bytes; the items read whole.
pub extern "win64" fn fread(
    buffer: *mut c_void,
    size: usize,
    count: usize,
    stream: *mut FILE,
) -> usize {
    fread_with(buffer, size, count, stream, false)
}

/// See [`fread`], without the stream's lock.
pub extern "win64" fn _fread_nolock(
    buffer: *mut c_void,
    size: usize,
    count: usize,
    stream: *mut FILE,
) -> usize {
    fread_with(buffer, size, count, stream, true)
}

/// `fread_s`'s body: a buffer too small for the request is cleared,
/// `ERANGE`.
fn fread_s_with(
    buffer: *mut c_void,
    buffer_size: usize,
    size: usize,
    count: usize,
    stream: *mut FILE,
    nolock: bool,
) -> usize {
    if size == 0 || count == 0 {
        return 0;
    }
    if buffer.is_null() {
        report(Fail::Invalid(EINVAL));
        return 0;
    }
    if size
        .checked_mul(count)
        .is_none_or(|total| total > buffer_size)
    {
        // SAFETY: the title's buffer of `buffer_size` bytes.
        unsafe { std::ptr::write_bytes(buffer.cast::<u8>(), 0, buffer_size) };
        report(Fail::Invalid(ERANGE));
        return 0;
    }
    fread_with(buffer, size, count, stream, nolock)
}

/// See [`fread`], into a buffer of `buffer_size` bytes.
pub extern "win64" fn fread_s(
    buffer: *mut c_void,
    buffer_size: usize,
    size: usize,
    count: usize,
    stream: *mut FILE,
) -> usize {
    fread_s_with(buffer, buffer_size, size, count, stream, false)
}

/// See [`fread_s`], without the stream's lock.
pub extern "win64" fn _fread_nolock_s(
    buffer: *mut c_void,
    buffer_size: usize,
    size: usize,
    count: usize,
    stream: *mut FILE,
) -> usize {
    fread_s_with(buffer, buffer_size, size, count, stream, true)
}

/// `fwrite`'s body.
fn fwrite_with(
    buffer: *const c_void,
    size: usize,
    count: usize,
    stream: *mut FILE,
    nolock: bool,
) -> usize {
    if size == 0 || count == 0 {
        return 0;
    }
    let Some(total) = size.checked_mul(count).filter(|_| !buffer.is_null()) else {
        report(Fail::Invalid(EINVAL));
        return 0;
    };
    with_stream(stream, nolock, 0, |state| {
        // SAFETY: the title's buffer of `size * count` bytes.
        let data = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), total) };
        Ok(state.write(data) / size)
    })
}

/// Writes `count` items of `size` bytes; the items written whole.
pub extern "win64" fn fwrite(
    buffer: *const c_void,
    size: usize,
    count: usize,
    stream: *mut FILE,
) -> usize {
    fwrite_with(buffer, size, count, stream, false)
}

/// See [`fwrite`], without the stream's lock.
pub extern "win64" fn _fwrite_nolock(
    buffer: *const c_void,
    size: usize,
    count: usize,
    stream: *mut FILE,
) -> usize {
    fwrite_with(buffer, size, count, stream, true)
}

fn fgetc_with(stream: *mut FILE, nolock: bool) -> i32 {
    with_stream(stream, nolock, EOF, |state| {
        narrow_only(state)?;
        Ok(state.read_byte().map_or(EOF, i32::from))
    })
}

fn fgetwc_with(stream: *mut FILE, nolock: bool) -> u16 {
    with_stream(stream, nolock, WEOF, |state| {
        let unicode = state.unicode();
        Ok(state.get_unit(unicode).unwrap_or(WEOF))
    })
}

fn fputc_with(c: i32, stream: *mut FILE, nolock: bool) -> i32 {
    with_stream(stream, nolock, EOF, |state| {
        narrow_only(state)?;
        Ok(if state.write(&[c as u8]) == 1 {
            c & 0xFF
        } else {
            EOF
        })
    })
}

fn fputwc_with(c: u16, stream: *mut FILE, nolock: bool) -> u16 {
    with_stream(stream, nolock, WEOF, |state| {
        let unicode = state.unicode();
        Ok(if state.put_unit(c, unicode) { c } else { WEOF })
    })
}

fn stdin() -> *mut FILE {
    pointer(standard()[0])
}

fn stdout() -> *mut FILE {
    pointer(standard()[1])
}

/// The next byte, or `EOF`.
pub extern "win64" fn fgetc(stream: *mut FILE) -> i32 {
    fgetc_with(stream, false)
}

/// See [`fgetc`].
pub extern "win64" fn getc(stream: *mut FILE) -> i32 {
    fgetc_with(stream, false)
}

/// See [`fgetc`], without the stream's lock.
pub extern "win64" fn _fgetc_nolock(stream: *mut FILE) -> i32 {
    fgetc_with(stream, true)
}

/// See [`_fgetc_nolock`].
pub extern "win64" fn _getc_nolock(stream: *mut FILE) -> i32 {
    fgetc_with(stream, true)
}

/// Fills the buffer and returns its first byte (the old `getc` macro's
/// helper), or `EOF`.
pub extern "win64" fn _filbuf(stream: *mut FILE) -> i32 {
    fgetc_with(stream, true)
}

/// [`fgetc`] of `stdin`.
pub extern "win64" fn getchar() -> i32 {
    fgetc_with(stdin(), false)
}

/// The next character, or `WEOF`.
pub extern "win64" fn fgetwc(stream: *mut FILE) -> u16 {
    fgetwc_with(stream, false)
}

/// See [`fgetwc`].
pub extern "win64" fn getwc(stream: *mut FILE) -> u16 {
    fgetwc_with(stream, false)
}

/// See [`fgetwc`], without the stream's lock.
pub extern "win64" fn _fgetwc_nolock(stream: *mut FILE) -> u16 {
    fgetwc_with(stream, true)
}

/// See [`_fgetwc_nolock`].
pub extern "win64" fn _getwc_nolock(stream: *mut FILE) -> u16 {
    fgetwc_with(stream, true)
}

/// [`fgetwc`] of `stdin`.
pub extern "win64" fn getwchar() -> u16 {
    fgetwc_with(stdin(), false)
}

/// Writes a byte; it, or `EOF`.
pub extern "win64" fn fputc(c: i32, stream: *mut FILE) -> i32 {
    fputc_with(c, stream, false)
}

/// See [`fputc`].
pub extern "win64" fn putc(c: i32, stream: *mut FILE) -> i32 {
    fputc_with(c, stream, false)
}

/// See [`fputc`], without the stream's lock.
pub extern "win64" fn _fputc_nolock(c: i32, stream: *mut FILE) -> i32 {
    fputc_with(c, stream, true)
}

/// See [`_fputc_nolock`].
pub extern "win64" fn _putc_nolock(c: i32, stream: *mut FILE) -> i32 {
    fputc_with(c, stream, true)
}

/// Writes `c` through the buffer (the old `putc` macro's helper).
pub extern "win64" fn _flsbuf(c: i32, stream: *mut FILE) -> i32 {
    fputc_with(c, stream, true)
}

/// [`fputc`] to `stdout`.
pub extern "win64" fn putchar(c: i32) -> i32 {
    fputc_with(c, stdout(), false)
}

/// Writes a character; it, or `WEOF`.
pub extern "win64" fn fputwc(c: u16, stream: *mut FILE) -> u16 {
    fputwc_with(c, stream, false)
}

/// See [`fputwc`].
pub extern "win64" fn putwc(c: u16, stream: *mut FILE) -> u16 {
    fputwc_with(c, stream, false)
}

/// See [`fputwc`], without the stream's lock.
pub extern "win64" fn _fputwc_nolock(c: u16, stream: *mut FILE) -> u16 {
    fputwc_with(c, stream, true)
}

/// See [`_fputwc_nolock`].
pub extern "win64" fn _putwc_nolock(c: u16, stream: *mut FILE) -> u16 {
    fputwc_with(c, stream, true)
}

/// [`fputwc`] to `stdout`.
pub extern "win64" fn putwchar(c: u16) -> u16 {
    fputwc_with(c, stdout(), false)
}

/// A line of at most `n - 1` units (its `\n` kept) into `buffer` and a
/// NUL; null at the end with nothing read, or on a failure.
fn read_line<T: Copy + Default>(
    stream: *mut FILE,
    buffer: *mut T,
    n: i32,
    wide: bool,
    convert: impl Fn(u16) -> T,
) -> *mut T {
    if buffer.is_null() || n <= 0 {
        report(Fail::Invalid(EINVAL));
        return std::ptr::null_mut();
    }
    let limit = n as usize - 1;
    with_stream(stream, false, std::ptr::null_mut(), |state| {
        let unicode = state.unicode();
        if !wide {
            narrow_only(state)?;
        }
        let mut count = 0;
        while count < limit {
            let Some(unit) = state.get_unit(unicode) else {
                break;
            };
            // SAFETY: the title's buffer of `n` units.
            unsafe { buffer.add(count).write(convert(unit)) };
            count += 1;
            if unit == u16::from(b'\n') {
                break;
            }
        }
        if (count == 0 && limit > 0) || state.flags & ERROR != 0 {
            return Ok(std::ptr::null_mut());
        }
        // SAFETY: as above; `count < n`.
        unsafe { buffer.add(count).write(T::default()) };
        Ok(buffer)
    })
}

/// A line into `buffer` (see [`read_line`]).
pub extern "win64" fn fgets(buffer: *mut c_char, n: i32, stream: *mut FILE) -> *mut c_char {
    read_line(stream, buffer.cast::<u8>(), n, false, |u| u as u8).cast()
}

/// See [`fgets`].
pub extern "win64" fn fgetws(buffer: *mut u16, n: i32, stream: *mut FILE) -> *mut u16 {
    read_line(stream, buffer, n, true, |u| u)
}

/// A line of `stdin` without its `\n`; one that does not fit empties the
/// buffer and is an invalid parameter (`ERANGE`), the rest of it read.
fn get_line<T: Copy + Default>(
    buffer: *mut T,
    size: usize,
    wide: bool,
    convert: impl Fn(u16) -> T,
) -> *mut T {
    if buffer.is_null() || size == 0 {
        report(Fail::Invalid(EINVAL));
        return std::ptr::null_mut();
    }
    with_stream(stdin(), false, std::ptr::null_mut(), |state| {
        let unicode = state.unicode();
        if !wide {
            narrow_only(state)?;
        }
        let mut line = Vec::new();
        let mut ended = true;
        while let Some(unit) = state.get_unit(unicode) {
            ended = false;
            if unit == u16::from(b'\n') {
                break;
            }
            line.push(unit);
        }
        if ended && line.is_empty() {
            return Ok(std::ptr::null_mut());
        }
        if line.len() + 1 > size {
            // SAFETY: the title's buffer of `size` units.
            unsafe { buffer.write(T::default()) };
            return Err(Fail::Invalid(ERANGE));
        }
        for (k, &unit) in line.iter().enumerate() {
            // SAFETY: as above; the line and its NUL fit.
            unsafe { buffer.add(k).write(convert(unit)) };
        }
        // SAFETY: as above.
        unsafe { buffer.add(line.len()).write(T::default()) };
        Ok(buffer)
    })
}

/// A line of `stdin` into `buffer` of `size` bytes.
pub extern "win64" fn gets_s(buffer: *mut c_char, size: usize) -> *mut c_char {
    get_line(buffer.cast::<u8>(), size, false, |u| u as u8).cast()
}

/// See [`gets_s`].
pub extern "win64" fn _getws_s(buffer: *mut u16, size: usize) -> *mut u16 {
    get_line(buffer, size, true, |u| u)
}

/// Writes units (and a `\n` if asked) to a stream: `ok` or `failure`.
fn put_text(
    stream: *mut FILE,
    units: Option<Vec<u16>>,
    wide: bool,
    newline: bool,
    ok: i32,
    failure: i32,
) -> i32 {
    let Some(mut units) = units else {
        report(Fail::Invalid(EINVAL));
        return failure;
    };
    units.pop();
    if newline {
        units.push(u16::from(b'\n'));
    }
    with_stream(stream, false, failure, |state| {
        let unicode = state.unicode();
        if !wide {
            narrow_only(state)?;
        }
        let bytes = encode(&units, unicode);
        Ok(if state.write(&bytes) == bytes.len() {
            ok
        } else {
            failure
        })
    })
}

/// Writes a string; 0 or `EOF`.
pub extern "win64" fn fputs(text: *const c_char, stream: *mut FILE) -> i32 {
    put_text(stream, narrow_units(text, false), false, false, 0, EOF)
}

/// See [`fputs`]; 0 or `WEOF`.
pub extern "win64" fn fputws(text: *const u16, stream: *mut FILE) -> i32 {
    put_text(stream, wide_string(text), true, false, 0, i32::from(WEOF))
}

/// Writes a string and a `\n` to `stdout`; 0 or `EOF`.
pub extern "win64" fn puts(text: *const c_char) -> i32 {
    put_text(stdout(), narrow_units(text, false), false, true, 0, EOF)
}

/// See [`puts`]; 0 or `WEOF`.
pub extern "win64" fn _putws(text: *const u16) -> i32 {
    put_text(stdout(), wide_string(text), true, true, 0, i32::from(WEOF))
}

fn ungetc_with(c: i32, stream: *mut FILE, nolock: bool) -> i32 {
    if c == EOF {
        return EOF;
    }
    with_stream(stream, nolock, EOF, |state| {
        narrow_only(state)?;
        if state.flags & READ == 0 || (state.pending > 0 && !state.flush()) {
            return Ok(EOF);
        }
        state.unget.push(c as u8);
        state.flags &= !AT_EOF;
        Ok(c & 0xFF)
    })
}

fn ungetwc_with(c: u16, stream: *mut FILE, nolock: bool) -> u16 {
    if c == WEOF {
        return WEOF;
    }
    with_stream(stream, nolock, WEOF, |state| {
        if state.flags & READ == 0 || (state.pending > 0 && !state.flush()) {
            return Ok(WEOF);
        }
        let unicode = state.unicode();
        state.push_back(c, unicode);
        state.flags &= !AT_EOF;
        Ok(c)
    })
}

/// Pushes a byte back to be read next; it, or `EOF`.
pub extern "win64" fn ungetc(c: i32, stream: *mut FILE) -> i32 {
    ungetc_with(c, stream, false)
}

/// See [`ungetc`], without the stream's lock.
pub extern "win64" fn _ungetc_nolock(c: i32, stream: *mut FILE) -> i32 {
    ungetc_with(c, stream, true)
}

/// Pushes a character back; it, or `WEOF`.
pub extern "win64" fn ungetwc(c: u16, stream: *mut FILE) -> u16 {
    ungetwc_with(c, stream, false)
}

/// See [`ungetwc`], without the stream's lock.
pub extern "win64" fn _ungetwc_nolock(c: u16, stream: *mut FILE) -> u16 {
    ungetwc_with(c, stream, true)
}

// ---------------------------------------------------------------------------
// Positions and state.

fn fseek_with(stream: *mut FILE, offset: i64, origin: i32, nolock: bool) -> i32 {
    with_stream(stream, nolock, -1, |state| state.seek(offset, origin))
}

fn ftell_with(stream: *mut FILE, nolock: bool) -> i64 {
    with_stream(stream, nolock, -1, |state| {
        state.tell().map_err(Fail::Errno)
    })
}

/// Moves the stream (`SEEK_SET` 0, `SEEK_CUR` 1, `SEEK_END` 2); 0 or -1.
pub extern "win64" fn fseek(stream: *mut FILE, offset: i32, origin: i32) -> i32 {
    fseek_with(stream, i64::from(offset), origin, false)
}

/// See [`fseek`].
pub extern "win64" fn _fseeki64(stream: *mut FILE, offset: i64, origin: i32) -> i32 {
    fseek_with(stream, offset, origin, false)
}

/// See [`fseek`], without the stream's lock.
pub extern "win64" fn _fseek_nolock(stream: *mut FILE, offset: i32, origin: i32) -> i32 {
    fseek_with(stream, i64::from(offset), origin, true)
}

/// See [`_fseeki64`], without the stream's lock.
pub extern "win64" fn _fseeki64_nolock(stream: *mut FILE, offset: i64, origin: i32) -> i32 {
    fseek_with(stream, offset, origin, true)
}

/// A position as `ftell` gives it: past 2^31 - 1 is `EINVAL`.
fn narrow_position(position: i64) -> i32 {
    if position < 0 {
        return -1;
    }
    i32::try_from(position).unwrap_or_else(|_| {
        set_errno(EINVAL);
        -1
    })
}

/// The stream's position; -1 on failure.
pub extern "win64" fn ftell(stream: *mut FILE) -> i32 {
    narrow_position(ftell_with(stream, false))
}

/// See [`ftell`].
pub extern "win64" fn _ftelli64(stream: *mut FILE) -> i64 {
    ftell_with(stream, false)
}

/// See [`ftell`], without the stream's lock.
pub extern "win64" fn _ftell_nolock(stream: *mut FILE) -> i32 {
    narrow_position(ftell_with(stream, true))
}

/// See [`_ftelli64`], without the stream's lock.
pub extern "win64" fn _ftelli64_nolock(stream: *mut FILE) -> i64 {
    ftell_with(stream, true)
}

/// The position to `*position` (an `fpos_t`, 64 bits); 0 or -1.
pub extern "win64" fn fgetpos(stream: *mut FILE, position: *mut i64) -> i32 {
    if position.is_null() {
        report(Fail::Invalid(EINVAL));
        return -1;
    }
    let at = ftell_with(stream, false);
    if at < 0 {
        return -1;
    }
    // SAFETY: the title's `fpos_t`.
    unsafe { position.write_unaligned(at) };
    0
}

/// Moves to a position `fgetpos` gave; 0 or -1.
pub extern "win64" fn fsetpos(stream: *mut FILE, position: *const i64) -> i32 {
    if position.is_null() {
        report(Fail::Invalid(EINVAL));
        return -1;
    }
    // SAFETY: the title's `fpos_t`.
    let at = unsafe { position.read_unaligned() };
    fseek_with(stream, at, 0, false)
}

/// Moves to the start and clears the error.
pub extern "win64" fn rewind(stream: *mut FILE) {
    with_stream(stream, false, (), |state| {
        let _ = state.seek(0, 0);
        state.flags &= !ERROR;
        Ok(())
    });
}

/// Non-zero at the end of the stream.
pub extern "win64" fn feof(stream: *mut FILE) -> i32 {
    with_stream(stream, true, 0, |state| {
        Ok(if state.flags & AT_EOF != 0 {
            PUBLIC_EOF
        } else {
            0
        })
    })
}

/// Non-zero after a failure.
pub extern "win64" fn ferror(stream: *mut FILE) -> i32 {
    with_stream(stream, true, 0, |state| {
        Ok(if state.flags & ERROR != 0 {
            PUBLIC_ERROR
        } else {
            0
        })
    })
}

/// Clears the end and the error (the descriptor's `^Z` end too).
pub extern "win64" fn clearerr(stream: *mut FILE) {
    with_stream(stream, false, (), |state| {
        state.flags &= !(AT_EOF | ERROR);
        lowio::clear_eof(state.fd);
        Ok(())
    });
}

/// See [`clearerr`]; 0 or `EINVAL`.
pub extern "win64" fn clearerr_s(stream: *mut FILE) -> i32 {
    if find(stream).is_none() {
        report(Fail::Invalid(EINVAL));
        return EINVAL;
    }
    clearerr(stream);
    0
}

/// Sets how a stream buffers (`_IOFBF`, `_IOLBF`, `_IONBF`), with the
/// title's buffer or one of `size` bytes; before any read, write or move
/// only. 0, or non-zero.
pub extern "win64" fn setvbuf(
    stream: *mut FILE,
    buffer: *mut c_char,
    mode: i32,
    size: usize,
) -> i32 {
    if ![IOFBF, IOLBF, IONBF].contains(&mode)
        || (mode != IONBF && !(2..=i32::MAX as usize).contains(&size))
    {
        report(Fail::Invalid(EINVAL));
        return -1;
    }
    with_stream(stream, false, -1, |state| {
        if state.flags & STARTED != 0 || state.pending > 0 || state.valid > 0 {
            return Err(Fail::Errno(EINVAL));
        }
        if mode == IONBF {
            state.mode = BufferMode::None;
            state.buffer = Buffer::Unmade;
            state.size = 2;
        } else {
            let size = size & !1;
            state.mode = if mode == IOLBF {
                BufferMode::Line
            } else {
                BufferMode::Full
            };
            state.size = size;
            state.buffer = if buffer.is_null() {
                Buffer::Unmade
            } else {
                Buffer::Title(buffer.cast(), size)
            };
        }
        Ok(0)
    })
}

/// `setvbuf(stream, buffer, _IOFBF, BUFSIZ)`, or `_IONBF` for a null
/// buffer.
pub extern "win64" fn setbuf(stream: *mut FILE, buffer: *mut c_char) {
    if buffer.is_null() {
        setvbuf(stream, buffer, IONBF, 0);
    } else {
        setvbuf(stream, buffer, IOFBF, BUFSIZ);
    }
}

/// The stream's descriptor.
pub extern "win64" fn _fileno(stream: *mut FILE) -> i32 {
    let Some(s) = find(stream) else {
        report(Fail::Invalid(EINVAL));
        return -1;
    };
    lock(&s.state).fd
}

/// Locks a stream for the calling thread (recursively).
pub extern "win64" fn _lock_file(stream: *mut FILE) {
    match find(stream) {
        Some(s) => s.lock.acquire(),
        None => report(Fail::Invalid(EINVAL)),
    }
}

/// Lets go of [`_lock_file`]'s lock.
pub extern "win64" fn _unlock_file(stream: *mut FILE) {
    match find(stream) {
        Some(s) => s.lock.release(),
        None => report(Fail::Invalid(EINVAL)),
    }
}

/// The standard stream `index` (0 stdin, 1 stdout, 2 stderr), the same
/// pointer each call.
pub extern "win64" fn __acrt_iob_func(index: u32) -> *mut FILE {
    match standard().get(index as usize) {
        Some(stream) => pointer(stream),
        None => std::ptr::null_mut(),
    }
}

/// No title should look inside a stream: `EINVAL`.
pub extern "win64" fn _get_stream_buffer_pointers(
    _stream: *mut FILE,
    _base: *mut *mut *mut c_char,
    _pointer: *mut *mut *mut c_char,
    _count: *mut *mut i32,
) -> i32 {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!("the title asked for a stream's buffer pointers, which are not given");
    }
    set_errno(EINVAL);
    EINVAL
}

// ---------------------------------------------------------------------------
// Temporary files.

/// A temporary file `GetTempFileNameW` makes in `GetTempPathW`'s folder,
/// NUL-terminated.
fn temp_file() -> Option<Vec<u16>> {
    let mut dir = [0u16; 261];
    let n = GetTempPathW(dir.len() as u32, dir.as_mut_ptr());
    if n == 0 || n as usize >= dir.len() {
        return None;
    }
    let mut name = [0u16; 260];
    let prefix = [u16::from(b't'), 0];
    if GetTempFileNameW(dir.as_ptr(), prefix.as_ptr(), 0, name.as_mut_ptr()) == 0 {
        return None;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(name[..len].iter().copied().chain([0]).collect())
}

/// `tmpfile_s`'s body: a `w+b` stream deleted when it is closed.
fn tmpfile_stream() -> Result<&'static Stream, Fail> {
    let path = temp_file().ok_or_else(|| {
        Fail::Errno(super::lowio::dos_map_err(
            super::super::win32::handles::last_error(),
        ))
    })?;
    let mode: Vec<u16> = "w+bD".encode_utf16().chain([0]).collect();
    let stream = open_stream(Some(path), Some(mode), SH_DENYNO, None)?;
    lock(&stream.state).flags |= TEMPORARY;
    Ok(stream)
}

/// A temporary stream (`w+b`), deleted when closed; null on failure.
pub extern "win64" fn tmpfile() -> *mut FILE {
    match tmpfile_stream() {
        Ok(stream) => pointer(stream),
        Err(fail) => {
            report(fail);
            std::ptr::null_mut()
        }
    }
}

/// See [`tmpfile`]: the stream to `*out`; 0 or the `errno` value.
pub extern "win64" fn tmpfile_s(out: *mut *mut FILE) -> i32 {
    if out.is_null() {
        report(Fail::Invalid(EINVAL));
        return EINVAL;
    }
    let stream = tmpfile();
    // SAFETY: the title's place for the stream.
    unsafe { out.write(stream) };
    if stream.is_null() { errno() } else { 0 }
}

/// Whether a path names nothing.
fn unused(path: &str) -> bool {
    let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
    GetFileAttributesW(wide.as_ptr()) == u32::MAX
}

/// Whether a path names a directory.
fn is_directory(path: &str) -> bool {
    let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
    let attributes = GetFileAttributesW(wide.as_ptr());
    attributes != u32::MAX && attributes & 0x10 != 0
}

/// The temporary folder (`GetTempPathW`), with its trailing `\`.
fn temp_dir() -> String {
    let mut dir = [0u16; 261];
    let n = (GetTempPathW(dir.len() as u32, dir.as_mut_ptr()) as usize).min(dir.len());
    String::from_utf16_lossy(&dir[..n])
}

static TEMP_NUMBER: AtomicU32 = AtomicU32::new(0);

/// A name in `dir` (with its separator) that names nothing:
/// `prefix` and a number after it.
fn unused_name(dir: &str, prefix: &str) -> String {
    let pid = _getpid() as u32;
    loop {
        let n = TEMP_NUMBER.fetch_add(1, Ordering::Relaxed);
        let name = format!("{dir}{prefix}{pid:x}.{n}");
        if unused(&name) {
            return name;
        }
    }
}

/// A file name for `tmpnam`: `s`, the process id and a number, in the
/// temporary folder.
fn tmpnam_name() -> String {
    unused_name(&temp_dir(), "s")
}

thread_local! {
    static TMPNAM_NARROW: UnsafeCell<[u8; TMPNAM_SIZE]> = const { UnsafeCell::new([0; TMPNAM_SIZE]) };
    static TMPNAM_WIDE: UnsafeCell<[u16; TMPNAM_SIZE]> = const { UnsafeCell::new([0; TMPNAM_SIZE]) };
}

/// Copies `text` and a NUL into `buffer` of `size` units; false when it
/// does not fit.
fn copy_name<T: Copy + Default>(text: &[T], buffer: *mut T, size: usize) -> bool {
    if text.len() + 1 > size {
        return false;
    }
    // SAFETY: the buffer holds `size` units.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buffer, text.len());
        buffer.add(text.len()).write(T::default());
    }
    true
}

/// A file name that names nothing, in `buffer` (`L_tmpnam` bytes) or a
/// per-thread buffer.
pub extern "win64" fn tmpnam(buffer: *mut c_char) -> *mut c_char {
    let name = tmpnam_name().into_bytes();
    let target = if buffer.is_null() {
        TMPNAM_NARROW.with(|b| b.get().cast::<u8>())
    } else {
        buffer.cast::<u8>()
    };
    if copy_name(&name, target, TMPNAM_SIZE) {
        target.cast()
    } else {
        std::ptr::null_mut()
    }
}

/// See [`tmpnam`].
pub extern "win64" fn _wtmpnam(buffer: *mut u16) -> *mut u16 {
    let name: Vec<u16> = tmpnam_name().encode_utf16().collect();
    let target = if buffer.is_null() {
        TMPNAM_WIDE.with(|b| b.get().cast::<u16>())
    } else {
        buffer
    };
    if copy_name(&name, target, TMPNAM_SIZE) {
        target
    } else {
        std::ptr::null_mut()
    }
}

/// `tmpnam_s`'s body.
fn tmpnam_s_with<T: Copy + Default>(name: &[T], buffer: *mut T, size: usize) -> i32 {
    if buffer.is_null() || size == 0 {
        report(Fail::Invalid(EINVAL));
        return EINVAL;
    }
    if !copy_name(name, buffer, size) {
        // SAFETY: the title's buffer of `size` units.
        unsafe { buffer.write(T::default()) };
        report(Fail::Invalid(ERANGE));
        return ERANGE;
    }
    0
}

/// See [`tmpnam`], into `buffer` of `size` bytes; 0 or the `errno` value.
pub extern "win64" fn tmpnam_s(buffer: *mut c_char, size: usize) -> i32 {
    tmpnam_s_with(tmpnam_name().as_bytes(), buffer.cast::<u8>(), size)
}

/// See [`tmpnam_s`].
pub extern "win64" fn _wtmpnam_s(buffer: *mut u16, size: usize) -> i32 {
    let name: Vec<u16> = tmpnam_name().encode_utf16().collect();
    tmpnam_s_with(&name, buffer, size)
}

/// `_tempnam`'s body: the folder is `TMP`'s if it is one, else `dir` if it
/// is one, else the temporary folder.
fn tempnam_name(dir: Option<String>, prefix: Option<String>) -> String {
    let tmp = lock(environment())
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("TMP"))
        .map(|(_, value)| value.clone());
    let mut base = [tmp, dir]
        .into_iter()
        .flatten()
        .find(|d| is_directory(d))
        .unwrap_or_else(temp_dir);
    if !base.ends_with('\\') && !base.ends_with('/') {
        base.push('\\');
    }
    unused_name(&base, &prefix.unwrap_or_default())
}

/// A file name that names nothing, in a `malloc` block.
pub extern "win64" fn _tempnam(dir: *const c_char, prefix: *const c_char) -> *mut c_char {
    let text = |p: *const c_char| {
        // SAFETY: the title's string.
        (!p.is_null()).then(|| String::from_utf8_lossy(unsafe { narrow_bytes(p) }).into_owned())
    };
    let name = tempnam_name(text(dir), text(prefix)).into_bytes();
    let block = malloc(name.len() + 1).cast::<u8>();
    if block.is_null() || !copy_name(&name, block, name.len() + 1) {
        return std::ptr::null_mut();
    }
    block.cast()
}

/// See [`_tempnam`].
pub extern "win64" fn _wtempnam(dir: *const u16, prefix: *const u16) -> *mut u16 {
    let text = |p: *const u16| {
        // SAFETY: the title's string.
        (!p.is_null()).then(|| String::from_utf16_lossy(unsafe { wide_units(p) }))
    };
    let name: Vec<u16> = tempnam_name(text(dir), text(prefix))
        .encode_utf16()
        .collect();
    let block = malloc(2 * (name.len() + 1)).cast::<u16>();
    if block.is_null() || !copy_name(&name, block, name.len() + 1) {
        return std::ptr::null_mut();
    }
    block
}

// ---------------------------------------------------------------------------
// printf and scanf.

/// Enables `%n` (or not); the previous setting.
pub extern "win64" fn _set_printf_count_output(value: i32) -> i32 {
    i32::from(COUNT_OUTPUT.swap(value != 0, Ordering::Relaxed))
}

/// Whether `%n` is enabled.
pub extern "win64" fn _get_printf_count_output() -> i32 {
    i32::from(COUNT_OUTPUT.load(Ordering::Relaxed))
}

/// A narrow format string.
fn narrow_spec<'a>(format: *const c_char) -> Option<Spec<'a>> {
    // SAFETY: the title's NUL-terminated format.
    (!format.is_null()).then(|| Spec::Narrow(unsafe { narrow_bytes(format) }))
}

/// A wide format string.
fn wide_spec<'a>(format: *const u16) -> Option<Spec<'a>> {
    // SAFETY: the title's NUL-terminated format.
    (!format.is_null()).then(|| Spec::Wide(unsafe { wide_units(format) }))
}

/// A printf to a stream: formatted whole, then written; the characters, or
/// -1.
fn print_stream(options: u64, stream: *mut FILE, spec: Option<Spec>, args: *const u64) -> i32 {
    let Some(s) = find(stream) else {
        report(Fail::Invalid(EINVAL));
        return -1;
    };
    let Some(spec) = spec else {
        report(Fail::Invalid(EINVAL));
        return -1;
    };
    s.lock.acquire();
    let (open, unicode) = {
        let state = lock(&s.state);
        (state.open, state.open && state.unicode())
    };
    let result = if !open {
        Err(Fail::Errno(EBADF))
    } else if unicode && !spec.is_wide() {
        Err(Fail::Invalid(EINVAL))
    } else {
        let mut sink = VecSink::default();
        match format::format(options, &spec, &mut VaList::new(args), &mut sink) {
            Ok(n) => {
                let bytes = encode(&sink.0, unicode);
                let written = lock(&s.state).write(&bytes);
                Ok(if written == bytes.len() {
                    i32::try_from(n).unwrap_or(-1)
                } else {
                    -1
                })
            }
            Err(code) => Err(Fail::Errno(code)),
        }
    };
    s.lock.release();
    result.unwrap_or_else(|fail| {
        report(fail);
        -1
    })
}

/// `fprintf` and its family.
pub extern "win64" fn __stdio_common_vfprintf(
    options: u64,
    stream: *mut FILE,
    format: *const c_char,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    print_stream(options & !POSITIONAL, stream, narrow_spec(format), args)
}

/// `fprintf_s` and its family.
pub extern "win64" fn __stdio_common_vfprintf_s(
    options: u64,
    stream: *mut FILE,
    format: *const c_char,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    print_stream(options & !POSITIONAL, stream, narrow_spec(format), args)
}

/// `_fprintf_p` and its family: positional arguments allowed.
pub extern "win64" fn __stdio_common_vfprintf_p(
    options: u64,
    stream: *mut FILE,
    format: *const c_char,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    print_stream(options | POSITIONAL, stream, narrow_spec(format), args)
}

/// `fwprintf` and its family.
pub extern "win64" fn __stdio_common_vfwprintf(
    options: u64,
    stream: *mut FILE,
    format: *const u16,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    print_stream(options & !POSITIONAL, stream, wide_spec(format), args)
}

/// `fwprintf_s` and its family.
pub extern "win64" fn __stdio_common_vfwprintf_s(
    options: u64,
    stream: *mut FILE,
    format: *const u16,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    print_stream(options & !POSITIONAL, stream, wide_spec(format), args)
}

/// `_fwprintf_p` and its family.
pub extern "win64" fn __stdio_common_vfwprintf_p(
    options: u64,
    stream: *mut FILE,
    format: *const u16,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    print_stream(options | POSITIONAL, stream, wide_spec(format), args)
}

/// A character of a string buffer.
trait OutChar: Copy {
    const NUL: Self;
    fn from_unit(unit: u16) -> Self;
}

impl OutChar for u8 {
    const NUL: Self = 0;
    fn from_unit(unit: u16) -> Self {
        u8::try_from(unit).unwrap_or(b'?')
    }
}

impl OutChar for u16 {
    const NUL: Self = 0;
    fn from_unit(unit: u16) -> Self {
        unit
    }
}

/// A sink into a title's buffer, which takes at most `room` characters.
struct BufferSink<T> {
    buffer: *mut T,
    room: usize,
    used: usize,
}

impl<T: OutChar> Sink for BufferSink<T> {
    fn put(&mut self, text: &[u16]) -> bool {
        for &unit in text {
            if self.used >= self.room {
                return false;
            }
            // SAFETY: the title's buffer, which holds `room` characters.
            unsafe { self.buffer.add(self.used).write(T::from_unit(unit)) };
            self.used += 1;
        }
        true
    }
}

/// Writes a NUL at `at`.
fn terminate<T: OutChar>(buffer: *mut T, at: usize) {
    // SAFETY: the callers pass an index inside the title's buffer.
    unsafe { buffer.add(at).write(T::NUL) };
}

/// Formats into a buffer that takes at most `room` characters: the length
/// of the whole output, or `None` (the buffer emptied when it has room for
/// a NUL).
fn format_into<T: OutChar>(
    options: u64,
    buffer: *mut T,
    count: usize,
    room: usize,
    spec: &Spec,
    args: *const u64,
) -> Option<usize> {
    let mut sink = BufferSink {
        buffer,
        room,
        used: 0,
    };
    match format::format(options, spec, &mut VaList::new(args), &mut sink) {
        Ok(n) => Some(n),
        Err(code) => {
            set_errno(code);
            if !buffer.is_null() && count > 0 {
                terminate(buffer, 0);
            }
            None
        }
    }
}

fn length_result(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(-1)
}

/// `vsprintf`, `vsnprintf`, `_vsnprintf`, `_vscprintf` and their family,
/// by `options`: the standard behaviour writes at most `count - 1`
/// characters and a NUL and returns the whole length; the legacy one
/// writes at most `count` characters, a NUL only if there is room (or,
/// with the null-termination option, always: truncating), and returns -1
/// when the output did not fit.
fn sprintf<T: OutChar>(
    options: u64,
    buffer: *mut T,
    count: usize,
    spec: Option<Spec>,
    args: *const u64,
) -> i32 {
    let Some(spec) = spec else {
        report(Fail::Invalid(EINVAL));
        return -1;
    };
    if buffer.is_null() && count != 0 {
        report(Fail::Invalid(EINVAL));
        return -1;
    }
    let standard = options & STANDARD_SNPRINTF_BEHAVIOR != 0;
    let room = match (buffer.is_null(), standard) {
        (true, _) => 0,
        (false, true) => count.saturating_sub(1),
        (false, false) => count,
    };
    let Some(len) = format_into(options, buffer, count, room, &spec, args) else {
        return -1;
    };
    if buffer.is_null() {
        return length_result(len);
    }
    if standard {
        if count > 0 {
            terminate(buffer, len.min(count - 1));
        }
        return length_result(len);
    }
    if len < count {
        terminate(buffer, len);
        return length_result(len);
    }
    if options & LEGACY_VSPRINTF_NULL_TERMINATION != 0 {
        if count > 0 {
            terminate(buffer, count - 1);
        }
        return -1;
    }
    if len == count { length_result(len) } else { -1 }
}

/// `vsprintf_s`: the whole output must fit with its NUL; else the buffer
/// is emptied, an invalid parameter, `ERANGE` and -1.
fn sprintf_s<T: OutChar>(
    options: u64,
    buffer: *mut T,
    count: usize,
    spec: Option<Spec>,
    args: *const u64,
) -> i32 {
    let (Some(spec), false, true) = (spec, buffer.is_null(), count > 0) else {
        report(Fail::Invalid(EINVAL));
        return -1;
    };
    let Some(len) = format_into(options, buffer, count, count - 1, &spec, args) else {
        return -1;
    };
    if len < count {
        terminate(buffer, len);
        return length_result(len);
    }
    terminate(buffer, 0);
    report(Fail::Invalid(ERANGE));
    -1
}

/// `vsnprintf_s`: at most `max` characters (`_TRUNCATE`: what fits) and a
/// NUL. Output that does not fit is truncated and -1 when `max` is
/// `_TRUNCATE` or below the buffer's size; else the buffer is emptied, an
/// invalid parameter, `ERANGE` and -1.
fn snprintf_s<T: OutChar>(
    options: u64,
    buffer: *mut T,
    count: usize,
    max: usize,
    spec: Option<Spec>,
    args: *const u64,
) -> i32 {
    let Some(spec) = spec else {
        report(Fail::Invalid(EINVAL));
        return -1;
    };
    if max == 0 && buffer.is_null() && count == 0 {
        return 0;
    }
    if buffer.is_null() || count == 0 {
        report(Fail::Invalid(EINVAL));
        return -1;
    }
    let limit = if count > max { max } else { count - 1 };
    let Some(len) = format_into(options, buffer, count, limit, &spec, args) else {
        return -1;
    };
    if len <= limit {
        terminate(buffer, len);
        return length_result(len);
    }
    if count > max || max == TRUNCATE {
        terminate(buffer, limit);
        return -1;
    }
    terminate(buffer, 0);
    report(Fail::Invalid(ERANGE));
    -1
}

/// `_vsprintf_p`: [`sprintf`] with positional arguments; a failure empties
/// the buffer.
fn sprintf_p<T: OutChar>(
    options: u64,
    buffer: *mut T,
    count: usize,
    spec: Option<Spec>,
    args: *const u64,
) -> i32 {
    let result = sprintf(options | POSITIONAL, buffer, count, spec, args);
    if result < 0 && !buffer.is_null() && count > 0 {
        terminate(buffer, 0);
    }
    result
}

/// See [`sprintf`].
pub extern "win64" fn __stdio_common_vsprintf(
    options: u64,
    buffer: *mut c_char,
    count: usize,
    format: *const c_char,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    sprintf(
        options & !POSITIONAL,
        buffer.cast::<u8>(),
        count,
        narrow_spec(format),
        args,
    )
}

/// See [`sprintf_s`].
pub extern "win64" fn __stdio_common_vsprintf_s(
    options: u64,
    buffer: *mut c_char,
    count: usize,
    format: *const c_char,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    sprintf_s(
        options & !POSITIONAL,
        buffer.cast::<u8>(),
        count,
        narrow_spec(format),
        args,
    )
}

/// See [`sprintf_p`].
pub extern "win64" fn __stdio_common_vsprintf_p(
    options: u64,
    buffer: *mut c_char,
    count: usize,
    format: *const c_char,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    sprintf_p(
        options,
        buffer.cast::<u8>(),
        count,
        narrow_spec(format),
        args,
    )
}

/// See [`snprintf_s`].
pub extern "win64" fn __stdio_common_vsnprintf_s(
    options: u64,
    buffer: *mut c_char,
    count: usize,
    max: usize,
    format: *const c_char,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    snprintf_s(
        options & !POSITIONAL,
        buffer.cast::<u8>(),
        count,
        max,
        narrow_spec(format),
        args,
    )
}

/// See [`sprintf`].
pub extern "win64" fn __stdio_common_vswprintf(
    options: u64,
    buffer: *mut u16,
    count: usize,
    format: *const u16,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    sprintf(
        options & !POSITIONAL,
        buffer,
        count,
        wide_spec(format),
        args,
    )
}

/// See [`sprintf_s`].
pub extern "win64" fn __stdio_common_vswprintf_s(
    options: u64,
    buffer: *mut u16,
    count: usize,
    format: *const u16,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    sprintf_s(
        options & !POSITIONAL,
        buffer,
        count,
        wide_spec(format),
        args,
    )
}

/// See [`sprintf_p`].
pub extern "win64" fn __stdio_common_vswprintf_p(
    options: u64,
    buffer: *mut u16,
    count: usize,
    format: *const u16,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    sprintf_p(options, buffer, count, wide_spec(format), args)
}

/// See [`snprintf_s`].
pub extern "win64" fn __stdio_common_vsnwprintf_s(
    options: u64,
    buffer: *mut u16,
    count: usize,
    max: usize,
    format: *const u16,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    snprintf_s(
        options & !POSITIONAL,
        buffer,
        count,
        max,
        wide_spec(format),
        args,
    )
}

/// A stream as a scan's source: its characters one at a time, the stream
/// locked for each.
struct StreamSource {
    stream: &'static Stream,
    unicode: bool,
}

impl Source for StreamSource {
    fn get(&mut self) -> Option<u16> {
        lock(&self.stream.state).get_unit(self.unicode)
    }

    fn unget(&mut self, c: u16) {
        lock(&self.stream.state).push_back(c, self.unicode);
    }
}

/// A scanf of a stream.
fn scan_stream(options: u64, stream: *mut FILE, spec: Option<Spec>, args: *const u64) -> i32 {
    let Some(s) = find(stream) else {
        report(Fail::Invalid(EINVAL));
        return EOF;
    };
    let Some(spec) = spec else {
        report(Fail::Invalid(EINVAL));
        return EOF;
    };
    s.lock.acquire();
    let (open, unicode) = {
        let state = lock(&s.state);
        (state.open, state.open && state.unicode())
    };
    let result = if !open {
        Err(Fail::Errno(EBADF))
    } else if unicode && !spec.is_wide() {
        Err(Fail::Invalid(EINVAL))
    } else {
        let mut source = StreamSource { stream: s, unicode };
        Ok(scan::scan(
            options,
            &spec,
            &mut VaList::new(args),
            &mut source,
        ))
    };
    s.lock.release();
    result.unwrap_or_else(|fail| {
        report(fail);
        EOF
    })
}

/// A scanf of a string of `count` characters (or up to its NUL).
fn scan_string(options: u64, units: Option<Vec<u16>>, spec: Option<Spec>, args: *const u64) -> i32 {
    let (Some(units), Some(spec)) = (units, spec) else {
        report(Fail::Invalid(EINVAL));
        return EOF;
    };
    let mut source = UnitSource {
        units: &units,
        at: 0,
    };
    scan::scan(options, &spec, &mut VaList::new(args), &mut source)
}

/// The first `count` characters of a string (or up to its NUL), as UTF-16.
fn bounded<T: Copy + Into<u16> + Default + PartialEq>(
    text: *const T,
    count: usize,
) -> Option<Vec<u16>> {
    if text.is_null() {
        return None;
    }
    let mut units = Vec::new();
    while units.len() < count {
        // SAFETY: the title's string, read up to its NUL or `count`.
        let c = unsafe { *text.add(units.len()) };
        if c == T::default() {
            break;
        }
        units.push(c.into());
    }
    Some(units)
}

/// `fscanf` and its family.
pub extern "win64" fn __stdio_common_vfscanf(
    options: u64,
    stream: *mut FILE,
    format: *const c_char,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    scan_stream(options, stream, narrow_spec(format), args)
}

/// `fwscanf` and its family.
pub extern "win64" fn __stdio_common_vfwscanf(
    options: u64,
    stream: *mut FILE,
    format: *const u16,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    scan_stream(options, stream, wide_spec(format), args)
}

/// `sscanf` and its family (`_snscanf`: `count` characters).
pub extern "win64" fn __stdio_common_vsscanf(
    options: u64,
    buffer: *const c_char,
    count: usize,
    format: *const c_char,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    scan_string(
        options,
        bounded(buffer.cast::<u8>(), count),
        narrow_spec(format),
        args,
    )
}

/// `swscanf` and its family.
pub extern "win64" fn __stdio_common_vswscanf(
    options: u64,
    buffer: *const u16,
    count: usize,
    format: *const u16,
    _locale: *mut c_void,
    args: *const u64,
) -> i32 {
    scan_string(options, bounded(buffer, count), wide_spec(format), args)
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(__acrt_iob_func),
        hle_export!(fopen),
        hle_export!(_wfopen),
        hle_export!(fopen_s),
        hle_export!(_wfopen_s),
        hle_export!(_fsopen),
        hle_export!(_wfsopen),
        hle_export!(freopen),
        hle_export!(_wfreopen),
        hle_export!(freopen_s),
        hle_export!(_wfreopen_s),
        hle_export!(_fdopen),
        hle_export!(_wfdopen),
        hle_export!(fclose),
        hle_export!(_fcloseall),
        hle_export!(fread),
        hle_export!(fread_s),
        hle_export!(fwrite),
        hle_export!(fgetc),
        hle_export!(fgetwc),
        hle_export!(getc),
        hle_export!(getwc),
        hle_export!(getchar),
        hle_export!(getwchar),
        hle_export!(fgets),
        hle_export!(fgetws),
        hle_export!(gets_s),
        hle_export!(_getws_s),
        hle_export!(fputc),
        hle_export!(fputwc),
        hle_export!(putc),
        hle_export!(putwc),
        hle_export!(putchar),
        hle_export!(putwchar),
        hle_export!(fputs),
        hle_export!(fputws),
        hle_export!(puts),
        hle_export!(_putws),
        hle_export!(ungetc),
        hle_export!(ungetwc),
        hle_export!(fseek),
        hle_export!(_fseeki64),
        hle_export!(ftell),
        hle_export!(_ftelli64),
        hle_export!(fsetpos),
        hle_export!(fgetpos),
        hle_export!(rewind),
        hle_export!(fflush),
        hle_export!(_flushall),
        hle_export!(feof),
        hle_export!(ferror),
        hle_export!(clearerr),
        hle_export!(clearerr_s),
        hle_export!(setvbuf),
        hle_export!(setbuf),
        hle_export!(_fileno),
        hle_export!(tmpfile),
        hle_export!(tmpfile_s),
        hle_export!(tmpnam),
        hle_export!(_wtmpnam),
        hle_export!(tmpnam_s),
        hle_export!(_wtmpnam_s),
        hle_export!(_tempnam),
        hle_export!(_wtempnam),
        hle_export!(_lock_file),
        hle_export!(_unlock_file),
        hle_export!(_fread_nolock),
        hle_export!(_fread_nolock_s),
        hle_export!(_fwrite_nolock),
        hle_export!(_fseek_nolock),
        hle_export!(_fseeki64_nolock),
        hle_export!(_ftell_nolock),
        hle_export!(_ftelli64_nolock),
        hle_export!(_fgetc_nolock),
        hle_export!(_fgetwc_nolock),
        hle_export!(_fputc_nolock),
        hle_export!(_fputwc_nolock),
        hle_export!(_getc_nolock),
        hle_export!(_getwc_nolock),
        hle_export!(_putc_nolock),
        hle_export!(_putwc_nolock),
        hle_export!(_ungetc_nolock),
        hle_export!(_ungetwc_nolock),
        hle_export!(_fclose_nolock),
        hle_export!(_fflush_nolock),
        hle_export!(_get_stream_buffer_pointers),
        hle_export!(_rmtmp),
        hle_export!(_filbuf),
        hle_export!(_flsbuf),
        hle_export!(_set_printf_count_output),
        hle_export!(_get_printf_count_output),
        hle_export!(__stdio_common_vfprintf),
        hle_export!(__stdio_common_vfprintf_s),
        hle_export!(__stdio_common_vfprintf_p),
        hle_export!(__stdio_common_vfwprintf),
        hle_export!(__stdio_common_vfwprintf_s),
        hle_export!(__stdio_common_vfwprintf_p),
        hle_export!(__stdio_common_vsprintf),
        hle_export!(__stdio_common_vsprintf_s),
        hle_export!(__stdio_common_vsprintf_p),
        hle_export!(__stdio_common_vsnprintf_s),
        hle_export!(__stdio_common_vswprintf),
        hle_export!(__stdio_common_vswprintf_s),
        hle_export!(__stdio_common_vswprintf_p),
        hle_export!(__stdio_common_vsnwprintf_s),
        hle_export!(__stdio_common_vfscanf),
        hle_export!(__stdio_common_vfwscanf),
        hle_export!(__stdio_common_vsscanf),
        hle_export!(__stdio_common_vswscanf),
    ]
}

#[cfg(test)]
mod tests {
    use super::super::format::tests::{Arg, slots};
    use super::super::lowio::tests::{Tolerant, c, exclusive, folder, shared, w};
    use super::super::runtime::errno;
    use super::*;

    #[test]
    fn a_second_fclose_is_ebadf() {
        // A closed stream is used again by the next `fopen`, which may be
        // another test's.
        let _alone = exclusive();
        let dir = folder("stdio-close");
        let stream = open(&format!(r"{dir}	wice.txt"), "w");
        assert_eq!(fclose(stream), 0);
        assert_eq!(fclose(stream), EOF);
        assert_eq!(errno(), EBADF);
        assert_eq!(fputc(i32::from(b'x'), stream), EOF);
        assert_eq!(errno(), EBADF);
        let again = open(&format!(r"{dir}gain.txt"), "w");
        assert_eq!(again, stream, "the closed stream is used again");
        assert_eq!(fclose(again), 0);
    }

    fn open(path: &str, mode: &str) -> *mut FILE {
        let stream = fopen(c(path).as_ptr().cast(), c(mode).as_ptr().cast());
        assert!(!stream.is_null(), "{path} {mode}: errno {}", errno());
        stream
    }

    fn fprintf(stream: *mut FILE, format: &str, args: &[Arg]) -> i32 {
        let slots = slots(args);
        let format = c(format);
        __stdio_common_vfprintf(
            0,
            stream,
            format.as_ptr().cast(),
            std::ptr::null_mut(),
            slots.as_ptr(),
        )
    }

    /// A file's bytes as the disk has them.
    fn bytes_of(path: &str) -> Vec<u8> {
        let stream = open(path, "rb");
        let mut out = vec![0u8; 65536];
        let n = fread(out.as_mut_ptr().cast(), 1, out.len(), stream);
        assert_eq!(fclose(stream), 0);
        out.truncate(n);
        out
    }

    fn write_text(stream: *mut FILE, text: &str) -> usize {
        fwrite(text.as_ptr().cast(), 1, text.len(), stream)
    }

    #[test]
    fn streams_write_read_and_seek() {
        let _shared = shared();
        let dir = folder("stdio-rw");
        let path = format!(r"{dir}\data.txt");
        let stream = open(&path, "w");
        assert_eq!(
            fprintf(
                stream,
                "%d %s\n%5.2f\n",
                &[Arg::I(42), Arg::S("words"), Arg::F(2.5)]
            ),
            15
        );
        assert_eq!(fputs(c("last line").as_ptr().cast(), stream), 0);
        assert_eq!(fclose(stream), 0);
        assert_eq!(bytes_of(&path), b"42 words\r\n 2.50\r\nlast line");

        let stream = open(&path, "r");
        let mut line = [0u8; 32];
        assert!(!fgets(line.as_mut_ptr().cast(), 32, stream).is_null());
        assert_eq!(&line[..10], b"42 words\n\0");
        assert_eq!(ftell(stream), 10);
        assert_eq!(fgetc(stream), i32::from(b' '));
        assert_eq!(ungetc(i32::from(b'#'), stream), i32::from(b'#'));
        assert_eq!(fgetc(stream), i32::from(b'#'));
        let mut rest = [0u8; 64];
        let n = fread(rest.as_mut_ptr().cast(), 1, 64, stream);
        assert_eq!(&rest[..n], b"2.50\nlast line");
        assert_ne!(feof(stream), 0);
        assert_eq!(ferror(stream), 0);
        assert_eq!(fgetc(stream), EOF);
        assert_eq!(fseek(stream, 3, 0), 0);
        assert_eq!(feof(stream), 0);
        assert_eq!(ftell(stream), 3);
        assert_eq!(fgetc(stream), i32::from(b'w'));
        assert_eq!(fseek(stream, -4, 2), 0);
        assert_eq!(_ftelli64(stream), 22);
        assert_eq!(fgetc(stream), i32::from(b'l'));
        assert_eq!(fseek(stream, 2, 1), 0);
        assert_eq!(fgetc(stream), i32::from(b'e'));
        let mut position = 0i64;
        assert_eq!(fgetpos(stream, &mut position), 0);
        rewind(stream);
        assert_eq!(fgetc(stream), i32::from(b'4'));
        assert_eq!(fsetpos(stream, &position), 0);
        assert_eq!(fgetc(stream), EOF);
        assert_eq!(fclose(stream), 0);

        let stream = open(&path, "rb");
        let mut buffer = [0u8; 5];
        assert_eq!(fread(buffer.as_mut_ptr().cast(), 2, 2, stream), 2);
        assert_eq!(&buffer[..4], b"42 w");
        // Mixing reads and writes on an update stream.
        assert_eq!(fclose(stream), 0);
        let stream = open(&path, "r+b");
        assert_eq!(fgetc(stream), i32::from(b'4'));
        assert_eq!(fseek(stream, 0, 1), 0);
        assert_eq!(fputc(i32::from(b'X'), stream), i32::from(b'X'));
        assert_eq!(fflush(stream), 0);
        assert_eq!(fclose(stream), 0);
        assert_eq!(&bytes_of(&path)[..3], b"4X ");
    }

    #[test]
    fn appends_large_writes_and_buffering() {
        let _shared = shared();
        let dir = folder("stdio-append");
        let path = format!(r"{dir}\log.txt");
        let stream = open(&path, "wb");
        assert_eq!(write_text(stream, "abc"), 3);
        assert_eq!(fclose(stream), 0);
        let stream = open(&path, "a+b");
        assert_eq!(write_text(stream, "de"), 2);
        assert_eq!(fseek(stream, 0, 0), 0);
        let mut out = [0u8; 16];
        assert_eq!(fread(out.as_mut_ptr().cast(), 1, 16, stream), 5);
        assert_eq!(&out[..5], b"abcde");
        assert_eq!(write_text(stream, "f"), 1);
        assert_eq!(fclose(stream), 0);
        assert_eq!(bytes_of(&path), b"abcdef");

        // Larger than the buffer: written through it.
        let big: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let path = format!(r"{dir}\big.bin");
        let stream = open(&path, "wb");
        assert_eq!(fwrite(big.as_ptr().cast(), 1, big.len(), stream), big.len());
        assert_eq!(lowio::_filelengthi64(_fileno(stream)), 10_000);
        assert_eq!(fclose(stream), 0);
        assert_eq!(bytes_of(&path), big);
        let stream = open(&path, "rb");
        let mut back = vec![0u8; 10_000];
        assert_eq!(fread(back.as_mut_ptr().cast(), 100, 100, stream), 100);
        assert_eq!(back, big);
        assert_eq!(fclose(stream), 0);

        // Buffered output waits for a flush; unbuffered goes at once.
        let path = format!(r"{dir}\buffered.txt");
        let stream = open(&path, "wb");
        assert_eq!(write_text(stream, "xyz"), 3);
        assert_eq!(lowio::_filelengthi64(_fileno(stream)), 0);
        assert_eq!(fflush(std::ptr::null_mut()), 0);
        assert_eq!(lowio::_filelengthi64(_fileno(stream)), 3);
        assert_eq!(fclose(stream), 0);
        let path = format!(r"{dir}\unbuffered.txt");
        let stream = open(&path, "wb");
        assert_eq!(setvbuf(stream, std::ptr::null_mut(), IONBF, 0), 0);
        assert_eq!(write_text(stream, "xyz"), 3);
        assert_eq!(lowio::_filelengthi64(_fileno(stream)), 3);
        assert_ne!(
            setvbuf(stream, std::ptr::null_mut(), IOFBF, 64),
            0,
            "too late"
        );
        assert_eq!(fclose(stream), 0);
        // A title's buffer.
        let mut own = [0u8; 16];
        let stream = open(&path, "wb");
        assert_eq!(
            setvbuf(stream, own.as_mut_ptr().cast(), IOFBF, own.len()),
            0
        );
        assert_eq!(write_text(stream, "12345"), 5);
        assert_eq!(&own[..5], b"12345");
        assert_eq!(fclose(stream), 0);
        assert_eq!(bytes_of(&path), b"12345");
    }

    #[test]
    fn text_streams_tell_the_position_on_disk() {
        let _shared = shared();
        let dir = folder("stdio-tell");
        let path = format!(r"{dir}\lines.txt");
        let stream = open(&path, "w");
        assert_eq!(write_text(stream, "a\nb\n"), 4);
        assert_eq!(ftell(stream), 6);
        assert_eq!(fclose(stream), 0);
        let stream = open(&path, "r");
        assert_eq!(fgetc(stream), i32::from(b'a'));
        assert_eq!(ftell(stream), 1);
        assert_eq!(fgetc(stream), i32::from(b'\n'));
        assert_eq!(ftell(stream), 3);
        assert_eq!(fclose(stream), 0);
    }

    #[test]
    fn modes_are_checked() {
        let _shared = shared();
        let _tolerant = Tolerant::new();
        let dir = folder("stdio-modes");
        let path = format!(r"{dir}\m.txt");
        for bad in ["", "q", "rr+", "r++", "wbt", "rx", "w, ccs=EBCDIC", "w,ccs"] {
            let stream = fopen(c(&path).as_ptr().cast(), c(bad).as_ptr().cast());
            assert!(stream.is_null(), "{bad}");
            assert_eq!(errno(), EINVAL, "{bad}");
        }
        let stream = open(&path, "wx");
        assert_eq!(fclose(stream), 0);
        assert!(fopen(c(&path).as_ptr().cast(), c("wx").as_ptr().cast()).is_null());
        assert_eq!(errno(), super::super::runtime::EEXIST);
        let missing = format!(r"{dir}\missing.txt");
        let mut out = std::ptr::dangling_mut();
        assert_eq!(
            fopen_s(
                &mut out,
                c(&missing).as_ptr().cast(),
                c("r").as_ptr().cast()
            ),
            super::super::runtime::ENOENT
        );
        assert!(out.is_null());
        assert_eq!(
            _wfopen_s(&mut out, w(&path).as_ptr(), w("r+tcN").as_ptr()),
            0
        );
        assert_eq!(fclose(out), 0);
        assert!(fgetc(std::ptr::null_mut()) == EOF && errno() == EINVAL);
        // A stream open for reading only does not write.
        let stream = open(&path, "r");
        assert_eq!(fputc(i32::from(b'x'), stream), EOF);
        assert_ne!(ferror(stream), 0);
        clearerr(stream);
        assert_eq!(ferror(stream), 0);
        assert_eq!(fclose(stream), 0);
    }

    #[test]
    fn temporary_files_and_names() {
        let _shared = shared();
        let _dir = folder("stdio-temp");
        let mut stream = std::ptr::null_mut();
        assert_eq!(tmpfile_s(&mut stream), 0);
        assert!(!stream.is_null());
        let handle = lowio::_get_osfhandle(_fileno(stream));
        let mut name = [0u16; 260];
        let len = super::super::super::win32::files::GetFinalPathNameByHandleW(
            handle as *mut c_void,
            name.as_mut_ptr(),
            260,
            0,
        );
        assert!(len > 0 && (len as usize) < 260);
        let path = String::from_utf16_lossy(&name[..len as usize]);
        assert_eq!(
            super::super::lowio::_waccess(w(&path).as_ptr(), 0),
            0,
            "{path}"
        );
        assert_eq!(write_text(stream, "scratch"), 7);
        rewind(stream);
        let mut back = [0u8; 7];
        assert_eq!(fread(back.as_mut_ptr().cast(), 1, 7, stream), 7);
        assert_eq!(&back, b"scratch");
        assert_eq!(fclose(stream), 0);
        assert_eq!(
            super::super::lowio::_waccess(w(&path).as_ptr(), 0),
            -1,
            "{path}"
        );

        let mut buffer = [0u8; 260];
        assert_eq!(
            tmpnam(buffer.as_mut_ptr().cast()).cast::<u8>(),
            buffer.as_mut_ptr()
        );
        let first =
            String::from_utf8_lossy(&buffer[..buffer.iter().position(|&b| b == 0).unwrap()])
                .into_owned();
        assert_eq!(tmpnam_s(buffer.as_mut_ptr().cast(), 260), 0);
        let second =
            String::from_utf8_lossy(&buffer[..buffer.iter().position(|&b| b == 0).unwrap()])
                .into_owned();
        assert_ne!(first, second);
        let named = _tempnam(std::ptr::null(), c("pre").as_ptr().cast());
        assert!(!named.is_null());
        // SAFETY: a NUL-terminated block of ours.
        let text = String::from_utf8_lossy(unsafe { narrow_bytes(named) }).into_owned();
        assert!(text.contains("pre"), "{text}");
        super::super::heap::free(named.cast());
    }

    #[test]
    fn fscanf_reads_a_stream() {
        let _shared = shared();
        let dir = folder("stdio-scan");
        let path = format!(r"{dir}\numbers.txt");
        let stream = open(&path, "w");
        assert_eq!(write_text(stream, "17 2.5 word\nnext"), 16);
        assert_eq!(fclose(stream), 0);
        let stream = open(&path, "r");
        let (mut n, mut f, mut word) = (0i32, 0f64, [0u8; 8]);
        let args = slots(&[
            Arg::P(&raw mut n as usize),
            Arg::P(&raw mut f as usize),
            Arg::P(word.as_mut_ptr() as usize),
        ]);
        let format = c("%d %lf %s");
        let read = __stdio_common_vfscanf(
            0,
            stream,
            format.as_ptr().cast(),
            std::ptr::null_mut(),
            args.as_ptr(),
        );
        assert_eq!(read, 3);
        assert_eq!((n, f, &word[..5]), (17, 2.5, &b"word\0"[..]));
        let mut line = [0u8; 16];
        assert!(!fgets(line.as_mut_ptr().cast(), 16, stream).is_null());
        assert_eq!(&line[..2], b"\n\0", "the scan left the rest of the line");
        assert!(!fgets(line.as_mut_ptr().cast(), 16, stream).is_null());
        assert_eq!(&line[..5], b"next\0");
        let none = __stdio_common_vfscanf(
            0,
            stream,
            format.as_ptr().cast(),
            std::ptr::null_mut(),
            args.as_ptr(),
        );
        assert_eq!(none, EOF);
        assert_eq!(fclose(stream), 0);

        let text = c("5,6");
        let (mut a, mut b) = (0i32, 0i32);
        let args = slots(&[Arg::P(&raw mut a as usize), Arg::P(&raw mut b as usize)]);
        let format = c("%d,%d");
        let got = __stdio_common_vsscanf(
            0,
            text.as_ptr().cast(),
            usize::MAX,
            format.as_ptr().cast(),
            std::ptr::null_mut(),
            args.as_ptr(),
        );
        assert_eq!((got, a, b), (2, 5, 6));
        let got = __stdio_common_vsscanf(
            0,
            text.as_ptr().cast(),
            2,
            format.as_ptr().cast(),
            std::ptr::null_mut(),
            args.as_ptr(),
        );
        assert_eq!(got, 1, "a bounded string ends at its count");
    }

    #[test]
    fn freopen_moves_stdout_to_a_file() {
        // Descriptor 1 is freed for a moment, which another test could get.
        let _alone = exclusive();
        let dir = folder("stdio-freopen");
        let path = format!(r"{dir}\out.txt");
        let out = __acrt_iob_func(1);
        assert_eq!(out, __acrt_iob_func(1));
        assert_ne!(out, __acrt_iob_func(2));
        let moved = freopen(c(&path).as_ptr().cast(), c("w").as_ptr().cast(), out);
        assert_eq!(moved, out);
        assert_eq!(puts(c("hello").as_ptr().cast()), 0);
        assert_eq!(putchar(i32::from(b'!')), i32::from(b'!'));
        assert_eq!(fflush(out), 0);
        // Put stdout back on the host's stream.
        {
            let mut state = lock(&standard()[1].state);
            state.close();
            *state = State::opened(1, WRITE, BufferMode::None);
        }
        lowio::reset_standard(1);
        assert_eq!(bytes_of(&path), b"hello\r\n!");
    }

    #[test]
    fn wide_streams_convert_through_the_descriptor() {
        let _shared = shared();
        let dir = folder("stdio-wide");
        let path = format!(r"{dir}\wide.txt");
        let stream = _wfopen(w(&path).as_ptr(), w("w, ccs=UTF-8").as_ptr());
        assert!(!stream.is_null());
        assert_eq!(fputws(w("héllo\n").as_ptr(), stream), 0);
        assert_eq!(fputwc(0x3A9, stream), 0x3A9);
        {
            let _tolerant = Tolerant::new();
            assert_eq!(
                fputc(i32::from(b'x'), stream),
                EOF,
                "narrow on a Unicode stream"
            );
            assert_eq!(errno(), EINVAL);
        }
        assert_eq!(fclose(stream), 0);
        assert_eq!(bytes_of(&path), "\u{FEFF}héllo\r\nΩ".as_bytes());
        let stream = _wfopen(w(&path).as_ptr(), w("r, ccs=UTF-8").as_ptr());
        let mut line = [0u16; 16];
        assert!(!fgetws(line.as_mut_ptr(), 16, stream).is_null());
        assert_eq!(&line[..7], &w("héllo\n")[..]);
        assert_eq!(fgetwc(stream), 0x3A9);
        assert_eq!(fgetwc(stream), WEOF);
        assert_eq!(ungetwc(0x41, stream), 0x41);
        assert_eq!(fgetwc(stream), 0x41);
        assert_eq!(fclose(stream), 0);
        // A wide function on an ANSI stream writes bytes.
        let narrow = format!(r"{dir}\narrow.txt");
        let stream = open(&narrow, "wb");
        assert_eq!(fputws(w("aé\u{3A9}").as_ptr(), stream), 0);
        assert_eq!(fclose(stream), 0);
        assert_eq!(bytes_of(&narrow), b"a\xE9?");
        let stream = open(&narrow, "rb");
        assert_eq!(fgetwc(stream), u16::from(b'a'));
        assert_eq!(fgetwc(stream), 0xE9);
        assert_eq!(fclose(stream), 0);
        let wide_format = w("%s=%d");
        let args = slots(&[Arg::W(&w("ab")[..2]), Arg::I(3)]);
        let stream = open(&narrow, "w");
        let n = __stdio_common_vfwprintf(
            0,
            stream,
            wide_format.as_ptr(),
            std::ptr::null_mut(),
            args.as_ptr(),
        );
        assert_eq!(n, 4);
        assert_eq!(fclose(stream), 0);
        assert_eq!(bytes_of(&narrow), b"ab=3");
    }

    fn vsprintf(options: u64, count: usize, format: &str, args: &[Arg]) -> (i32, Vec<u8>) {
        let mut buffer = vec![b'#'; 16];
        let slots = slots(args);
        let format = c(format);
        let n = __stdio_common_vsprintf(
            options,
            buffer.as_mut_ptr().cast(),
            count,
            format.as_ptr().cast(),
            std::ptr::null_mut(),
            slots.as_ptr(),
        );
        (n, buffer)
    }

    #[test]
    fn string_printf_follows_each_option() {
        let _tolerant = Tolerant::new();
        let standard = STANDARD_SNPRINTF_BEHAVIOR;
        let legacy_null = LEGACY_VSPRINTF_NULL_TERMINATION;
        let args = [Arg::I(12345)];
        // snprintf: count - 1 characters and a NUL, the whole length back.
        let (n, b) = vsprintf(standard, 4, "%d", &args);
        assert_eq!((n, &b[..5]), (5, &b"123\0#"[..]));
        let (n, b) = vsprintf(standard, 8, "%d", &args);
        assert_eq!((n, &b[..7]), (5, &b"12345\0#"[..]));
        // _vsnprintf: count characters, a NUL only with room, -1 on overflow.
        let (n, b) = vsprintf(0, 4, "%d", &args);
        assert_eq!((n, &b[..5]), (-1, &b"1234#"[..]));
        let (n, b) = vsprintf(0, 5, "%d", &args);
        assert_eq!((n, &b[..6]), (5, &b"12345#"[..]));
        let (n, b) = vsprintf(0, 6, "%d", &args);
        assert_eq!((n, &b[..6]), (5, &b"12345\0"[..]));
        // vsprintf: no bound; with a bound, truncated and terminated.
        let (n, b) = vsprintf(legacy_null, usize::MAX, "%d", &args);
        assert_eq!((n, &b[..6]), (5, &b"12345\0"[..]));
        let (n, b) = vsprintf(legacy_null, 4, "%d", &args);
        assert_eq!((n, &b[..5]), (-1, &b"123\0#"[..]));
        // A count of the output only (_scprintf).
        let slots_ = slots(&args);
        let format = c("%d!");
        let n = __stdio_common_vsprintf(
            standard,
            std::ptr::null_mut(),
            0,
            format.as_ptr().cast(),
            std::ptr::null_mut(),
            slots_.as_ptr(),
        );
        assert_eq!(n, 6);
        let n = __stdio_common_vsprintf(
            0,
            std::ptr::null_mut(),
            4,
            format.as_ptr().cast(),
            std::ptr::null_mut(),
            slots_.as_ptr(),
        );
        assert_eq!((n, errno()), (-1, EINVAL));

        // sprintf_s: all or nothing.
        let mut buffer = [b'#'; 8];
        let n = __stdio_common_vsprintf_s(
            0,
            buffer.as_mut_ptr().cast(),
            6,
            format.as_ptr().cast(),
            std::ptr::null_mut(),
            slots_.as_ptr(),
        );
        assert_eq!((n, buffer[0], errno()), (-1, 0, ERANGE));
        let n = __stdio_common_vsprintf_s(
            0,
            buffer.as_mut_ptr().cast(),
            7,
            format.as_ptr().cast(),
            std::ptr::null_mut(),
            slots_.as_ptr(),
        );
        assert_eq!((n, &buffer[..7]), (6, &b"12345!\0"[..]));

        // vsnprintf_s: _TRUNCATE truncates; a max below the size truncates;
        // otherwise the buffer is emptied.
        let mut buffer = [b'#'; 8];
        let call = |buffer: &mut [u8; 8], count: usize, max: usize| {
            __stdio_common_vsnprintf_s(
                0,
                buffer.as_mut_ptr().cast(),
                count,
                max,
                format.as_ptr().cast(),
                std::ptr::null_mut(),
                slots_.as_ptr(),
            )
        };
        assert_eq!(call(&mut buffer, 4, TRUNCATE), -1);
        assert_eq!(&buffer[..4], b"123\0");
        assert_eq!(call(&mut buffer, 8, 2), -1);
        assert_eq!(&buffer[..3], b"12\0");
        assert_eq!(call(&mut buffer, 8, 10), 6);
        assert_eq!(&buffer[..7], b"12345!\0");
        buffer = [b'#'; 8];
        assert_eq!(call(&mut buffer, 4, 10), -1);
        assert_eq!((buffer[0], errno()), (0, ERANGE));

        // Positional arguments, in the `_p` function only.
        let positional = c("%2$s-%1$d");
        let pargs = slots(&[Arg::I(7), Arg::S("x")]);
        let mut buffer = [b'#'; 8];
        let n = __stdio_common_vsprintf_p(
            standard,
            buffer.as_mut_ptr().cast(),
            8,
            positional.as_ptr().cast(),
            std::ptr::null_mut(),
            pargs.as_ptr(),
        );
        assert_eq!((n, &buffer[..4]), (3, &b"x-7\0"[..]));
        let n = __stdio_common_vsprintf(
            standard,
            buffer.as_mut_ptr().cast(),
            8,
            positional.as_ptr().cast(),
            std::ptr::null_mut(),
            pargs.as_ptr(),
        );
        assert_eq!((n, buffer[0], errno()), (-1, 0, EINVAL));

        // The wide forms.
        let mut wide = [0u16; 8];
        let wformat = w("%ls%c");
        let wargs = slots(&[Arg::W(&w("ab")[..2]), Arg::U(0x3A9)]);
        let n = __stdio_common_vswprintf(
            standard,
            wide.as_mut_ptr(),
            8,
            wformat.as_ptr(),
            std::ptr::null_mut(),
            wargs.as_ptr(),
        );
        assert_eq!((n, &wide[..4]), (3, &w("abΩ")[..]));
        let n = __stdio_common_vsnwprintf_s(
            0,
            wide.as_mut_ptr(),
            3,
            TRUNCATE,
            wformat.as_ptr(),
            std::ptr::null_mut(),
            wargs.as_ptr(),
        );
        assert_eq!((n, &wide[..3]), (-1, &w("ab")[..]));
    }

    #[test]
    fn locks_nest_and_count_output_is_a_setting() {
        let out = __acrt_iob_func(2);
        _lock_file(out);
        _lock_file(out);
        assert_eq!(_fileno(out), 2);
        _unlock_file(out);
        _unlock_file(out);
        assert!(matches!(_get_printf_count_output(), 0 | 1));
        assert_eq!(_fileno(__acrt_iob_func(0)), 0);
        assert!(__acrt_iob_func(3).is_null());
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 109);
    }
}
