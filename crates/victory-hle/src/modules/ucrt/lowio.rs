//! File descriptors and the file system (`api-ms-win-crt-stdio`'s low-level
//! I/O, `api-ms-win-crt-filesystem`): `_open` and its family over a table
//! of descriptors, each a Win32 file handle with the UCRT's flags and text
//! mode, and the path functions (`_stat`, `_access`, `_findfirst`,
//! `_fullpath`, `_splitpath`, ...). Everything goes through `win32::files`
//! (called as Rust functions), so paths, drives and errors are the ones the
//! title's own Win32 calls see; a Win32 error becomes `errno` through
//! [`dos_map_err`].
//!
//! Narrow paths are read as UTF-8 and narrow paths given back are UTF-8, as
//! `win32::files`' `A` functions read them.
//!
//! Text mode: a write turns `\n` into `\r\n`, a read turns `\r\n` back and
//! stops at `^Z`; `_O_U16TEXT`/`_O_WTEXT` descriptors take UTF-16 and write
//! it as it is, `_O_U8TEXT` ones take UTF-16 and write UTF-8 (a device in
//! either: UTF-8), with a byte order mark written on a fresh file and
//! skipped (and obeyed) on read.
//!
//! Limits: sharing modes are passed on and not enforced (`CreateFileW`
//! does not); `_locking` locks nothing; `_pipe` fails; closing a
//! descriptor whose handle is a standard one (`GetStdHandle`) does not
//! close the handle; `_getdcwd` of another drive than the current one is
//! its root (there is one current directory); a UTF-8 read may split a
//! character no buffer can hold (a four-byte one into a two-byte buffer
//! reads as U+FFFD).
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// The UCRT's signatures, as they are.
#![allow(clippy::too_many_arguments)]

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard, Once};

use super::super::win32::files::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, DeleteFileW, FindClose,
    FindFirstFileW, FindNextFileW, FlushFileBuffers, GetCurrentDirectoryW, GetFileAttributesExW,
    GetFileAttributesW, GetFileInformationByHandle, GetFileType, GetFullPathNameW,
    GetLogicalDrives, GetStdHandle, MoveFileExW, ReadFile, RemoveDirectoryW, SetCurrentDirectoryW,
    SetEndOfFile, SetFileAttributesW, SetFilePointerEx, WIN32_FILE_ATTRIBUTE_DATA,
    WIN32_FIND_DATAW, WriteFile,
};
use super::super::win32::handles::{CloseHandle, DuplicateHandle, GetCurrentProcess, last_error};
use super::super::win32::time::FILETIME;
use super::heap::malloc;
use super::runtime::{
    _get_fmode, _getpid, _invalid_parameter_noinfo, _set_doserrno, EACCES, EAGAIN, EBADF, EEXIST,
    EFBIG, EINVAL, EMFILE, ENOENT, ENOEXEC, ENOMEM, ENOSPC, ENOTEMPTY, EPIPE, ERANGE, EXDEV, lock,
    set_errno,
};
use super::string::{narrow_bytes, wide_units};
use crate::{HleExport, hle_export};

// `fcntl.h`.
pub(crate) const O_RDONLY: i32 = 0x0000;
pub(crate) const O_WRONLY: i32 = 0x0001;
pub(crate) const O_RDWR: i32 = 0x0002;
pub(crate) const O_APPEND: i32 = 0x0008;
pub(crate) const O_TEMPORARY: i32 = 0x0040;
pub(crate) const O_NOINHERIT: i32 = 0x0080;
pub(crate) const O_CREAT: i32 = 0x0100;
pub(crate) const O_TRUNC: i32 = 0x0200;
pub(crate) const O_EXCL: i32 = 0x0400;
const O_OBTAIN_DIR: i32 = 0x2000;
pub(crate) const O_TEXT: i32 = 0x4000;
pub(crate) const O_BINARY: i32 = 0x8000;
pub(crate) const O_WTEXT: i32 = 0x1_0000;
pub(crate) const O_U16TEXT: i32 = 0x2_0000;
pub(crate) const O_U8TEXT: i32 = 0x4_0000;
const O_UNICODE: i32 = O_WTEXT | O_U16TEXT | O_U8TEXT;

// `share.h`.
const SH_DENYRW: i32 = 0x10;
const SH_DENYWR: i32 = 0x20;
const SH_DENYRD: i32 = 0x30;
pub(crate) const SH_DENYNO: i32 = 0x40;

// `sys/stat.h`.
const S_IFDIR: u16 = 0x4000;
const S_IFCHR: u16 = 0x2000;
const S_IFIFO: u16 = 0x1000;
const S_IFREG: u16 = 0x8000;
pub(crate) const S_IREAD: i32 = 0x100;
pub(crate) const S_IWRITE: i32 = 0x80;
const S_IEXEC: i32 = 0x40;

/// The most descriptors there may be (the UCRT's `_NHANDLE_`).
const MAX_DESCRIPTORS: usize = 8192;

// The UCRT's `__crt_lowio_handle_data` flags.
const FOPEN: u8 = 0x01;
const FEOFLAG: u8 = 0x02;
const FPIPE: u8 = 0x08;
const FNOINHERIT: u8 = 0x10;
const FAPPEND: u8 = 0x20;
const FDEV: u8 = 0x40;
const FTEXT: u8 = 0x80;

/// The `^Z` that ends a text file.
const CTRL_Z: u8 = 0x1A;

// Win32.
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 1;
const FILE_SHARE_WRITE: u32 = 2;
const CREATE_NEW: u32 = 1;
const CREATE_ALWAYS: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const OPEN_ALWAYS: u32 = 4;
const TRUNCATE_EXISTING: u32 = 5;
const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
const FILE_TYPE_UNKNOWN: u32 = 0;
const FILE_TYPE_CHAR: u32 = 2;
const FILE_TYPE_PIPE: u32 = 3;
const FILE_BEGIN: u32 = 0;
const FILE_CURRENT: u32 = 1;
const FILE_END: u32 = 2;
const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
const INVALID_HANDLE: usize = usize::MAX;
const DUPLICATE_SAME_ACCESS: u32 = 2;
const MOVEFILE_COPY_ALLOWED: u32 = 2;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_INVALID_DRIVE: u32 = 15;
const ERROR_NO_MORE_FILES: u32 = 18;
const ERROR_BROKEN_PIPE: u32 = 109;
const ERROR_ALREADY_EXISTS: u32 = 183;
const ERROR_FILENAME_EXCED_RANGE: u32 = 206;

/// 100-nanosecond intervals between 1601-01-01 and 1970-01-01.
const EPOCH_DIFFERENCE: i64 = 116_444_736_000_000_000;

/// `_MAX_PATH`, `_MAX_DRIVE`, `_MAX_DIR`, `_MAX_FNAME`, `_MAX_EXT`.
const MAX_PATH: usize = 260;
const MAX_DRIVE: usize = 3;
const MAX_DIR: usize = 256;
const MAX_FNAME: usize = 256;
const MAX_EXT: usize = 256;

/// The UCRT's `_dosmaperr`: the `errno` value of a Win32 error, which is
/// also stored in `_doserrno`; `errno` is set to it and it is returned.
pub(crate) fn dos_map_err(code: u32) -> i32 {
    _set_doserrno(code);
    let errno = match code {
        2 | 3 | 15 | 18 | 53 | 67 | 123 | 161 | 206 => ENOENT,
        4 => EMFILE,
        5 | 16 | 19 | 32 | 33 | 65 | 82 | 83 | 108 | 132 | 158 | 167 => EACCES,
        6 | 114 | 130 => EBADF,
        7 | 8 | 9 | 1816 => ENOMEM,
        10 | 11 => ENOEXEC,
        12 | 13 | 87 | 131 => EINVAL,
        17 => EXDEV,
        112 => ENOSPC,
        109 => EPIPE,
        145 => ENOTEMPTY,
        80 | 183 => EEXIST,
        89 | 164 | 215 => EAGAIN,
        19..=36 => EACCES,
        188..=202 => ENOEXEC,
        _ => EINVAL,
    };
    set_errno(errno);
    errno
}

/// [`dos_map_err`] of the calling thread's last error.
fn os_error() -> i32 {
    dos_map_err(last_error())
}

/// Sets `errno` and gives it back.
fn error(code: i32) -> i32 {
    set_errno(code);
    code
}

/// Sets `errno` and reports an invalid parameter; gives `code` back.
fn invalid(code: i32) -> i32 {
    set_errno(code);
    _invalid_parameter_noinfo();
    code
}

/// A bad descriptor: `_doserrno` 0, `EBADF`, an invalid parameter.
fn bad_fd() -> i32 {
    _set_doserrno(0);
    invalid(EBADF)
}

/// A read or write's Win32 error: access denied is `EBADF`, as the UCRT
/// has it.
fn io_error(code: u32) -> i32 {
    if code == ERROR_ACCESS_DENIED {
        _set_doserrno(code);
        return error(EBADF);
    }
    dos_map_err(code)
}

/// `Ok(value)` as is, `Err(_)` as `failure` (errno is already set).
fn or<T>(result: Result<T, i32>, failure: T) -> T {
    result.unwrap_or(failure)
}

// ---------------------------------------------------------------------------
// The descriptor table.

/// How a text descriptor's characters are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextMode {
    Ansi,
    Utf8,
    Utf16,
}

/// A descriptor: the UCRT's `__crt_lowio_handle_data`, what of it these
/// functions use.
#[derive(Debug, Clone, Copy)]
struct Fd {
    handle: usize,
    flags: u8,
    text_mode: TextMode,
    /// The byte a text read of a device looked at past a `\r`.
    pipe_char: Option<u8>,
}

static TABLE: Mutex<Vec<Option<Fd>>> = Mutex::new(Vec::new());
static STANDARD: Once = Once::new();

/// The table, with descriptors 0, 1 and 2 made on first use from the
/// standard handles.
fn table() -> MutexGuard<'static, Vec<Option<Fd>>> {
    STANDARD.call_once(|| {
        let mut table = lock(&TABLE);
        while table.len() < 3 {
            table.push(None);
        }
        for (fd, id) in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .into_iter()
            .enumerate()
        {
            table[fd].get_or_insert_with(|| standard_fd(id));
        }
    });
    lock(&TABLE)
}

fn standard_fd(id: u32) -> Fd {
    Fd {
        handle: GetStdHandle(id) as usize,
        flags: FOPEN | FDEV | FTEXT,
        text_mode: TextMode::Ansi,
        pipe_char: None,
    }
}

/// Whether a handle is one of the standard ones.
fn is_standard_handle(handle: usize) -> bool {
    [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
        .into_iter()
        .any(|id| GetStdHandle(id) as usize == handle)
}

/// Runs `body` on an open descriptor, the table locked; a bad one is
/// [`bad_fd`].
fn with_fd<R>(fd: i32, body: impl FnOnce(&mut Fd) -> Result<R, i32>) -> Result<R, i32> {
    let mut table = table();
    let index = usize::try_from(fd)
        .ok()
        .filter(|&i| table.get(i).is_some_and(Option::is_some));
    match index {
        Some(i) => match &mut table[i] {
            Some(entry) => body(entry),
            None => unreachable!("checked above"),
        },
        None => {
            drop(table);
            Err(bad_fd())
        }
    }
}

/// A copy of an open descriptor.
fn fd_copy(fd: i32) -> Result<Fd, i32> {
    with_fd(fd, |f| Ok(*f))
}

/// Puts `entry` in the lowest free descriptor; `EMFILE` when there is none.
fn allocate(entry: Fd) -> Result<i32, i32> {
    let mut table = table();
    let index = match table.iter().position(Option::is_none) {
        Some(i) => i,
        None if table.len() < MAX_DESCRIPTORS => {
            table.push(None);
            table.len() - 1
        }
        None => {
            _set_doserrno(0);
            return Err(error(EMFILE));
        }
    };
    table[index] = Some(entry);
    Ok(index as i32)
}

/// A descriptor's text mode: `None` for a binary one (for `stdio.rs`).
pub(crate) fn text_mode(fd: i32) -> Option<TextMode> {
    let table = table();
    let entry = usize::try_from(fd)
        .ok()
        .and_then(|i| table.get(i).copied().flatten())?;
    (entry.flags & FTEXT != 0).then_some(entry.text_mode)
}

/// Whether a descriptor is a device or a pipe (for `stdio.rs`).
pub(crate) fn is_device(fd: i32) -> bool {
    let table = table();
    usize::try_from(fd)
        .ok()
        .and_then(|i| table.get(i).copied().flatten())
        .is_some_and(|entry| entry.flags & (FDEV | FPIPE) != 0)
}

/// Clears a descriptor's `^Z` end (for `clearerr`).
pub(crate) fn clear_eof(fd: i32) {
    let mut table = table();
    if let Some(Some(entry)) = usize::try_from(fd).ok().and_then(|i| table.get_mut(i)) {
        entry.flags &= !FEOFLAG;
    }
}

/// Puts back a standard descriptor as it was first made (tests that move
/// the standard streams).
#[cfg(test)]
pub(crate) fn reset_standard(fd: i32) {
    let id = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE][fd as usize];
    table()[fd as usize] = Some(standard_fd(id));
}

// ---------------------------------------------------------------------------
// Raw I/O on a handle.

fn raw_read(handle: usize, buffer: &mut [u8]) -> Result<usize, u32> {
    let mut done = 0u32;
    let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    let ok = ReadFile(
        handle as *mut c_void,
        buffer.as_mut_ptr().cast(),
        len,
        &mut done,
        std::ptr::null_mut(),
    );
    match ok {
        0 if last_error() == ERROR_BROKEN_PIPE => Ok(0),
        0 => Err(last_error()),
        _ => Ok(done as usize),
    }
}

/// Writes all of `buffer`; `ENOSPC` if the handle takes nothing.
fn raw_write(handle: usize, mut buffer: &[u8]) -> Result<(), i32> {
    while !buffer.is_empty() {
        let mut done = 0u32;
        let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let ok = WriteFile(
            handle as *mut c_void,
            buffer.as_ptr().cast(),
            len,
            &mut done,
            std::ptr::null_mut(),
        );
        if ok == 0 {
            return Err(io_error(last_error()));
        }
        if done == 0 {
            _set_doserrno(0);
            return Err(error(ENOSPC));
        }
        buffer = &buffer[done as usize..];
    }
    Ok(())
}

fn raw_seek(handle: usize, distance: i64, method: u32) -> Result<i64, u32> {
    let mut position = 0i64;
    if SetFilePointerEx(handle as *mut c_void, distance, &mut position, method) == 0 {
        return Err(last_error());
    }
    Ok(position)
}

// ---------------------------------------------------------------------------
// Reading and writing.

/// Reads up to `out.len()` bytes with `\r\n` turned into `\n`, stopping at
/// a `^Z` (which ends the file for later reads) when `ctrl_z`. A `\r` the
/// read ends on is decided by the byte after it: a file is moved back to
/// it, a device keeps it for the next read.
fn read_crlf(entry: &mut Fd, out: &mut [u8], ctrl_z: bool) -> Result<usize, i32> {
    let mut raw = vec![0u8; out.len()];
    let mut n = 0;
    if let Some(c) = entry.pipe_char.take() {
        raw[0] = c;
        n = 1;
    }
    if n < raw.len() {
        n += raw_read(entry.handle, &mut raw[n..]).map_err(io_error)?;
    }
    let device = entry.flags & (FDEV | FPIPE) != 0;
    let (mut i, mut o) = (0, 0);
    while i < n {
        let b = raw[i];
        if ctrl_z && b == CTRL_Z {
            if !device {
                entry.flags |= FEOFLAG;
            }
            break;
        }
        if b != b'\r' {
            out[o] = b;
            o += 1;
            i += 1;
            continue;
        }
        if i + 1 < n {
            if raw[i + 1] == b'\n' {
                out[o] = b'\n';
                i += 2;
            } else {
                out[o] = b'\r';
                i += 1;
            }
            o += 1;
            continue;
        }
        i += 1;
        let mut peek = [0u8];
        let got = raw_read(entry.handle, &mut peek).unwrap_or(0);
        if got == 1 && peek[0] == b'\n' {
            out[o] = b'\n';
        } else {
            out[o] = b'\r';
            if got == 1 {
                if device {
                    entry.pipe_char = Some(peek[0]);
                } else {
                    let _ = raw_seek(entry.handle, -1, FILE_CURRENT);
                }
            }
        }
        o += 1;
    }
    Ok(o)
}

/// The length of a UTF-8 sequence its first byte starts.
fn utf8_length(lead: u8) -> usize {
    match lead {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// Where an incomplete UTF-8 sequence starts at the end of `bytes`, and its
/// full length.
fn incomplete_tail(bytes: &[u8]) -> Option<(usize, usize)> {
    let start = bytes
        .iter()
        .rposition(|&b| b & 0xC0 != 0x80)
        .filter(|&i| bytes.len() - i < 4)?;
    let needed = utf8_length(bytes[start]);
    (bytes.len() - start < needed).then_some((start, needed))
}

/// Writes UTF-16 units to `out` as little-endian bytes; their byte count.
fn put_units(units: &[u16], out: &mut [u8]) -> usize {
    for (k, unit) in units.iter().enumerate() {
        out[2 * k..2 * k + 2].copy_from_slice(&unit.to_le_bytes());
    }
    units.len() * 2
}

/// A `_O_U8TEXT` read: UTF-8 bytes (with `\r\n` and `^Z` handled) as
/// UTF-16 units, an incomplete last character completed from the file (or
/// left for the next read when it does not fit).
fn read_utf8(entry: &mut Fd, out: &mut [u8]) -> Result<usize, i32> {
    let capacity = out.len() / 2;
    let mut bytes = vec![0u8; capacity];
    let n = read_crlf(entry, &mut bytes, true)?;
    bytes.truncate(n);
    if let Some((start, needed)) = incomplete_tail(&bytes) {
        let mut more = vec![0u8; needed - (bytes.len() - start)];
        let got = raw_read(entry.handle, &mut more).unwrap_or(0);
        bytes.extend_from_slice(&more[..got]);
    }
    let mut units: Vec<u16> = String::from_utf8_lossy(&bytes).encode_utf16().collect();
    if units.len() > capacity {
        // The last character does not fit: it is read again next time (or,
        // alone, read as U+FFFD).
        let last = bytes.iter().rposition(|&b| b & 0xC0 != 0x80).unwrap_or(0);
        if last == 0 {
            units = vec![0xFFFD];
        } else {
            let back = (bytes.len() - last) as i64;
            if entry.flags & (FDEV | FPIPE) == 0 {
                let _ = raw_seek(entry.handle, -back, FILE_CURRENT);
            }
            units = String::from_utf8_lossy(&bytes[..last])
                .encode_utf16()
                .collect();
        }
    }
    Ok(put_units(&units, out))
}

/// A `_O_U16TEXT`/`_O_WTEXT` read: UTF-16 units with `\r\n` turned into
/// `\n`.
fn read_utf16(entry: &mut Fd, out: &mut [u8]) -> Result<usize, i32> {
    let mut raw = vec![0u8; out.len()];
    let mut n = raw_read(entry.handle, &mut raw).map_err(io_error)?;
    let device = entry.flags & (FDEV | FPIPE) != 0;
    if n % 2 == 1 {
        if !device {
            let _ = raw_seek(entry.handle, -1, FILE_CURRENT);
        }
        n -= 1;
    }
    let raw_units: Vec<u16> = raw[..n]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let mut units = Vec::with_capacity(raw_units.len());
    let mut i = 0;
    while i < raw_units.len() {
        let u = raw_units[i];
        if u != u16::from(b'\r') {
            units.push(u);
            i += 1;
            continue;
        }
        if i + 1 < raw_units.len() {
            if raw_units[i + 1] == u16::from(b'\n') {
                units.push(u16::from(b'\n'));
                i += 2;
            } else {
                units.push(u);
                i += 1;
            }
            continue;
        }
        i += 1;
        let mut peek = [0u8; 2];
        let got = raw_read(entry.handle, &mut peek).unwrap_or(0);
        if got == 2 && u16::from_le_bytes(peek) == u16::from(b'\n') {
            units.push(u16::from(b'\n'));
        } else {
            units.push(u);
            if got > 0 && !device {
                let _ = raw_seek(entry.handle, -(got as i64), FILE_CURRENT);
            }
        }
    }
    Ok(put_units(&units, out))
}

/// `_read`'s body, the descriptor locked.
fn read_fd(entry: &mut Fd, out: &mut [u8]) -> Result<usize, i32> {
    if out.is_empty() || entry.flags & FEOFLAG != 0 {
        return Ok(0);
    }
    if entry.flags & FTEXT == 0 {
        return raw_read(entry.handle, out).map_err(io_error);
    }
    match entry.text_mode {
        TextMode::Ansi => read_crlf(entry, out, true),
        TextMode::Utf8 => read_utf8(entry, out),
        TextMode::Utf16 => read_utf16(entry, out),
    }
}

/// `text` with `\n` turned into `\r\n`, as UTF-8.
fn crlf_utf8(units: &[u16]) -> Vec<u8> {
    String::from_utf16_lossy(units)
        .replace('\n', "\r\n")
        .into_bytes()
}

/// `_write`'s body, the descriptor locked: the bytes of `data` written.
fn write_fd(entry: &mut Fd, data: &[u8]) -> Result<usize, i32> {
    if entry.flags & FAPPEND != 0 {
        raw_seek(entry.handle, 0, FILE_END).map_err(dos_map_err)?;
    }
    if entry.flags & FTEXT == 0 {
        raw_write(entry.handle, data)?;
        return Ok(data.len());
    }
    let device = entry.flags & (FDEV | FPIPE) != 0;
    let out: Vec<u8> = match entry.text_mode {
        TextMode::Ansi => {
            let mut out = Vec::with_capacity(data.len() + data.len() / 8);
            for &b in data {
                if b == b'\n' {
                    out.push(b'\r');
                }
                out.push(b);
            }
            out
        }
        mode => {
            let units: Vec<u16> = data
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            if mode == TextMode::Utf8 || device {
                crlf_utf8(&units)
            } else {
                let mut out = Vec::with_capacity(data.len() + data.len() / 8);
                for &u in &units {
                    if u == u16::from(b'\n') {
                        out.extend_from_slice(&u16::from(b'\r').to_le_bytes());
                    }
                    out.extend_from_slice(&u.to_le_bytes());
                }
                out
            }
        }
    };
    raw_write(entry.handle, &out)?;
    Ok(data.len())
}

/// Reads from a descriptor (text mode: translated); the bytes read, 0 at
/// the end, -1 on failure.
pub extern "win64" fn _read(fd: i32, buffer: *mut c_void, count: u32) -> i32 {
    let entry = match fd_copy(fd) {
        Ok(entry) => entry,
        Err(_) => return -1,
    };
    if count == 0 {
        return 0;
    }
    if buffer.is_null() || count > i32::MAX as u32 {
        invalid(EINVAL);
        return -1;
    }
    if entry.flags & FTEXT != 0 && entry.text_mode != TextMode::Ansi && count % 2 == 1 {
        invalid(EINVAL);
        return -1;
    }
    // SAFETY: the title's buffer of `count` bytes.
    let out = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), count as usize) };
    or(
        with_fd(fd, |entry| read_fd(entry, out)).map(|n| n as i32),
        -1,
    )
}

/// Writes to a descriptor (text mode: translated); the bytes of `buffer`
/// written, -1 on failure.
pub extern "win64" fn _write(fd: i32, buffer: *const c_void, count: u32) -> i32 {
    let entry = match fd_copy(fd) {
        Ok(entry) => entry,
        Err(_) => return -1,
    };
    if count == 0 {
        return 0;
    }
    if buffer.is_null() || count > i32::MAX as u32 {
        invalid(EINVAL);
        return -1;
    }
    if entry.flags & FTEXT != 0 && entry.text_mode != TextMode::Ansi && count % 2 == 1 {
        invalid(EINVAL);
        return -1;
    }
    // SAFETY: the title's buffer of `count` bytes.
    let data = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), count as usize) };
    or(
        with_fd(fd, |entry| write_fd(entry, data)).map(|n| n as i32),
        -1,
    )
}

/// Moves a descriptor's position; clears its `^Z` end.
pub(crate) fn seek(fd: i32, offset: i64, origin: i32) -> Result<i64, i32> {
    with_fd(fd, |entry| {
        let method = u32::try_from(origin).unwrap_or(u32::MAX);
        let position = raw_seek(entry.handle, offset, method).map_err(dos_map_err)?;
        entry.flags &= !FEOFLAG;
        entry.pipe_char = None;
        Ok(position)
    })
}

/// See [`seek`]; a position past 2^31 - 1 is `EINVAL`.
pub extern "win64" fn _lseek(fd: i32, offset: i32, origin: i32) -> i32 {
    match seek(fd, i64::from(offset), origin) {
        Ok(position) => i32::try_from(position).unwrap_or_else(|_| {
            error(EINVAL);
            -1
        }),
        Err(_) => -1,
    }
}

/// See [`seek`].
pub extern "win64" fn _lseeki64(fd: i32, offset: i64, origin: i32) -> i64 {
    or(seek(fd, offset, origin), -1)
}

/// The descriptor's position.
pub extern "win64" fn _tell(fd: i32) -> i32 {
    _lseek(fd, 0, 1)
}

/// See [`_tell`].
pub extern "win64" fn _telli64(fd: i32) -> i64 {
    _lseeki64(fd, 0, 1)
}

/// Writes the file's data to disk (`FlushFileBuffers`); a failure is
/// `EBADF` with the Win32 error in `_doserrno`, as the UCRT has it.
pub extern "win64" fn _commit(fd: i32) -> i32 {
    let result = with_fd(fd, |entry| {
        if FlushFileBuffers(entry.handle as *mut c_void) == 0 {
            _set_doserrno(last_error());
            return Err(error(EBADF));
        }
        Ok(0)
    });
    or(result, -1)
}

/// `_chsize_s`'s body: the file ends at `size`, the position kept.
fn change_size(fd: i32, size: i64) -> Result<(), i32> {
    if size < 0 {
        return Err(invalid(EINVAL));
    }
    with_fd(fd, |entry| {
        let h = entry.handle;
        let here = raw_seek(h, 0, FILE_CURRENT).map_err(dos_map_err)?;
        raw_seek(h, size, FILE_BEGIN).map_err(dos_map_err)?;
        let ended = SetEndOfFile(h as *mut c_void) != 0;
        let code = last_error();
        let _ = raw_seek(h, here, FILE_BEGIN);
        if !ended {
            return Err(dos_map_err(code));
        }
        Ok(())
    })
}

/// Changes the file's size (zeros added when it grows); 0 or -1.
pub extern "win64" fn _chsize(fd: i32, size: i32) -> i32 {
    match change_size(fd, i64::from(size)) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// See [`_chsize`]; 0 or the `errno` value.
pub extern "win64" fn _chsize_s(fd: i32, size: i64) -> i32 {
    match change_size(fd, size) {
        Ok(()) => 0,
        Err(code) => code,
    }
}

/// The file's length, the position kept.
fn length(fd: i32) -> Result<i64, i32> {
    with_fd(fd, |entry| {
        let h = entry.handle;
        let here = raw_seek(h, 0, FILE_CURRENT).map_err(dos_map_err)?;
        let end = raw_seek(h, 0, FILE_END).map_err(dos_map_err)?;
        let _ = raw_seek(h, here, FILE_BEGIN);
        Ok(end)
    })
}

/// The file's length; one past 2^31 - 1 is `EFBIG`.
pub extern "win64" fn _filelength(fd: i32) -> i32 {
    match length(fd) {
        Ok(n) => i32::try_from(n).unwrap_or_else(|_| {
            error(EFBIG);
            -1
        }),
        Err(_) => -1,
    }
}

/// The file's length.
pub extern "win64" fn _filelengthi64(fd: i32) -> i64 {
    or(length(fd), -1)
}

/// 1 at the end of the file, 0 before it, -1 on failure.
pub extern "win64" fn _eof(fd: i32) -> i32 {
    let result = with_fd(fd, |entry| {
        let h = entry.handle;
        let here = raw_seek(h, 0, FILE_CURRENT).map_err(dos_map_err)?;
        let end = raw_seek(h, 0, FILE_END).map_err(dos_map_err)?;
        let _ = raw_seek(h, here, FILE_BEGIN);
        Ok(i32::from(here >= end))
    });
    or(result, -1)
}

/// Non-zero (`FDEV`) for a character device; 0 otherwise, and for a bad
/// descriptor (`EBADF`).
pub extern "win64" fn _isatty(fd: i32) -> i32 {
    or(with_fd(fd, |entry| Ok(i32::from(entry.flags & FDEV))), 0)
}

/// Sets a descriptor's translation mode; the previous one (`_O_TEXT`,
/// `_O_BINARY`, or `_O_WTEXT` for any Unicode mode, as the UCRT answers).
pub extern "win64" fn _setmode(fd: i32, mode: i32) -> i32 {
    if fd_copy(fd).is_err() {
        return -1;
    }
    if ![O_TEXT, O_BINARY, O_WTEXT, O_U16TEXT, O_U8TEXT].contains(&mode) {
        invalid(EINVAL);
        return -1;
    }
    let result = with_fd(fd, |entry| {
        let previous = if entry.flags & FTEXT == 0 {
            O_BINARY
        } else if entry.text_mode == TextMode::Ansi {
            O_TEXT
        } else {
            O_WTEXT
        };
        if mode == O_BINARY {
            entry.flags &= !FTEXT;
        } else {
            entry.flags |= FTEXT;
            entry.text_mode = match mode {
                O_TEXT => TextMode::Ansi,
                O_U8TEXT => TextMode::Utf8,
                _ => TextMode::Utf16,
            };
        }
        Ok(previous)
    });
    or(result, -1)
}

/// A descriptor's Win32 handle; -1 for a bad one.
pub extern "win64" fn _get_osfhandle(fd: i32) -> isize {
    or(fd_copy(fd).map(|entry| entry.handle as isize), -1)
}

/// The flags a handle's file type gives: `FDEV` or `FPIPE`; `Err` for an
/// unknown type.
fn type_flags(handle: usize) -> Result<u8, i32> {
    match GetFileType(handle as *mut c_void) {
        FILE_TYPE_UNKNOWN => {
            let code = last_error();
            if code == 0 {
                _set_doserrno(0);
                return Err(error(EBADF));
            }
            Err(dos_map_err(code))
        }
        FILE_TYPE_CHAR => Ok(FDEV),
        FILE_TYPE_PIPE => Ok(FPIPE),
        _ => Ok(0),
    }
}

/// A new descriptor over a Win32 handle; `flags` takes `_O_APPEND`,
/// `_O_NOINHERIT` and the text modes (binary without one).
pub extern "win64" fn _open_osfhandle(handle: isize, flags: i32) -> i32 {
    let handle = handle as usize;
    let kind = match type_flags(handle) {
        Ok(kind) => kind,
        Err(_) => return -1,
    };
    let mut fd_flags = FOPEN | kind;
    if flags & O_APPEND != 0 {
        fd_flags |= FAPPEND;
    }
    if flags & O_NOINHERIT != 0 {
        fd_flags |= FNOINHERIT;
    }
    if flags & (O_TEXT | O_UNICODE) != 0 {
        fd_flags |= FTEXT;
    }
    let entry = Fd {
        handle,
        flags: fd_flags,
        text_mode: unicode_mode(flags).unwrap_or(TextMode::Ansi),
        pipe_char: None,
    };
    or(allocate(entry), -1)
}

/// The text mode a Unicode flag asks for.
fn unicode_mode(flags: i32) -> Option<TextMode> {
    if flags & O_U8TEXT != 0 {
        Some(TextMode::Utf8)
    } else if flags & (O_U16TEXT | O_WTEXT) != 0 {
        Some(TextMode::Utf16)
    } else {
        None
    }
}

/// A duplicate of a descriptor's handle, inheritable.
fn duplicate(handle: usize) -> Result<usize, i32> {
    let mut copy: *mut c_void = std::ptr::null_mut();
    let process = GetCurrentProcess();
    let ok = DuplicateHandle(
        process,
        handle as *mut c_void,
        process,
        &mut copy,
        0,
        1,
        DUPLICATE_SAME_ACCESS,
    );
    if ok == 0 {
        return Err(os_error());
    }
    Ok(copy as usize)
}

/// Closes a descriptor's handle unless it is a standard one.
fn close_handle(handle: usize) -> Result<(), i32> {
    if !is_standard_handle(handle) && CloseHandle(handle as *mut c_void) == 0 {
        return Err(os_error());
    }
    Ok(())
}

/// A new descriptor naming the same file (and position) as `fd`.
pub extern "win64" fn _dup(fd: i32) -> i32 {
    let result = fd_copy(fd).and_then(|entry| {
        let handle = duplicate(entry.handle)?;
        let copy = Fd {
            handle,
            flags: entry.flags & !FNOINHERIT,
            ..entry
        };
        allocate(copy).inspect_err(|_| {
            let _ = CloseHandle(handle as *mut c_void);
        })
    });
    or(result, -1)
}

/// Makes `to` name the same file as `from`, closing what `to` named; 0.
pub extern "win64" fn _dup2(from: i32, to: i32) -> i32 {
    let Ok(target) = usize::try_from(to) else {
        bad_fd();
        return -1;
    };
    if target >= MAX_DESCRIPTORS {
        bad_fd();
        return -1;
    }
    let entry = match fd_copy(from) {
        Ok(entry) => entry,
        Err(_) => return -1,
    };
    if from == to {
        return 0;
    }
    let handle = match duplicate(entry.handle) {
        Ok(handle) => handle,
        Err(_) => return -1,
    };
    let replaced = {
        let mut table = table();
        while table.len() <= target {
            table.push(None);
        }
        table[target].replace(Fd {
            handle,
            flags: entry.flags & !FNOINHERIT,
            ..entry
        })
    };
    if let Some(old) = replaced {
        let _ = close_handle(old.handle);
    }
    0
}

/// Closes a descriptor.
pub extern "win64" fn _close(fd: i32) -> i32 {
    let taken = {
        let mut table = table();
        usize::try_from(fd)
            .ok()
            .and_then(|i| table.get_mut(i))
            .and_then(Option::take)
    };
    let Some(entry) = taken else {
        bad_fd();
        return -1;
    };
    match close_handle(entry.handle) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Locks nothing: 0 for an open descriptor.
pub extern "win64" fn _locking(fd: i32, _mode: i32, _bytes: i32) -> i32 {
    or(fd_copy(fd).map(|_| 0), -1)
}

/// Pipes are not made here: -1 and `EINVAL`.
pub extern "win64" fn _pipe(_fds: *mut i32, _size: u32, _mode: i32) -> i32 {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!("the title asked for a pipe (_pipe), which is not supported");
    }
    error(EINVAL);
    -1
}

/// The permission bits a created file does not get.
static UMASK: AtomicI32 = AtomicI32::new(0);

/// Sets the mask of permission bits (`_S_IREAD`, `_S_IWRITE`); the
/// previous one.
pub extern "win64" fn _umask(mode: i32) -> i32 {
    UMASK.swap(mode & (S_IREAD | S_IWRITE), Ordering::Relaxed)
}

/// See [`_umask`], the previous mask to `*old`; `EINVAL` for other bits or
/// a null `old`.
pub extern "win64" fn _umask_s(mode: i32, old: *mut i32) -> i32 {
    if old.is_null() || mode & !(S_IREAD | S_IWRITE) != 0 {
        return invalid(EINVAL);
    }
    // SAFETY: the title's place for the mask.
    unsafe { old.write(_umask(mode)) };
    0
}

// ---------------------------------------------------------------------------
// Opening.

/// A title's narrow path as a NUL-terminated UTF-16 string (read as UTF-8).
fn narrow_path(path: *const c_char) -> Option<Vec<u16>> {
    if path.is_null() {
        return None;
    }
    // SAFETY: the title's NUL-terminated string.
    let bytes = unsafe { narrow_bytes(path) };
    Some(
        String::from_utf8_lossy(bytes)
            .encode_utf16()
            .chain([0])
            .collect(),
    )
}

/// A title's wide path, NUL-terminated.
fn wide_path(path: *const u16) -> Option<Vec<u16>> {
    if path.is_null() {
        return None;
    }
    // SAFETY: the title's NUL-terminated string.
    Some(
        unsafe { wide_units(path) }
            .iter()
            .copied()
            .chain([0])
            .collect(),
    )
}

/// Opens `path` (NUL-terminated UTF-16) as `_wsopen_s` does: the new
/// descriptor, or the `errno` value (set).
pub(crate) fn open(
    path: &[u16],
    oflag: i32,
    shflag: i32,
    pmode: i32,
    secure: bool,
) -> Result<i32, i32> {
    let access = match oflag & (O_RDONLY | O_WRONLY | O_RDWR) {
        O_RDONLY => GENERIC_READ,
        O_WRONLY => GENERIC_WRITE,
        O_RDWR => GENERIC_READ | GENERIC_WRITE,
        _ => return Err(invalid(EINVAL)),
    };
    let share = match shflag {
        SH_DENYRW => 0,
        SH_DENYWR => FILE_SHARE_READ,
        SH_DENYRD => FILE_SHARE_WRITE,
        SH_DENYNO => FILE_SHARE_READ | FILE_SHARE_WRITE,
        _ => return Err(invalid(EINVAL)),
    };
    if secure && pmode & !(S_IREAD | S_IWRITE) != 0 {
        return Err(invalid(EINVAL));
    }
    let bit = |b: i32| oflag & b != 0;
    let disposition = match (bit(O_CREAT), bit(O_EXCL), bit(O_TRUNC)) {
        (true, true, _) => CREATE_NEW,
        (true, false, true) => CREATE_ALWAYS,
        (true, false, false) => OPEN_ALWAYS,
        (false, _, true) => TRUNCATE_EXISTING,
        (false, _, false) => OPEN_EXISTING,
    };
    let mut flags = 0;
    if oflag & O_TEMPORARY != 0 {
        flags |= FILE_FLAG_DELETE_ON_CLOSE;
    }
    if oflag & O_OBTAIN_DIR != 0 {
        flags |= FILE_FLAG_BACKUP_SEMANTICS;
    }
    let text = if oflag & O_BINARY != 0 {
        false
    } else if oflag & (O_TEXT | O_UNICODE) != 0 {
        true
    } else {
        let mut fmode = 0;
        _get_fmode(&mut fmode);
        fmode & O_BINARY == 0
    };
    let unicode = text && oflag & O_UNICODE != 0;
    // A write-only Unicode file is read too, for its byte order mark.
    let extra_read = unicode && oflag & (O_WRONLY | O_RDWR) == O_WRONLY;
    let create = |access: u32| {
        let null = std::ptr::null_mut();
        let handle = CreateFileW(path.as_ptr(), access, share, null, disposition, flags, null);
        (handle as usize, last_error())
    };
    let (mut handle, mut code) = create(access | if extra_read { GENERIC_READ } else { 0 });
    let mut readable = access & GENERIC_READ != 0 || extra_read;
    if handle == INVALID_HANDLE && extra_read {
        (handle, code) = create(access);
        readable = false;
    }
    if handle == INVALID_HANDLE {
        return Err(dos_map_err(code));
    }
    let created = disposition == CREATE_NEW
        || (matches!(disposition, CREATE_ALWAYS | OPEN_ALWAYS) && code != ERROR_ALREADY_EXISTS);
    let kind = match type_flags(handle) {
        Ok(kind) => kind,
        Err(e) => {
            let _ = CloseHandle(handle as *mut c_void);
            return Err(e);
        }
    };
    if oflag & O_CREAT != 0 && created && (pmode & !UMASK.load(Ordering::Relaxed)) & S_IWRITE == 0 {
        SetFileAttributesW(path.as_ptr(), FILE_ATTRIBUTE_READONLY);
    }
    let mut fd_flags = FOPEN | kind;
    if oflag & O_APPEND != 0 {
        fd_flags |= FAPPEND;
    }
    if oflag & O_NOINHERIT != 0 {
        fd_flags |= FNOINHERIT;
    }
    if text {
        fd_flags |= FTEXT;
    }
    let mut text_mode = TextMode::Ansi;
    if text && kind == 0 {
        let writable = access & GENERIC_WRITE != 0;
        match prepare_text(handle, oflag, readable, writable) {
            Ok(mode) => text_mode = mode,
            Err(e) => {
                let _ = CloseHandle(handle as *mut c_void);
                return Err(e);
            }
        }
    } else if unicode {
        text_mode = unicode_mode(oflag).unwrap_or(TextMode::Utf16);
    }
    let entry = Fd {
        handle,
        flags: fd_flags,
        text_mode,
        pipe_char: None,
    };
    allocate(entry).inspect_err(|_| {
        let _ = CloseHandle(handle as *mut c_void);
    })
}

/// A text file just opened, as the UCRT prepares it: a read-write one
/// loses a last `^Z`; a Unicode one gets its mode from its byte order mark
/// (moving past it), or, empty and writable, gets the mark its mode has.
fn prepare_text(
    handle: usize,
    oflag: i32,
    readable: bool,
    writable: bool,
) -> Result<TextMode, i32> {
    let os = |code: u32| dos_map_err(code);
    if oflag & O_RDWR != 0 {
        let size = raw_seek(handle, 0, FILE_END).map_err(os)?;
        if size > 0 {
            raw_seek(handle, -1, FILE_END).map_err(os)?;
            let mut last = [0u8];
            if raw_read(handle, &mut last).map_err(os)? == 1 && last[0] == CTRL_Z {
                raw_seek(handle, -1, FILE_END).map_err(os)?;
                SetEndOfFile(handle as *mut c_void);
            }
        }
        raw_seek(handle, 0, FILE_BEGIN).map_err(os)?;
    }
    let Some(mut mode) = unicode_mode(oflag) else {
        return Ok(TextMode::Ansi);
    };
    let size = raw_seek(handle, 0, FILE_END).map_err(os)?;
    raw_seek(handle, 0, FILE_BEGIN).map_err(os)?;
    if readable && size > 0 {
        let mut bom = [0u8; 3];
        let n = raw_read(handle, &mut bom).map_err(os)?;
        let skip = if n == 3 && bom == [0xEF, 0xBB, 0xBF] {
            mode = TextMode::Utf8;
            3
        } else if n >= 2 && bom[..2] == [0xFF, 0xFE] {
            mode = TextMode::Utf16;
            2
        } else if n >= 2 && bom[..2] == [0xFE, 0xFF] {
            // UTF-16 big-endian is not supported, as in the UCRT.
            return Err(error(EINVAL));
        } else {
            0
        };
        raw_seek(handle, skip, FILE_BEGIN).map_err(os)?;
    }
    if writable && size == 0 {
        let bom: &[u8] = match mode {
            TextMode::Utf8 => &[0xEF, 0xBB, 0xBF],
            _ => &[0xFF, 0xFE],
        };
        raw_write(handle, bom)?;
    }
    Ok(mode)
}

/// `_open`'s body for a converted path; -1 on failure.
fn open_or(path: Option<Vec<u16>>, oflag: i32, shflag: i32, pmode: i32) -> i32 {
    let Some(path) = path else {
        invalid(EINVAL);
        return -1;
    };
    or(open(&path, oflag, shflag, pmode, false), -1)
}

/// Opens a file; `pmode` (the variadic argument) is read only with
/// `_O_CREAT`.
pub extern "win64" fn _open(path: *const c_char, oflag: i32, pmode: i32) -> i32 {
    open_or(narrow_path(path), oflag, SH_DENYNO, pmode)
}

/// See [`_open`].
pub extern "win64" fn _wopen(path: *const u16, oflag: i32, pmode: i32) -> i32 {
    open_or(wide_path(path), oflag, SH_DENYNO, pmode)
}

/// See [`_open`], with a sharing mode.
pub extern "win64" fn _sopen(path: *const c_char, oflag: i32, shflag: i32, pmode: i32) -> i32 {
    open_or(narrow_path(path), oflag, shflag, pmode)
}

/// See [`_sopen`].
pub extern "win64" fn _wsopen(path: *const u16, oflag: i32, shflag: i32, pmode: i32) -> i32 {
    open_or(wide_path(path), oflag, shflag, pmode)
}

/// `_sopen_dispatch`'s body: the descriptor to `*fd` (-1 on failure); 0 or
/// the `errno` value.
fn open_into(
    path: Option<Vec<u16>>,
    oflag: i32,
    shflag: i32,
    pmode: i32,
    fd: *mut i32,
    secure: bool,
) -> i32 {
    if fd.is_null() {
        return invalid(EINVAL);
    }
    // SAFETY: the title's place for the descriptor.
    unsafe { fd.write(-1) };
    let Some(path) = path else {
        return invalid(EINVAL);
    };
    match open(&path, oflag, shflag, pmode, secure) {
        Ok(n) => {
            // SAFETY: as above.
            unsafe { fd.write(n) };
            0
        }
        Err(code) => code,
    }
}

/// See [`_sopen`]: the descriptor to `*fd`; 0 or the `errno` value.
pub extern "win64" fn _sopen_s(
    fd: *mut i32,
    path: *const c_char,
    oflag: i32,
    shflag: i32,
    pmode: i32,
) -> i32 {
    open_into(narrow_path(path), oflag, shflag, pmode, fd, true)
}

/// See [`_sopen_s`].
pub extern "win64" fn _wsopen_s(
    fd: *mut i32,
    path: *const u16,
    oflag: i32,
    shflag: i32,
    pmode: i32,
) -> i32 {
    open_into(wide_path(path), oflag, shflag, pmode, fd, true)
}

/// What the headers' inline `_open` calls: see [`_sopen_s`] (`secure`
/// checks the permission bits).
pub extern "win64" fn _sopen_dispatch(
    path: *const c_char,
    oflag: i32,
    shflag: i32,
    pmode: i32,
    fd: *mut i32,
    secure: i32,
) -> i32 {
    open_into(narrow_path(path), oflag, shflag, pmode, fd, secure != 0)
}

/// See [`_sopen_dispatch`].
pub extern "win64" fn _wsopen_dispatch(
    path: *const u16,
    oflag: i32,
    shflag: i32,
    pmode: i32,
    fd: *mut i32,
    secure: i32,
) -> i32 {
    open_into(wide_path(path), oflag, shflag, pmode, fd, secure != 0)
}

// ---------------------------------------------------------------------------
// File status.

/// `time_t` seconds of a FILETIME; -1 for none (0).
fn time_of(time: FILETIME) -> i64 {
    let value = (u64::from(time.high) << 32 | u64::from(time.low)) as i64;
    if value == 0 {
        return -1;
    }
    (value - EPOCH_DIFFERENCE).div_euclid(10_000_000)
}

/// What every `stat` structure holds.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct StatInfo {
    dev: u32,
    mode: u16,
    nlink: i16,
    size: i64,
    atime: i64,
    mtime: i64,
    ctime: i64,
}

/// A 64-bit value truncated to a narrower field, as the 32-bit structures
/// take it.
trait Narrowed {
    fn narrowed(value: i64) -> Self;
}

impl Narrowed for i64 {
    fn narrowed(value: i64) -> Self {
        value
    }
}

impl Narrowed for i32 {
    fn narrowed(value: i64) -> Self {
        value as i32
    }
}

impl Narrowed for u32 {
    fn narrowed(value: i64) -> Self {
        value as u32
    }
}

macro_rules! stat_struct {
    ($(#[$doc:meta])* $name:ident, $size:ty, $time:ty, $bytes:expr) => {
        $(#[$doc])*
        #[repr(C)]
        #[derive(Debug, Default, Clone, Copy)]
        pub struct $name {
            pub st_dev: u32,
            pub st_ino: u16,
            pub st_mode: u16,
            pub st_nlink: i16,
            pub st_uid: i16,
            pub st_gid: i16,
            pub st_rdev: u32,
            pub st_size: $size,
            pub st_atime: $time,
            pub st_mtime: $time,
            pub st_ctime: $time,
        }

        const _: () = assert!(std::mem::size_of::<$name>() == $bytes);

        impl From<&StatInfo> for $name {
            fn from(info: &StatInfo) -> Self {
                $name {
                    st_dev: info.dev,
                    st_ino: 0,
                    st_mode: info.mode,
                    st_nlink: info.nlink,
                    st_uid: 0,
                    st_gid: 0,
                    st_rdev: info.dev,
                    st_size: <$size>::narrowed(info.size),
                    st_atime: <$time>::narrowed(info.atime),
                    st_mtime: <$time>::narrowed(info.mtime),
                    st_ctime: <$time>::narrowed(info.ctime),
                }
            }
        }
    };
}

stat_struct!(
    /// `struct _stat64` (56 bytes: 4 of padding after `st_rdev`).
    Stat64, i64, i64, 56
);
stat_struct!(
    /// `struct _stat64i32`.
    Stat64i32, i32, i64, 48
);
stat_struct!(
    /// `struct _stat32`.
    Stat32, i32, i32, 36
);
stat_struct!(
    /// `struct _stat32i64`.
    Stat32i64, i64, i32, 48
);

/// The mode bits of a path's attributes, as the UCRT makes them: read
/// always, write unless read-only, a directory (or a root) executable, an
/// `.exe`, `.cmd`, `.bat` or `.com` file executable; each replicated into
/// the group and other bits.
fn stat_mode(attributes: u32, path: &[u16]) -> u16 {
    let mut mode = if attributes & FILE_ATTRIBUTE_READONLY != 0 {
        S_IREAD
    } else {
        S_IREAD | S_IWRITE
    };
    let text = String::from_utf16_lossy(path).to_ascii_lowercase();
    let mut kind = S_IFREG;
    if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        kind = S_IFDIR;
        mode |= S_IEXEC;
    }
    if [".exe", ".cmd", ".bat", ".com"]
        .iter()
        .any(|e| text.ends_with(e))
    {
        mode |= S_IEXEC;
    }
    mode |= (mode & 0x1C0) >> 3;
    mode |= (mode & 0x1C0) >> 6;
    kind | mode as u16
}

/// A path's status, through `GetFileAttributesExW`.
fn stat_path(path: &[u16]) -> Result<StatInfo, i32> {
    let text = &path[..path.len() - 1];
    if text
        .iter()
        .any(|&c| c == u16::from(b'*') || c == u16::from(b'?'))
    {
        _set_doserrno(ERROR_FILE_NOT_FOUND);
        return Err(error(ENOENT));
    }
    let mut data = WIN32_FILE_ATTRIBUTE_DATA::default();
    if GetFileAttributesExW(path.as_ptr(), 0, &mut data) == 0 {
        return Err(os_error());
    }
    let mut full = vec![0u16; 32768];
    let n = GetFullPathNameW(
        path.as_ptr(),
        full.len() as u32,
        full.as_mut_ptr(),
        std::ptr::null_mut(),
    );
    let letter = u8::try_from(full[0]).unwrap_or(0).to_ascii_uppercase();
    let dev = if n >= 2 && full[1] == u16::from(b':') && letter.is_ascii_uppercase() {
        u32::from(letter - b'A')
    } else {
        0
    };
    Ok(StatInfo {
        dev,
        mode: stat_mode(data.attributes, text),
        nlink: 1,
        size: (u64::from(data.size_high) << 32 | u64::from(data.size_low)) as i64,
        atime: time_of(data.last_access),
        mtime: time_of(data.last_write),
        ctime: time_of(data.creation),
    })
}

/// A descriptor's status: a device or pipe from its kind, a file from
/// `GetFileInformationByHandle`.
fn stat_fd(fd: i32) -> Result<StatInfo, i32> {
    let entry = fd_copy(fd)?;
    if entry.flags & (FDEV | FPIPE) != 0 {
        let kind = if entry.flags & FDEV != 0 {
            S_IFCHR
        } else {
            S_IFIFO
        };
        return Ok(StatInfo {
            dev: fd as u32,
            mode: kind,
            nlink: 1,
            ..StatInfo::default()
        });
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if GetFileInformationByHandle(entry.handle as *mut c_void, &mut info) == 0 {
        return Err(os_error());
    }
    let mut mode = if info.attributes & FILE_ATTRIBUTE_READONLY != 0 {
        S_IREAD
    } else {
        S_IREAD | S_IWRITE
    };
    mode |= (mode & 0x1C0) >> 3;
    mode |= (mode & 0x1C0) >> 6;
    let kind = if info.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        S_IFDIR
    } else {
        S_IFREG
    };
    Ok(StatInfo {
        dev: 0,
        mode: kind | mode as u16,
        nlink: i16::try_from(info.links).unwrap_or(i16::MAX),
        size: (u64::from(info.size_high) << 32 | u64::from(info.size_low)) as i64,
        atime: time_of(info.last_access),
        mtime: time_of(info.last_write),
        ctime: time_of(info.creation),
    })
}

/// Writes a status to the title's structure; 0 or -1.
fn put_stat<T: for<'a> From<&'a StatInfo>>(info: Result<StatInfo, i32>, out: *mut T) -> i32 {
    match info {
        Ok(info) => {
            // SAFETY: the title's structure, checked non-null by the callers.
            unsafe { out.write_unaligned(T::from(&info)) };
            0
        }
        Err(_) => -1,
    }
}

/// `_stat`'s body.
fn stat_into<T: for<'a> From<&'a StatInfo>>(path: Option<Vec<u16>>, out: *mut T) -> i32 {
    let (Some(path), false) = (path, out.is_null()) else {
        invalid(EINVAL);
        return -1;
    };
    put_stat(stat_path(&path), out)
}

/// `_fstat`'s body.
fn fstat_into<T: for<'a> From<&'a StatInfo>>(fd: i32, out: *mut T) -> i32 {
    if out.is_null() {
        invalid(EINVAL);
        return -1;
    }
    put_stat(stat_fd(fd), out)
}

macro_rules! stat_functions {
    ($ty:ident, $stat:ident, $wstat:ident, $fstat:ident) => {
        /// A path's status (mode, drive, size, times).
        pub extern "win64" fn $stat(path: *const c_char, out: *mut $ty) -> i32 {
            stat_into(narrow_path(path), out)
        }

        /// A path's status (mode, drive, size, times).
        pub extern "win64" fn $wstat(path: *const u16, out: *mut $ty) -> i32 {
            stat_into(wide_path(path), out)
        }

        /// A descriptor's status.
        pub extern "win64" fn $fstat(fd: i32, out: *mut $ty) -> i32 {
            fstat_into(fd, out)
        }
    };
}

stat_functions!(Stat64, _stat64, _wstat64, _fstat64);
stat_functions!(Stat64i32, _stat64i32, _wstat64i32, _fstat64i32);
stat_functions!(Stat32, _stat32, _wstat32, _fstat32);
stat_functions!(Stat32i64, _stat32i64, _wstat32i64, _fstat32i64);

// ---------------------------------------------------------------------------
// Paths.

/// `_access_s`'s body: 0 or the `errno` value.
fn access(path: Option<Vec<u16>>, mode: i32) -> i32 {
    let Some(path) = path else {
        return invalid(EINVAL);
    };
    if mode & !6 != 0 {
        return invalid(EINVAL);
    }
    let attributes = GetFileAttributesW(path.as_ptr());
    if attributes == INVALID_FILE_ATTRIBUTES {
        return os_error();
    }
    if mode & 2 != 0 && attributes & FILE_ATTRIBUTE_READONLY != 0 {
        _set_doserrno(ERROR_ACCESS_DENIED);
        return error(EACCES);
    }
    0
}

/// Whether a file exists (0) and may be read (4), written (2) or both (6);
/// 0 or -1.
pub extern "win64" fn _access(path: *const c_char, mode: i32) -> i32 {
    if access(narrow_path(path), mode) == 0 {
        0
    } else {
        -1
    }
}

/// See [`_access`].
pub extern "win64" fn _waccess(path: *const u16, mode: i32) -> i32 {
    if access(wide_path(path), mode) == 0 {
        0
    } else {
        -1
    }
}

/// See [`_access`]; 0 or the `errno` value.
pub extern "win64" fn _access_s(path: *const c_char, mode: i32) -> i32 {
    access(narrow_path(path), mode)
}

/// See [`_access_s`].
pub extern "win64" fn _waccess_s(path: *const u16, mode: i32) -> i32 {
    access(wide_path(path), mode)
}

/// Runs a Win32 path call: 0, or -1 with its error as `errno`; a null path
/// is an invalid parameter.
fn path_call(path: Option<Vec<u16>>, call: impl FnOnce(*const u16) -> i32) -> i32 {
    let Some(path) = path else {
        invalid(EINVAL);
        return -1;
    };
    if call(path.as_ptr()) == 0 {
        os_error();
        return -1;
    }
    0
}

/// Deletes a file.
pub extern "win64" fn _unlink(path: *const c_char) -> i32 {
    path_call(narrow_path(path), |p| DeleteFileW(p))
}

/// See [`_unlink`].
pub extern "win64" fn _wunlink(path: *const u16) -> i32 {
    path_call(wide_path(path), |p| DeleteFileW(p))
}

/// See [`_unlink`].
pub extern "win64" fn remove(path: *const c_char) -> i32 {
    path_call(narrow_path(path), |p| DeleteFileW(p))
}

/// See [`_unlink`].
pub extern "win64" fn _wremove(path: *const u16) -> i32 {
    path_call(wide_path(path), |p| DeleteFileW(p))
}

/// `rename`'s body: `MoveFileExW` with `MOVEFILE_COPY_ALLOWED`, as the
/// UCRT's.
fn rename_paths(from: Option<Vec<u16>>, to: Option<Vec<u16>>) -> i32 {
    let (Some(from), Some(to)) = (from, to) else {
        invalid(EINVAL);
        return -1;
    };
    if MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_COPY_ALLOWED) == 0 {
        os_error();
        return -1;
    }
    0
}

/// Renames a file or directory; an existing target is `EEXIST`.
pub extern "win64" fn rename(from: *const c_char, to: *const c_char) -> i32 {
    rename_paths(narrow_path(from), narrow_path(to))
}

/// See [`rename`].
pub extern "win64" fn _wrename(from: *const u16, to: *const u16) -> i32 {
    rename_paths(wide_path(from), wide_path(to))
}

/// Creates a directory.
pub extern "win64" fn _mkdir(path: *const c_char) -> i32 {
    path_call(narrow_path(path), |p| CreateDirectoryW(p, std::ptr::null()))
}

/// See [`_mkdir`].
pub extern "win64" fn _wmkdir(path: *const u16) -> i32 {
    path_call(wide_path(path), |p| CreateDirectoryW(p, std::ptr::null()))
}

/// Removes an empty directory.
pub extern "win64" fn _rmdir(path: *const c_char) -> i32 {
    path_call(narrow_path(path), |p| RemoveDirectoryW(p))
}

/// See [`_rmdir`].
pub extern "win64" fn _wrmdir(path: *const u16) -> i32 {
    path_call(wide_path(path), |p| RemoveDirectoryW(p))
}

/// Sets the current directory.
pub extern "win64" fn _chdir(path: *const c_char) -> i32 {
    path_call(narrow_path(path), |p| SetCurrentDirectoryW(p))
}

/// See [`_chdir`].
pub extern "win64" fn _wchdir(path: *const u16) -> i32 {
    path_call(wide_path(path), |p| SetCurrentDirectoryW(p))
}

/// A character of a narrow or wide path.
trait Char: Copy + Default + Eq + 'static {
    fn ascii(b: u8) -> Self;
    /// The title's NUL-terminated string's characters; empty for null.
    ///
    /// # Safety
    /// `s` is null or a NUL-terminated string.
    unsafe fn units<'a>(s: *const Self) -> &'a [Self];
    /// A path as the title gets it back (narrow: UTF-8).
    fn from_wide(units: &[u16]) -> Vec<Self>;
    /// A title's path as NUL-terminated UTF-16.
    fn to_wide(text: &[Self]) -> Vec<u16>;
}

impl Char for u8 {
    fn ascii(b: u8) -> Self {
        b
    }
    unsafe fn units<'a>(s: *const Self) -> &'a [Self] {
        // SAFETY: as the caller says.
        unsafe { narrow_bytes(s.cast()) }
    }
    fn from_wide(units: &[u16]) -> Vec<Self> {
        String::from_utf16_lossy(units).into_bytes()
    }
    fn to_wide(text: &[Self]) -> Vec<u16> {
        String::from_utf8_lossy(text)
            .encode_utf16()
            .chain([0])
            .collect()
    }
}

impl Char for u16 {
    fn ascii(b: u8) -> Self {
        u16::from(b)
    }
    unsafe fn units<'a>(s: *const Self) -> &'a [Self] {
        // SAFETY: as the caller says.
        unsafe { wide_units(s) }
    }
    fn from_wide(units: &[u16]) -> Vec<Self> {
        units.to_vec()
    }
    fn to_wide(text: &[Self]) -> Vec<u16> {
        text.iter().copied().chain([0]).collect()
    }
}

/// `text` and a NUL to the title's buffer of `size` characters, or, for a
/// null buffer, to a `malloc` block of at least `minimum` characters; the
/// buffer, or `ERANGE` (`ENOMEM`) when it does not fit.
fn give_back<T: Char>(
    text: &[T],
    buffer: *mut T,
    size: usize,
    minimum: usize,
) -> Result<*mut T, i32> {
    let needed = text.len() + 1;
    let target = if buffer.is_null() {
        let count = needed.max(minimum);
        let block = malloc(count * std::mem::size_of::<T>()).cast::<T>();
        if block.is_null() {
            return Err(error(ENOMEM));
        }
        block
    } else {
        if needed > size {
            return Err(error(ERANGE));
        }
        buffer
    };
    // SAFETY: a buffer of at least `needed` characters.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), target, text.len());
        target.add(text.len()).write(T::default());
    }
    Ok(target)
}

/// The current directory, as UTF-16.
fn current_directory() -> Vec<u16> {
    let mut buffer = vec![0u16; 32768];
    let n = GetCurrentDirectoryW(buffer.len() as u32, buffer.as_mut_ptr());
    buffer.truncate(n as usize);
    buffer
}

/// `_getcwd`'s body: a null buffer gets a `malloc` block of at least
/// `size` characters.
fn getcwd<T: Char>(buffer: *mut T, size: i32) -> *mut T {
    if !buffer.is_null() && size <= 0 {
        invalid(EINVAL);
        return std::ptr::null_mut();
    }
    let size = usize::try_from(size).unwrap_or(0);
    let text = T::from_wide(&current_directory());
    give_back(&text, buffer, size, size).unwrap_or(std::ptr::null_mut())
}

/// The current directory into `buffer` (`malloc`'s when null).
pub extern "win64" fn _getcwd(buffer: *mut c_char, size: i32) -> *mut c_char {
    getcwd(buffer.cast::<u8>(), size).cast()
}

/// See [`_getcwd`].
pub extern "win64" fn _wgetcwd(buffer: *mut u16, size: i32) -> *mut u16 {
    getcwd(buffer, size)
}

/// `_getdcwd`'s body: drive 0 is the current one; another mounted drive's
/// directory is its root (there is one current directory); an unmounted
/// one is `EACCES`.
fn getdcwd<T: Char>(drive: i32, buffer: *mut T, size: i32) -> *mut T {
    let current = current_directory();
    let current_drive = current
        .first()
        .map_or(0, |&c| i32::from(c as u8 & !0x20) - 0x40);
    if drive == 0 || drive == current_drive {
        return getcwd(buffer, size);
    }
    let mounted = (1..=26).contains(&drive) && GetLogicalDrives() & (1 << (drive - 1)) != 0;
    if !mounted {
        _set_doserrno(ERROR_INVALID_DRIVE);
        invalid(EACCES);
        return std::ptr::null_mut();
    }
    if !buffer.is_null() && size <= 0 {
        invalid(EINVAL);
        return std::ptr::null_mut();
    }
    let root: Vec<u16> = format!("{}:\\", char::from(b'@' + drive as u8))
        .encode_utf16()
        .collect();
    let size = usize::try_from(size).unwrap_or(0);
    give_back(&T::from_wide(&root), buffer, size, size).unwrap_or(std::ptr::null_mut())
}

/// The current directory of a drive (1 = A, 0 = the current drive).
pub extern "win64" fn _getdcwd(drive: i32, buffer: *mut c_char, size: i32) -> *mut c_char {
    getdcwd(drive, buffer.cast::<u8>(), size).cast()
}

/// See [`_getdcwd`].
pub extern "win64" fn _wgetdcwd(drive: i32, buffer: *mut u16, size: i32) -> *mut u16 {
    getdcwd(drive, buffer, size)
}

/// The current drive (1 = A).
pub extern "win64" fn _getdrive() -> i32 {
    current_directory()
        .first()
        .map_or(0, |&c| i32::from(c as u8 & !0x20) - 0x40)
}

/// Makes a drive (1 = A) current: `SetCurrentDirectoryW("X:")`, which is
/// its root here.
pub extern "win64" fn _chdrive(drive: i32) -> i32 {
    if !(1..=26).contains(&drive) {
        _set_doserrno(ERROR_INVALID_DRIVE);
        invalid(EACCES);
        return -1;
    }
    let path: Vec<u16> = [u16::from(b'@' + drive as u8), u16::from(b':'), 0].to_vec();
    if SetCurrentDirectoryW(path.as_ptr()) == 0 {
        os_error();
        return -1;
    }
    0
}

/// A bit per mounted drive (bit 0 is `A:`).
pub extern "win64" fn _getdrives() -> u32 {
    GetLogicalDrives()
}

/// `_fullpath`'s body: a null or empty path is the current directory.
fn fullpath<T: Char>(buffer: *mut T, path: *const T, size: usize) -> *mut T {
    // SAFETY: the title's string or null.
    let text = unsafe { T::units(path) };
    if text.is_empty() {
        return getcwd(buffer, i32::try_from(size).unwrap_or(i32::MAX));
    }
    let wide = T::to_wide(text);
    let mut full = vec![0u16; 32768];
    let n = GetFullPathNameW(
        wide.as_ptr(),
        full.len() as u32,
        full.as_mut_ptr(),
        std::ptr::null_mut(),
    );
    if n == 0 || n as usize >= full.len() {
        os_error();
        return std::ptr::null_mut();
    }
    full.truncate(n as usize);
    give_back(&T::from_wide(&full), buffer, size, 0).unwrap_or(std::ptr::null_mut())
}

/// The full path of `path` into `buffer` (`malloc`'s when null).
pub extern "win64" fn _fullpath(
    buffer: *mut c_char,
    path: *const c_char,
    size: usize,
) -> *mut c_char {
    fullpath(buffer.cast::<u8>(), path.cast::<u8>(), size).cast()
}

/// See [`_fullpath`].
pub extern "win64" fn _wfullpath(buffer: *mut u16, path: *const u16, size: usize) -> *mut u16 {
    fullpath(buffer, path, size)
}

/// A path's drive (`X:`), directory (up to its last separator), name and
/// extension (from its last `.` after the directory).
fn split<T: Char>(path: &[T]) -> [&[T]; 4] {
    let (drive, rest) = if path.len() >= 2 && path[1] == T::ascii(b':') {
        path.split_at(2)
    } else {
        (&path[..0], path)
    };
    let separator = rest
        .iter()
        .rposition(|&c| c == T::ascii(b'\\') || c == T::ascii(b'/'));
    let (dir, tail) = match separator {
        Some(at) => rest.split_at(at + 1),
        None => (&rest[..0], rest),
    };
    let (name, ext) = match tail.iter().rposition(|&c| c == T::ascii(b'.')) {
        Some(at) => tail.split_at(at),
        None => (tail, &tail[..0]),
    };
    [drive, dir, name, ext]
}

/// `_splitpath_s`'s body: each part to its buffer (null with size 0 to
/// skip it); `EINVAL` for a null path or a buffer and size that disagree,
/// `ERANGE` for a part that does not fit (every buffer emptied, an invalid
/// parameter).
fn splitpath_s<T: Char>(path: *const T, buffers: [(*mut T, usize); 4]) -> i32 {
    let reset = || {
        for (buffer, size) in buffers {
            if !buffer.is_null() && size > 0 {
                // SAFETY: the title's buffer of `size` characters.
                unsafe { buffer.write(T::default()) };
            }
        }
    };
    let mismatched = buffers
        .iter()
        .any(|&(buffer, size)| buffer.is_null() != (size == 0));
    if path.is_null() || mismatched {
        reset();
        return invalid(EINVAL);
    }
    // SAFETY: the title's NUL-terminated path.
    let parts = split(unsafe { T::units(path) });
    if parts
        .iter()
        .zip(buffers)
        .any(|(part, (buffer, size))| !buffer.is_null() && part.len() + 1 > size)
    {
        reset();
        return invalid(ERANGE);
    }
    for (part, (buffer, _)) in parts.iter().zip(buffers) {
        if !buffer.is_null() {
            // SAFETY: the title's buffer, which holds the part and a NUL.
            unsafe {
                std::ptr::copy_nonoverlapping(part.as_ptr(), buffer, part.len());
                buffer.add(part.len()).write(T::default());
            }
        }
    }
    0
}

/// Splits a path into drive, directory, name and extension.
pub extern "win64" fn _splitpath_s(
    path: *const c_char,
    drive: *mut c_char,
    drive_size: usize,
    dir: *mut c_char,
    dir_size: usize,
    name: *mut c_char,
    name_size: usize,
    ext: *mut c_char,
    ext_size: usize,
) -> i32 {
    splitpath_s(
        path.cast::<u8>(),
        [
            (drive.cast(), drive_size),
            (dir.cast(), dir_size),
            (name.cast(), name_size),
            (ext.cast(), ext_size),
        ],
    )
}

/// See [`_splitpath_s`].
pub extern "win64" fn _wsplitpath_s(
    path: *const u16,
    drive: *mut u16,
    drive_size: usize,
    dir: *mut u16,
    dir_size: usize,
    name: *mut u16,
    name_size: usize,
    ext: *mut u16,
    ext_size: usize,
) -> i32 {
    splitpath_s(
        path,
        [
            (drive, drive_size),
            (dir, dir_size),
            (name, name_size),
            (ext, ext_size),
        ],
    )
}

/// The buffers of `_splitpath`, sized `_MAX_DRIVE`, `_MAX_DIR`,
/// `_MAX_FNAME` and `_MAX_EXT` (0 for a null one).
fn unsized_buffers<T>(buffers: [*mut T; 4]) -> [(*mut T, usize); 4] {
    let sizes = [MAX_DRIVE, MAX_DIR, MAX_FNAME, MAX_EXT];
    std::array::from_fn(|i| {
        let buffer = buffers[i];
        (buffer, if buffer.is_null() { 0 } else { sizes[i] })
    })
}

/// See [`_splitpath_s`], into buffers of the `_MAX_*` sizes.
pub extern "win64" fn _splitpath(
    path: *const c_char,
    drive: *mut c_char,
    dir: *mut c_char,
    name: *mut c_char,
    ext: *mut c_char,
) {
    let buffers = [drive, dir, name, ext].map(|b| b.cast::<u8>());
    splitpath_s(path.cast::<u8>(), unsized_buffers(buffers));
}

/// See [`_splitpath`].
pub extern "win64" fn _wsplitpath(
    path: *const u16,
    drive: *mut u16,
    dir: *mut u16,
    name: *mut u16,
    ext: *mut u16,
) {
    splitpath_s(path, unsized_buffers([drive, dir, name, ext]));
}

/// `_makepath_s`'s body: the drive's letter and `:`, the directory and a
/// `\` if it has no separator at its end, the name, and `.` and the
/// extension; `ERANGE` when it does not fit.
fn makepath_s<T: Char>(out: *mut T, size: usize, parts: [*const T; 4]) -> i32 {
    if out.is_null() || size == 0 {
        return invalid(EINVAL);
    }
    // SAFETY: the title's strings, or null.
    let [drive, dir, name, ext] = parts.map(|p| unsafe { T::units(p) });
    let mut path: Vec<T> = Vec::new();
    if let Some(&letter) = drive.first() {
        path.push(letter);
        path.push(T::ascii(b':'));
    }
    if let Some(&last) = dir.last() {
        path.extend_from_slice(dir);
        if last != T::ascii(b'\\') && last != T::ascii(b'/') {
            path.push(T::ascii(b'\\'));
        }
    }
    path.extend_from_slice(name);
    if let Some(&first) = ext.first() {
        if first != T::ascii(b'.') {
            path.push(T::ascii(b'.'));
        }
        path.extend_from_slice(ext);
    }
    if path.len() + 1 > size {
        // SAFETY: the title's buffer of `size` characters.
        unsafe { out.write(T::default()) };
        return invalid(ERANGE);
    }
    // SAFETY: as above; the path and its NUL fit.
    unsafe {
        std::ptr::copy_nonoverlapping(path.as_ptr(), out, path.len());
        out.add(path.len()).write(T::default());
    }
    0
}

/// Joins a drive, directory, name and extension into a path.
pub extern "win64" fn _makepath_s(
    out: *mut c_char,
    size: usize,
    drive: *const c_char,
    dir: *const c_char,
    name: *const c_char,
    ext: *const c_char,
) -> i32 {
    makepath_s(
        out.cast::<u8>(),
        size,
        [drive, dir, name, ext].map(|p| p.cast::<u8>()),
    )
}

/// See [`_makepath_s`].
pub extern "win64" fn _wmakepath_s(
    out: *mut u16,
    size: usize,
    drive: *const u16,
    dir: *const u16,
    name: *const u16,
    ext: *const u16,
) -> i32 {
    makepath_s(out, size, [drive, dir, name, ext])
}

/// See [`_makepath_s`], into a buffer of `_MAX_PATH`.
pub extern "win64" fn _makepath(
    out: *mut c_char,
    drive: *const c_char,
    dir: *const c_char,
    name: *const c_char,
    ext: *const c_char,
) {
    _makepath_s(out, MAX_PATH, drive, dir, name, ext);
}

/// See [`_makepath`].
pub extern "win64" fn _wmakepath(
    out: *mut u16,
    drive: *const u16,
    dir: *const u16,
    name: *const u16,
    ext: *const u16,
) {
    _wmakepath_s(out, MAX_PATH, drive, dir, name, ext);
}

// ---------------------------------------------------------------------------
// Finding files.

/// A `_finddata` structure, filled from a find entry.
trait FindData {
    fn fill(&mut self, data: &WIN32_FIND_DATAW);
}

/// A find entry's name, without its NUL.
fn entry_name(data: &WIN32_FIND_DATAW) -> &[u16] {
    let n = data
        .file_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(data.file_name.len());
    &data.file_name[..n]
}

macro_rules! find_struct {
    ($(#[$doc:meta])* $name:ident, $char:ty, $time:ty, $size:ty, $bytes:expr) => {
        $(#[$doc])*
        #[repr(C)]
        pub struct $name {
            pub attrib: u32,
            pub time_create: $time,
            pub time_access: $time,
            pub time_write: $time,
            pub size: $size,
            pub name: [$char; 260],
        }

        const _: () = assert!(std::mem::size_of::<$name>() == $bytes);

        impl FindData for $name {
            fn fill(&mut self, data: &WIN32_FIND_DATAW) {
                self.attrib = if data.attributes == FILE_ATTRIBUTE_NORMAL {
                    0
                } else {
                    data.attributes
                };
                self.time_create = <$time>::narrowed(time_of(data.creation));
                self.time_access = <$time>::narrowed(time_of(data.last_access));
                self.time_write = <$time>::narrowed(time_of(data.last_write));
                let size = (u64::from(data.size_high) << 32 | u64::from(data.size_low)) as i64;
                self.size = <$size>::narrowed(size);
                let name = <$char>::from_wide(entry_name(data));
                let n = name.len().min(259);
                self.name[..n].copy_from_slice(&name[..n]);
                self.name[n] = <$char>::default();
            }
        }
    };
}

find_struct!(
    /// `struct _finddata64_t`.
    FindData64, u8, i64, i64, 304
);
find_struct!(
    /// `struct _wfinddata64_t`.
    WFindData64, u16, i64, i64, 560
);
find_struct!(
    /// `struct _finddata64i32_t`.
    FindData64i32, u8, i64, u32, 296
);
find_struct!(
    /// `struct _wfinddata64i32_t`.
    WFindData64i32, u16, i64, u32, 560
);
find_struct!(
    /// `struct _finddata32_t`.
    FindData32, u8, i32, u32, 280
);
find_struct!(
    /// `struct _wfinddata32_t`.
    WFindData32, u16, i32, u32, 540
);

/// A find failure's `errno`, as the UCRT maps it.
fn find_error() -> i32 {
    let code = last_error();
    _set_doserrno(code);
    error(match code {
        ERROR_FILE_NOT_FOUND
        | ERROR_PATH_NOT_FOUND
        | ERROR_NO_MORE_FILES
        | ERROR_FILENAME_EXCED_RANGE => ENOENT,
        ERROR_NOT_ENOUGH_MEMORY => ENOMEM,
        _ => EINVAL,
    })
}

/// A blank find entry.
fn blank_entry() -> WIN32_FIND_DATAW {
    // SAFETY: plain data, for which zeros are a value.
    unsafe { std::mem::zeroed() }
}

/// `_findfirst`'s body: the find handle, or -1.
fn find_first<T: FindData>(pattern: Option<Vec<u16>>, out: *mut T) -> isize {
    let (Some(pattern), false) = (pattern, out.is_null()) else {
        invalid(EINVAL);
        return -1;
    };
    let mut data = blank_entry();
    let handle = FindFirstFileW(pattern.as_ptr(), &mut data);
    if handle as usize == INVALID_HANDLE {
        find_error();
        return -1;
    }
    // SAFETY: the title's structure, checked non-null.
    unsafe { (*out).fill(&data) };
    handle as isize
}

/// `_findnext`'s body: 0, or -1.
fn find_next<T: FindData>(handle: isize, out: *mut T) -> i32 {
    if out.is_null() {
        invalid(EINVAL);
        return -1;
    }
    let mut data = blank_entry();
    if FindNextFileW(handle as *mut c_void, &mut data) == 0 {
        find_error();
        return -1;
    }
    // SAFETY: the title's structure, checked non-null.
    unsafe { (*out).fill(&data) };
    0
}

macro_rules! find_functions {
    ($ty:ident, $wty:ident, $first:ident, $next:ident, $wfirst:ident, $wnext:ident) => {
        /// The first entry a pattern matches; the handle for the rest.
        pub extern "win64" fn $first(pattern: *const c_char, out: *mut $ty) -> isize {
            find_first(narrow_path(pattern), out)
        }

        /// The next entry.
        pub extern "win64" fn $next(handle: isize, out: *mut $ty) -> i32 {
            find_next(handle, out)
        }

        /// The first entry a pattern matches; the handle for the rest.
        pub extern "win64" fn $wfirst(pattern: *const u16, out: *mut $wty) -> isize {
            find_first(wide_path(pattern), out)
        }

        /// The next entry.
        pub extern "win64" fn $wnext(handle: isize, out: *mut $wty) -> i32 {
            find_next(handle, out)
        }
    };
}

find_functions!(
    FindData64,
    WFindData64,
    _findfirst64,
    _findnext64,
    _wfindfirst64,
    _wfindnext64
);
find_functions!(
    FindData64i32,
    WFindData64i32,
    _findfirst64i32,
    _findnext64i32,
    _wfindfirst64i32,
    _wfindnext64i32
);
find_functions!(
    FindData32,
    WFindData32,
    _findfirst32,
    _findnext32,
    _wfindfirst32,
    _wfindnext32
);

/// Closes a find handle.
pub extern "win64" fn _findclose(handle: isize) -> i32 {
    if FindClose(handle as *mut c_void) == 0 {
        _set_doserrno(last_error());
        error(EINVAL);
        return -1;
    }
    0
}

// ---------------------------------------------------------------------------
// Temporary names.

/// `_mktemp_s`'s body over the template's characters: the six `X`s at its
/// end become a letter (the first from `a` that names no file) and five
/// digits of the process id; `EINVAL` for another template, `EEXIST` when
/// every letter is taken.
fn mktemp<T: Char>(template: &mut [T]) -> Result<(), i32> {
    let n = template.len();
    if n < 6 || template[n - 6..].iter().any(|&c| c != T::ascii(b'X')) {
        return Err(invalid(EINVAL));
    }
    let mut number = _getpid() as u32;
    for slot in template[n - 5..].iter_mut().rev() {
        *slot = T::ascii(b'0' + (number % 10) as u8);
        number /= 10;
    }
    for letter in b'a'..=b'z' {
        template[n - 6] = T::ascii(letter);
        let wide = T::to_wide(template);
        if GetFileAttributesW(wide.as_ptr()) == INVALID_FILE_ATTRIBUTES {
            return Ok(());
        }
    }
    Err(error(EEXIST))
}

/// `_mktemp_s`'s body over the title's buffer of `size` characters.
fn mktemp_s<T: Char>(template: *mut T, size: usize) -> i32 {
    if template.is_null() || size == 0 {
        return invalid(EINVAL);
    }
    // SAFETY: the title's buffer of `size` characters.
    let buffer = unsafe { std::slice::from_raw_parts_mut(template, size) };
    let Some(n) = buffer.iter().position(|&c| c == T::default()) else {
        return invalid(EINVAL);
    };
    match mktemp(&mut buffer[..n]) {
        Ok(()) => 0,
        Err(code) => code,
    }
}

/// Makes a unique file name of a template ending in `XXXXXX`.
pub extern "win64" fn _mktemp_s(template: *mut c_char, size: usize) -> i32 {
    mktemp_s(template.cast::<u8>(), size)
}

/// See [`_mktemp_s`].
pub extern "win64" fn _wmktemp_s(template: *mut u16, size: usize) -> i32 {
    mktemp_s(template, size)
}

/// See [`_mktemp_s`]: the template, or null.
pub extern "win64" fn _mktemp(template: *mut c_char) -> *mut c_char {
    if template.is_null() {
        invalid(EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: the title's NUL-terminated template.
    let n = unsafe { narrow_bytes(template) }.len();
    if mktemp_s(template.cast::<u8>(), n + 1) == 0 {
        template
    } else {
        std::ptr::null_mut()
    }
}

/// See [`_mktemp`].
pub extern "win64" fn _wmktemp(template: *mut u16) -> *mut u16 {
    if template.is_null() {
        invalid(EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: the title's NUL-terminated template.
    let n = unsafe { wide_units(template) }.len();
    if mktemp_s(template, n + 1) == 0 {
        template
    } else {
        std::ptr::null_mut()
    }
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(_open),
        hle_export!(_wopen),
        hle_export!(_sopen),
        hle_export!(_wsopen),
        hle_export!(_sopen_s),
        hle_export!(_wsopen_s),
        hle_export!(_sopen_dispatch),
        hle_export!(_wsopen_dispatch),
        hle_export!(_close),
        hle_export!(_read),
        hle_export!(_write),
        hle_export!(_lseek),
        hle_export!(_lseeki64),
        hle_export!(_tell),
        hle_export!(_telli64),
        hle_export!(_commit),
        hle_export!(_chsize),
        hle_export!(_chsize_s),
        hle_export!(_filelength),
        hle_export!(_filelengthi64),
        hle_export!(_eof),
        hle_export!(_isatty),
        hle_export!(_setmode),
        hle_export!(_get_osfhandle),
        hle_export!(_open_osfhandle),
        hle_export!(_dup),
        hle_export!(_dup2),
        hle_export!(_locking),
        hle_export!(_pipe),
        hle_export!(_umask),
        hle_export!(_umask_s),
        hle_export!(_fstat64),
        hle_export!(_fstat64i32),
        hle_export!(_fstat32),
        hle_export!(_fstat32i64),
        hle_export!(_stat64),
        hle_export!(_stat64i32),
        hle_export!(_stat32),
        hle_export!(_stat32i64),
        hle_export!(_wstat64),
        hle_export!(_wstat64i32),
        hle_export!(_wstat32),
        hle_export!(_wstat32i64),
        hle_export!(_access),
        hle_export!(_waccess),
        hle_export!(_access_s),
        hle_export!(_waccess_s),
        hle_export!(_unlink),
        hle_export!(_wunlink),
        hle_export!(remove),
        hle_export!(_wremove),
        hle_export!(rename),
        hle_export!(_wrename),
        hle_export!(_mkdir),
        hle_export!(_wmkdir),
        hle_export!(_rmdir),
        hle_export!(_wrmdir),
        hle_export!(_chdir),
        hle_export!(_wchdir),
        hle_export!(_getcwd),
        hle_export!(_wgetcwd),
        hle_export!(_getdcwd),
        hle_export!(_wgetdcwd),
        hle_export!(_getdrive),
        hle_export!(_chdrive),
        hle_export!(_getdrives),
        hle_export!(_fullpath),
        hle_export!(_wfullpath),
        hle_export!(_splitpath),
        hle_export!(_wsplitpath),
        hle_export!(_splitpath_s),
        hle_export!(_wsplitpath_s),
        hle_export!(_makepath),
        hle_export!(_wmakepath),
        hle_export!(_makepath_s),
        hle_export!(_wmakepath_s),
        hle_export!(_findfirst64),
        hle_export!(_findnext64),
        hle_export!(_wfindfirst64),
        hle_export!(_wfindnext64),
        hle_export!(_findfirst64i32),
        hle_export!(_findnext64i32),
        hle_export!(_wfindfirst64i32),
        hle_export!(_wfindnext64i32),
        hle_export!(_findfirst32),
        hle_export!(_findnext32),
        hle_export!(_wfindfirst32),
        hle_export!(_wfindnext32),
        hle_export!(_findclose),
        hle_export!(_mktemp_s),
        hle_export!(_wmktemp_s),
        hle_export!(_mktemp),
        hle_export!(_wmktemp),
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::super::runtime::{_set_thread_local_invalid_parameter_handler, errno};
    use super::*;

    /// The drive the stdio tests share: one temporary folder, mounted once
    /// (drive letters are few, and other tests mount theirs).
    pub(crate) fn drive() -> char {
        static DRIVE: OnceLock<(tempfile::TempDir, char)> = OnceLock::new();
        DRIVE
            .get_or_init(|| {
                let dir = tempfile::tempdir().unwrap();
                let letter = super::super::super::win32::files::mount(dir.path()).unwrap();
                (dir, letter)
            })
            .1
    }

    /// A NUL-terminated narrow string.
    pub(crate) fn c(s: &str) -> Vec<u8> {
        s.bytes().chain([0]).collect()
    }

    /// A NUL-terminated wide string.
    pub(crate) fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// A new folder of the test's own on the shared drive; its Windows path.
    /// The calling thread gets a TEB (for the last error).
    pub(crate) fn folder(name: &str) -> String {
        victory_loader::teb::attach_current_thread();
        let path = format!(r"{}:\{name}", drive());
        assert_eq!(_mkdir(c(&path).as_ptr().cast()), 0, "{path}");
        path
    }

    extern "win64" fn ignore(_: *const u16, _: *const u16, _: *const u16, _: u32, _: usize) {}

    /// Lets invalid parameters return (on this thread) for a test's life.
    pub(crate) struct Tolerant;

    impl Tolerant {
        pub(crate) fn new() -> Self {
            _set_thread_local_invalid_parameter_handler(Some(ignore));
            Tolerant
        }
    }

    impl Drop for Tolerant {
        fn drop(&mut self) {
            _set_thread_local_invalid_parameter_handler(None);
        }
    }

    /// Held shared by the tests that open descriptors, and alone by those
    /// that free one another test could be given (a second close, or
    /// moving a standard one).
    static DESCRIPTORS: std::sync::RwLock<()> = std::sync::RwLock::new(());

    pub(crate) fn shared() -> std::sync::RwLockReadGuard<'static, ()> {
        DESCRIPTORS
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn exclusive() -> std::sync::RwLockWriteGuard<'static, ()> {
        DESCRIPTORS
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn open_file(path: &str, oflag: i32) -> i32 {
        let mut fd = -2;
        let path = c(path);
        let code = _sopen_s(
            &mut fd,
            path.as_ptr().cast(),
            oflag,
            SH_DENYNO,
            S_IREAD | S_IWRITE,
        );
        assert_eq!(code, 0, "errno {code}");
        fd
    }

    fn write(fd: i32, data: &[u8]) -> i32 {
        _write(fd, data.as_ptr().cast(), data.len() as u32)
    }

    fn read(fd: i32, n: usize) -> Vec<u8> {
        let mut buffer = vec![0u8; n];
        let got = _read(fd, buffer.as_mut_ptr().cast(), n as u32);
        assert!(got >= 0, "errno {}", errno());
        buffer.truncate(got as usize);
        buffer
    }

    fn host_bytes(path: &str) -> Vec<u8> {
        let fd = open_file(path, O_RDONLY | O_BINARY);
        let bytes = read(fd, 4096);
        assert_eq!(_close(fd), 0);
        bytes
    }

    #[test]
    fn descriptors_write_seek_read_and_close() {
        let _shared = shared();
        let _tolerant = Tolerant::new();
        let dir = folder("lowio-io");
        let path = format!(r"{dir}\data.bin");
        let fd = open_file(&path, O_RDWR | O_CREAT | O_BINARY);
        assert!(fd >= 3);
        assert_eq!(write(fd, b"hello world"), 11);
        assert_eq!(_lseeki64(fd, 0, 0), 0);
        assert_eq!(read(fd, 5), b"hello");
        assert_eq!(_filelengthi64(fd), 11);
        assert_eq!(_filelength(fd), 11);
        assert_eq!((_eof(fd), _tell(fd)), (0, 5));
        assert_eq!(read(fd, 100), b" world");
        assert_eq!(_eof(fd), 1);
        assert_eq!(_lseek(fd, -5, 2), 6);
        assert_eq!(_telli64(fd), 6);
        assert_eq!(_chsize(fd, 3), 0);
        assert_eq!((_filelengthi64(fd), _telli64(fd)), (3, 6));
        assert_eq!(_chsize_s(fd, 8), 0);
        assert_eq!(_lseek(fd, 0, 0), 0);
        assert_eq!(read(fd, 100), b"hel\0\0\0\0\0");
        assert_eq!(_commit(fd), 0);
        assert_eq!(_isatty(fd), 0);
        assert_eq!(_locking(fd, 0, 1), 0);
        assert_eq!(_close(fd), 0);
        // A closed descriptor's number goes to the next open (maybe another
        // test's): one no test uses stands for it.
        assert_eq!(_close(7999), -1);
        assert_eq!(errno(), EBADF);
        assert_eq!(_read(9000, std::ptr::null_mut(), 0), -1);
        assert_eq!(errno(), EBADF);
    }

    #[test]
    fn text_mode_translates_newlines_and_stops_at_ctrl_z() {
        let _shared = shared();
        let dir = folder("lowio-text");
        let path = format!(r"{dir}\text.txt");
        let fd = open_file(&path, O_RDWR | O_CREAT | O_TEXT);
        assert_eq!(write(fd, b"a\nb\n"), 4);
        assert_eq!(_filelengthi64(fd), 6);
        assert_eq!(_lseek(fd, 0, 0), 0);
        assert_eq!(read(fd, 100), b"a\nb\n");
        assert_eq!(_close(fd), 0);
        assert_eq!(host_bytes(&path), b"a\r\nb\r\n");
        // A `\r` at the end of a read is decided by the next byte.
        let fd = open_file(&path, O_RDONLY | O_TEXT);
        assert_eq!(read(fd, 2), b"a\n");
        assert_eq!(read(fd, 1), b"b");
        assert_eq!(read(fd, 1), b"\n");
        assert_eq!(_close(fd), 0);

        let fd = open_file(&path, O_WRONLY | O_TRUNC | O_BINARY);
        assert_eq!(write(fd, b"x\r\ny\r\x1Az"), 7);
        assert_eq!(_close(fd), 0);
        let fd = open_file(&path, O_RDONLY | O_TEXT);
        assert_eq!(read(fd, 100), b"x\ny\r");
        assert_eq!(read(fd, 100), b"");
        assert_eq!(_close(fd), 0);
        // Opened for update, a text file loses its last `^Z`.
        let fd = open_file(&path, O_WRONLY | O_TRUNC | O_BINARY);
        assert_eq!(write(fd, b"q\x1A"), 2);
        assert_eq!(_close(fd), 0);
        let fd = open_file(&path, O_RDWR | O_TEXT);
        assert_eq!(_filelengthi64(fd), 1);
        assert_eq!(_close(fd), 0);
        // `_setmode` reports the previous mode.
        let fd = open_file(&path, O_RDONLY);
        assert_eq!(_setmode(fd, O_BINARY), O_TEXT);
        assert_eq!(_setmode(fd, O_U16TEXT), O_BINARY);
        assert_eq!(_setmode(fd, O_TEXT), O_WTEXT);
        assert_eq!(_close(fd), 0);
    }

    #[test]
    fn unicode_descriptors_convert_and_mark_their_files() {
        let _shared = shared();
        let dir = folder("lowio-unicode");
        let utf8 = format!(r"{dir}\u8.txt");
        let units: Vec<u8> = "é\n".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let fd = open_file(&utf8, O_WRONLY | O_CREAT | O_U8TEXT);
        assert_eq!(write(fd, &units), 4);
        assert_eq!(_close(fd), 0);
        assert_eq!(host_bytes(&utf8), b"\xEF\xBB\xBF\xC3\xA9\r\n");
        let fd = open_file(&utf8, O_RDONLY | O_WTEXT);
        assert_eq!(read(fd, 64), units);
        assert_eq!(_close(fd), 0);

        let utf16 = format!(r"{dir}\u16.txt");
        let fd = open_file(&utf16, O_RDWR | O_CREAT | O_U16TEXT);
        assert_eq!(write(fd, &units), 4);
        assert_eq!(_lseek(fd, 2, 0), 2);
        assert_eq!(read(fd, 64), units);
        assert_eq!(_close(fd), 0);
        assert_eq!(host_bytes(&utf16), b"\xFF\xFE\xE9\x00\r\x00\n\x00");
    }

    #[test]
    fn appends_go_to_the_end_and_exclusive_creates_fail() {
        let _shared = shared();
        let _tolerant = Tolerant::new();
        let dir = folder("lowio-append");
        let path = format!(r"{dir}\log.txt");
        let fd = open_file(&path, O_RDWR | O_CREAT | O_APPEND | O_BINARY);
        assert_eq!(write(fd, b"12"), 2);
        assert_eq!(_lseek(fd, 0, 0), 0);
        assert_eq!(write(fd, b"34"), 2);
        assert_eq!(_close(fd), 0);
        assert_eq!(host_bytes(&path), b"1234");
        let mut fd = 0;
        let name = c(&path);
        let flags = O_RDWR | O_CREAT | O_EXCL;
        assert_eq!(
            _sopen_s(&mut fd, name.as_ptr().cast(), flags, SH_DENYNO, S_IWRITE),
            EEXIST
        );
        assert_eq!((fd, errno()), (-1, EEXIST));
        assert_eq!(_open(name.as_ptr().cast(), flags, 0), -1);
        assert_eq!(errno(), EEXIST);
        let missing = c(&format!(r"{dir}\missing.txt"));
        assert_eq!(_open(missing.as_ptr().cast(), O_RDONLY, 0), -1);
        assert_eq!(errno(), ENOENT);
        assert_eq!(
            _sopen_s(&mut fd, name.as_ptr().cast(), 3, SH_DENYNO, 0),
            EINVAL
        );
        // A created file without `_S_IWRITE` is read-only.
        let read_only = format!(r"{dir}\ro.txt");
        let ro = c(&read_only);
        let fd = _open(ro.as_ptr().cast(), O_WRONLY | O_CREAT, S_IREAD);
        assert!(fd >= 0);
        assert_eq!(write(fd, b"x"), 1);
        assert_eq!(_close(fd), 0);
        assert_eq!(_access(ro.as_ptr().cast(), 2), -1);
        assert_eq!(errno(), EACCES);
        assert_eq!(_access_s(ro.as_ptr().cast(), 4), 0);
    }

    #[test]
    fn stat_reports_modes_sizes_and_times() {
        let _shared = shared();
        let dir = folder("lowio-stat");
        let path = format!(r"{dir}\file.txt");
        let fd = open_file(&path, O_WRONLY | O_CREAT | O_BINARY);
        assert_eq!(write(fd, b"12345"), 5);
        let mut by_fd = Stat64::default();
        assert_eq!(_fstat64(fd, &mut by_fd), 0);
        assert_eq!((by_fd.st_mode, by_fd.st_size), (0x81B6, 5));
        assert_eq!(_close(fd), 0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut file = Stat64::default();
        assert_eq!(_wstat64(w(&path).as_ptr(), &mut file), 0);
        assert_eq!((file.st_mode, file.st_size, file.st_nlink), (0x81B6, 5, 1));
        let letter = u32::from(drive() as u8 - b'A');
        assert_eq!((file.st_dev, file.st_rdev), (letter, letter));
        assert!((file.st_mtime - now).abs() < 60, "{} {now}", file.st_mtime);
        let mut folder_stat = Stat32::default();
        assert_eq!(_wstat32(w(&dir).as_ptr(), &mut folder_stat), 0);
        assert_eq!(folder_stat.st_mode, 0x41FF);
        let mut exe = Stat64i32::default();
        let program = format!(r"{dir}\run.EXE");
        let fd = open_file(&program, O_WRONLY | O_CREAT);
        assert_eq!(_close(fd), 0);
        assert_eq!(_stat64i32(c(&program).as_ptr().cast(), &mut exe), 0);
        assert_eq!(exe.st_mode, 0x81FF);
        let mut wild = Stat32i64::default();
        assert_eq!(
            _stat32i64(c(&format!(r"{dir}\*")).as_ptr().cast(), &mut wild),
            -1
        );
        assert_eq!(errno(), ENOENT);
        let mut device = Stat64::default();
        assert_eq!(_fstat64(1, &mut device), 0);
        assert_eq!(device.st_mode, S_IFCHR);
        let missing = c(&format!(r"{dir}\missing"));
        assert_eq!(_access(missing.as_ptr().cast(), 0), -1);
        assert_eq!(errno(), ENOENT);
        assert_eq!(_access_s(missing.as_ptr().cast(), 0), ENOENT);
    }

    #[test]
    fn directories_renames_and_deletes() {
        let _shared = shared();
        let dir = folder("lowio-dirs");
        let sub = c(&format!(r"{dir}\sub"));
        assert_eq!(_mkdir(sub.as_ptr().cast()), 0);
        assert_eq!(_mkdir(sub.as_ptr().cast()), -1);
        assert_eq!(errno(), EEXIST);
        let a = format!(r"{dir}\a.txt");
        let b = format!(r"{dir}\b.txt");
        assert_eq!(_close(open_file(&a, O_WRONLY | O_CREAT)), 0);
        assert_eq!(rename(c(&a).as_ptr().cast(), c(&b).as_ptr().cast()), 0);
        assert_eq!(_access(c(&a).as_ptr().cast(), 0), -1);
        assert_eq!(_waccess(w(&b).as_ptr(), 0), 0);
        assert_eq!(_close(open_file(&a, O_WRONLY | O_CREAT)), 0);
        assert_eq!(_wrename(w(&a).as_ptr(), w(&b).as_ptr()), -1);
        assert_eq!(errno(), EEXIST);
        assert_eq!(_unlink(c(&a).as_ptr().cast()), 0);
        assert_eq!(_wremove(w(&b).as_ptr()), 0);
        assert_eq!(remove(c(&b).as_ptr().cast()), -1);
        assert_eq!(errno(), ENOENT);
        assert_eq!(_wrmdir(w(&format!(r"{dir}\sub")).as_ptr()), 0);
        assert_eq!(_rmdir(sub.as_ptr().cast()), -1);
        assert_eq!(errno(), ENOENT);
    }

    #[test]
    fn finds_list_dots_names_and_attributes() {
        let _shared = shared();
        let _tolerant = Tolerant::new();
        let dir = folder("lowio-find");
        assert_eq!(
            _close(open_file(&format!(r"{dir}\b.bin"), O_WRONLY | O_CREAT)),
            0
        );
        let fd = open_file(&format!(r"{dir}\a.txt"), O_WRONLY | O_CREAT | O_BINARY);
        assert_eq!(write(fd, b"abc"), 3);
        assert_eq!(_close(fd), 0);
        assert_eq!(_mkdir(c(&format!(r"{dir}\sub")).as_ptr().cast()), 0);
        // SAFETY: plain data.
        let mut data: FindData64 = unsafe { std::mem::zeroed() };
        let handle = _findfirst64(c(&format!(r"{dir}\*")).as_ptr().cast(), &mut data);
        assert_ne!(handle, -1);
        let mut seen = Vec::new();
        loop {
            let n = data.name.iter().position(|&b| b == 0).unwrap();
            let name = String::from_utf8_lossy(&data.name[..n]).into_owned();
            seen.push((name, data.attrib, data.size));
            if _findnext64(handle, &mut data) != 0 {
                break;
            }
        }
        assert_eq!(errno(), ENOENT);
        assert_eq!(_findclose(handle), 0);
        let names: Vec<&str> = seen.iter().map(|(n, ..)| n.as_str()).collect();
        assert_eq!(names, [".", "..", "a.txt", "b.bin", "sub"]);
        assert_eq!(seen[2].1, 0x20);
        assert_eq!(seen[2].2, 3);
        assert_eq!(seen[4].1 & 0x10, 0x10);
        // SAFETY: plain data.
        let mut wide_data: WFindData32 = unsafe { std::mem::zeroed() };
        let handle = _wfindfirst32(w(&format!(r"{dir}\*.txt")).as_ptr(), &mut wide_data);
        assert_ne!(handle, -1);
        assert_eq!(&wide_data.name[..6], &w("a.txt")[..]);
        assert_eq!(_wfindnext32(handle, &mut wide_data), -1);
        assert_eq!(_findclose(handle), 0);
        assert_eq!(
            _findfirst64(c(&format!(r"{dir}\*.none")).as_ptr().cast(), &mut data),
            -1
        );
        assert_eq!(errno(), ENOENT);
    }

    fn text_of(buffer: &[u8]) -> String {
        let n = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..n]).into_owned()
    }

    #[test]
    fn paths_split_join_and_resolve() {
        let _tolerant = Tolerant::new();
        let x = drive();
        let mut buffer = [0u8; 64];
        let path = c(&format!(r"{x}:\one\two\..\three.txt"));
        let full = _fullpath(buffer.as_mut_ptr().cast(), path.as_ptr().cast(), 64);
        assert_eq!(full.cast::<u8>(), buffer.as_mut_ptr());
        assert_eq!(text_of(&buffer), format!(r"{x}:\one\three.txt"));
        assert!(_fullpath(buffer.as_mut_ptr().cast(), path.as_ptr().cast(), 5).is_null());
        assert_eq!(errno(), ERANGE);
        let allocated = _wfullpath(std::ptr::null_mut(), w(&format!(r"{x}:/a/./b")).as_ptr(), 0);
        // SAFETY: a NUL-terminated block of ours.
        let units = unsafe { wide_units(allocated) }.to_vec();
        assert_eq!(String::from_utf16_lossy(&units), format!(r"{x}:\a\b"));
        super::super::heap::free(allocated.cast());

        let (mut drive_part, mut dir, mut name, mut ext) =
            ([0u8; 3], [0u8; 64], [0u8; 64], [0u8; 8]);
        let source = c(r"C:\dir\sub\name.tar.gz");
        let code = _splitpath_s(
            source.as_ptr().cast(),
            drive_part.as_mut_ptr().cast(),
            3,
            dir.as_mut_ptr().cast(),
            64,
            name.as_mut_ptr().cast(),
            64,
            ext.as_mut_ptr().cast(),
            8,
        );
        assert_eq!(code, 0);
        let parts = [
            text_of(&drive_part),
            text_of(&dir),
            text_of(&name),
            text_of(&ext),
        ];
        assert_eq!(parts, ["C:", r"\dir\sub\", "name.tar", ".gz"]);
        let mut joined = [0u8; 64];
        let parts_c = parts.map(|p| c(&p));
        let code = _makepath_s(
            joined.as_mut_ptr().cast(),
            64,
            parts_c[0].as_ptr().cast(),
            parts_c[1].as_ptr().cast(),
            parts_c[2].as_ptr().cast(),
            parts_c[3].as_ptr().cast(),
        );
        assert_eq!(code, 0);
        assert_eq!(text_of(&joined), r"C:\dir\sub\name.tar.gz");

        // Missing parts: no drive and no extension; a directory without
        // its separator and an extension without its dot are completed.
        let source = w("rel/file");
        let (mut wdrive, mut wdir, mut wname, mut wext) =
            ([9u16; 3], [9u16; 16], [9u16; 16], [9u16; 4]);
        let code = _wsplitpath_s(
            source.as_ptr(),
            wdrive.as_mut_ptr(),
            3,
            wdir.as_mut_ptr(),
            16,
            wname.as_mut_ptr(),
            16,
            wext.as_mut_ptr(),
            4,
        );
        assert_eq!(code, 0);
        assert_eq!(
            (wdrive[0], &wdir[..5], &wname[..5], wext[0]),
            (0, &w("rel/")[..], &w("file")[..], 0)
        );
        let mut wjoined = [0u16; 32];
        let code = _wmakepath_s(
            wjoined.as_mut_ptr(),
            32,
            w("D").as_ptr(),
            w("top").as_ptr(),
            w("f").as_ptr(),
            w("txt").as_ptr(),
        );
        assert_eq!(code, 0);
        assert_eq!(&wjoined[..12], &w(r"D:top\f.txt")[..]);
        let code = _makepath_s(
            joined.as_mut_ptr().cast(),
            4,
            std::ptr::null(),
            std::ptr::null(),
            parts_c[2].as_ptr().cast(),
            std::ptr::null(),
        );
        assert_eq!((code, joined[0]), (ERANGE, 0));
        let code = _splitpath_s(
            source.as_ptr().cast(),
            std::ptr::null_mut(),
            3,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
        );
        assert_eq!(code, EINVAL);
        let code = _splitpath_s(
            c("abcdef.ext").as_ptr().cast(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
            name.as_mut_ptr().cast(),
            3,
            std::ptr::null_mut(),
            0,
        );
        assert_eq!((code, name[0]), (ERANGE, 0));

        let mut template = c("tmpXXXXXX");
        assert_eq!(_mktemp_s(template.as_mut_ptr().cast(), template.len()), 0);
        assert_eq!(template[3], b'a');
        assert!(template[4..9].iter().all(u8::is_ascii_digit));
        let mut bad = c("tmpXX");
        assert!(_mktemp(bad.as_mut_ptr().cast()).is_null());
        assert_eq!(errno(), EINVAL);
    }

    #[test]
    fn the_current_directory_follows_chdir() {
        let dir = folder("lowio-cwd");
        let mut before = [0u16; 260];
        assert!(!_wgetcwd(before.as_mut_ptr(), 260).is_null());
        assert_eq!(_chdir(c(&dir).as_ptr().cast()), 0);
        let mut buffer = [0u8; 64];
        let got = _getcwd(buffer.as_mut_ptr().cast(), 64);
        let drive_now = _getdrive();
        let short = _getcwd(buffer.as_mut_ptr().cast(), 3);
        let short_errno = errno();
        // Put back (another test's drive may be gone: not checked).
        _wchdir(before.as_ptr());
        assert!(!got.is_null());
        assert_eq!(text_of(&buffer), dir);
        assert_eq!(drive_now, i32::from(drive() as u8 - b'A' + 1));
        assert!(short.is_null());
        assert_eq!(short_errno, ERANGE);
        assert_ne!(_getdrives() & (1 << (drive() as u8 - b'A')), 0);
        let root = _getdcwd(i32::from(drive() as u8 - b'A' + 1), std::ptr::null_mut(), 0);
        assert!(!root.is_null());
        super::super::heap::free(root.cast());
    }

    #[test]
    fn duplicates_share_the_file_and_standard_handles_stay() {
        let _shared = shared();
        let dir = folder("lowio-dup");
        let path = format!(r"{dir}\dup.bin");
        let fd = open_file(&path, O_RDWR | O_CREAT | O_BINARY);
        let copy = _dup(fd);
        assert!(copy >= 0 && copy != fd);
        assert_eq!(write(copy, b"abc"), 3);
        assert_eq!(_tell(fd), 3, "a duplicate shares the position");
        let target = 4000;
        assert_eq!(_dup2(fd, target), 0);
        assert_eq!(_lseek(target, 0, 0), 0);
        assert_eq!(read(target, 10), b"abc");
        assert_eq!(_close(target), 0);
        assert_eq!(_close(copy), 0);
        assert_eq!(_close(fd), 0);
        let handle = GetStdHandle(STD_OUTPUT_HANDLE) as isize;
        assert_eq!(_get_osfhandle(1), handle);
        assert_ne!(_isatty(1), 0);
        let wrapped = _open_osfhandle(handle, O_TEXT);
        assert!(wrapped >= 3);
        assert_ne!(_isatty(wrapped), 0);
        assert_eq!(_close(wrapped), 0);
        assert_eq!(
            _get_osfhandle(1),
            handle,
            "closing a standard handle keeps it"
        );
    }

    #[test]
    fn os_errors_map_to_errno_as_the_ucrt() {
        let cases = [
            (2, ENOENT),
            (4, EMFILE),
            (5, EACCES),
            (6, EBADF),
            (8, ENOMEM),
            (11, ENOEXEC),
            (17, EXDEV),
            (19, EACCES),
            (25, EACCES),
            (32, EACCES),
            (80, EEXIST),
            (87, EINVAL),
            (109, EPIPE),
            (112, ENOSPC),
            (145, ENOTEMPTY),
            (164, EAGAIN),
            (183, EEXIST),
            (190, ENOEXEC),
            (1816, ENOMEM),
            (9999, EINVAL),
        ];
        for (code, expected) in cases {
            assert_eq!(dos_map_err(code), expected, "{code}");
            assert_eq!(errno(), expected);
            let mut doserrno = 0;
            super::super::runtime::_get_doserrno(&mut doserrno);
            assert_eq!(doserrno, code);
        }
    }

    #[test]
    fn umask_and_pipes() {
        let _tolerant = Tolerant::new();
        // `_S_IREAD` changes nothing a concurrent test could see.
        assert_eq!(_umask(S_IREAD), 0);
        let mut old = -1;
        assert_eq!(_umask_s(0, &mut old), 0);
        assert_eq!(old, S_IREAD);
        assert_eq!(_umask_s(0x1000, &mut old), EINVAL);
        let mut fds = [0i32; 2];
        assert_eq!(_pipe(fds.as_mut_ptr(), 512, O_BINARY), -1);
        assert_eq!(errno(), EINVAL);
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 93);
    }
}
