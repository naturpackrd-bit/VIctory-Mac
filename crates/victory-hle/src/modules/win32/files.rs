//! The title's files on host folders. On the console, title storage is a
//! drive of its own and titles build paths on that assumption; here each
//! drive letter is a host folder (`mount`, `mount_at`: the runner mounts the
//! game folder as `G:` and a temporary folder as `T:`). A Windows path is
//! resolved to a host path here, and the host's file system does the rest
//! through `std::fs`; file and find handles are objects in the handle table;
//! overlapped I/O completes at once.
#![allow(non_snake_case, non_camel_case_types)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// Windows' own type names, so that signatures read as its documentation.
#![allow(clippy::upper_case_acronyms)]
// Windows' signatures, as they are.
#![allow(clippy::too_many_arguments)]

use std::any::Any;
use std::collections::VecDeque;
use std::ffi::{CStr, c_void};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use super::handles::{self, ERROR_INVALID_HANDLE, INVALID_HANDLE_VALUE, Object, set_last_error};
use super::sync::SetEvent;
use super::time::{FILETIME, file_time_of};
use crate::{HleExport, hle_export};

type HANDLE = *mut c_void;
type LPVOID = *mut c_void;
type BOOL = i32;
type DWORD = u32;
type UINT = u32;

const TRUE: BOOL = 1;
const FALSE: BOOL = 0;

/// The function does not apply to this handle or is not supported here.
pub(crate) const ERROR_INVALID_FUNCTION: u32 = 1;
/// No file of that name.
pub(crate) const ERROR_FILE_NOT_FOUND: u32 = 2;
/// A directory on the way does not exist, or the drive is not mounted.
pub(crate) const ERROR_PATH_NOT_FOUND: u32 = 3;
/// The host refused, or the file is read-only, or is a directory.
pub(crate) const ERROR_ACCESS_DENIED: u32 = 5;
/// A find handle has no more entries.
pub(crate) const ERROR_NO_MORE_FILES: u32 = 18;
/// Any other host failure.
pub(crate) const ERROR_GEN_FAILURE: u32 = 31;
/// A read at or past the end of the file.
pub(crate) const ERROR_HANDLE_EOF: u32 = 38;
/// A create that must not find the file found it.
pub(crate) const ERROR_FILE_EXISTS: u32 = 80;
/// A parameter is out of range or a required pointer is null.
pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;
/// A path does not fit the title's buffer.
pub(crate) const ERROR_BUFFER_OVERFLOW: u32 = 111;
/// A structure does not fit the title's buffer.
pub(crate) const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
/// A file pointer would move before the start of the file.
pub(crate) const ERROR_NEGATIVE_SEEK: u32 = 131;
/// A directory to remove is not empty.
pub(crate) const ERROR_DIR_NOT_EMPTY: u32 = 145;
/// The file or directory exists.
pub(crate) const ERROR_ALREADY_EXISTS: u32 = 183;
/// A path does not fit the title's buffer (`GetVolumePathNameW`).
pub(crate) const ERROR_FILENAME_EXCED_RANGE: u32 = 206;
/// A name does not fit the title's buffer.
pub(crate) const ERROR_MORE_DATA: u32 = 234;
/// A path names a file where a directory was expected.
pub(crate) const ERROR_DIRECTORY: u32 = 267;
/// No such I/O to cancel.
pub(crate) const ERROR_NOT_FOUND: u32 = 1168;

const STATUS_SUCCESS: usize = 0;
const STATUS_END_OF_FILE: usize = 0xC000_0011;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const FILE_READ_DATA: u32 = 1;
const FILE_WRITE_DATA: u32 = 2;
const FILE_APPEND_DATA: u32 = 4;

const CREATE_NEW: u32 = 1;
const CREATE_ALWAYS: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const OPEN_ALWAYS: u32 = 4;
const TRUNCATE_EXISTING: u32 = 5;

const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;

const FILE_ATTRIBUTE_READONLY: u32 = 1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

const FILE_BEGIN: u32 = 0;
const FILE_CURRENT: u32 = 1;
const FILE_END: u32 = 2;
const INVALID_SET_FILE_POINTER: u32 = u32::MAX;
const INVALID_FILE_SIZE: u32 = u32::MAX;

const FILE_TYPE_UNKNOWN: u32 = 0;
const FILE_TYPE_DISK: u32 = 1;
const FILE_TYPE_CHAR: u32 = 2;

const FILE_BASIC_INFO: i32 = 0;
const FILE_STANDARD_INFO: i32 = 1;
const FILE_NAME_INFO: i32 = 2;
const FILE_DISPOSITION_INFO: i32 = 4;
const FILE_ALLOCATION_INFO: i32 = 5;
const FILE_END_OF_FILE_INFO: i32 = 6;
const FILE_ATTRIBUTE_TAG_INFO: i32 = 9;
const FILE_ID_INFO: i32 = 18;

const MOVEFILE_REPLACE_EXISTING: u32 = 1;

const DRIVE_NO_ROOT_DIR: u32 = 1;
const DRIVE_FIXED: u32 = 3;

const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;

/// UTF-8, the code page our consoles and narrow names use.
const CP_UTF8: u32 = 65001;

/// The serial number every mounted volume reports ("VICT").
const VOLUME_SERIAL: u32 = 0x5649_4354;
const FILE_CASE_PRESERVED_NAMES: u32 = 2;
const FILE_UNICODE_ON_DISK: u32 = 4;
const FILE_SUPPORTS_SPARSE_FILES: u32 = 0x40;

/// `MAX_PATH`: the size of `GetTempFileNameW`'s buffer and of a find
/// entry's name.
const MAX_PATH: usize = 260;

/// The bytes of a cluster `GetDiskFreeSpaceW` reports: 8 sectors of 512.
const SECTORS_PER_CLUSTER: u32 = 8;
const BYTES_PER_SECTOR: u32 = 512;

/// A lock, even if a thread panicked holding it: what our locks guard is
/// consistent between our operations.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sets the last error to `code` and gives back `value`, the function's
/// failure value.
fn fail<T>(code: u32, value: T) -> T {
    set_last_error(code);
    value
}

/// TRUE, or FALSE with the error as the last error.
fn status(result: Result<(), u32>) -> BOOL {
    match result {
        Ok(()) => TRUE,
        Err(code) => fail(code, FALSE),
    }
}

fn invalid() -> HANDLE {
    INVALID_HANDLE_VALUE as HANDLE
}

// Drives and paths.

/// Mounted drives: upper-case letter, host root (absolute, no trailing
/// separator).
static DRIVES: Mutex<Vec<(char, PathBuf)>> = Mutex::new(Vec::new());

/// The title's current directory, a Windows path `X:\dir` (no trailing
/// separator but at a root); `G:\` if `G:` is mounted when first needed,
/// else the first mounted drive's root, else `C:\` (which resolves to
/// nothing).
static CURRENT_DIR: Mutex<Option<String>> = Mutex::new(None);

/// `root` absolute, without trailing separators. An empty root is the
/// current directory, as it is to a path joined to it.
fn host_root(root: &Path) -> PathBuf {
    let root = if root.as_os_str().is_empty() {
        Path::new(".")
    } else {
        root
    };
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_owned());
    // Collecting the components drops trailing separators.
    root.components().collect()
}

/// Gives `root` a drive letter (the same one if it has one): the highest
/// free letter from Z down to D.
pub fn mount(root: &Path) -> Option<char> {
    let root = host_root(root);
    let mut drives = lock(&DRIVES);
    if let Some((letter, _)) = drives.iter().find(|(_, r)| *r == root) {
        return Some(*letter);
    }
    // The console's fixed drives (`G:` the package, `T:` temporary files,
    // which `mount_at` gives) are never handed out here, mounted or not.
    let letter = ('D'..='Z')
        .rev()
        .filter(|l| !matches!(l, 'G' | 'T'))
        .find(|l| !drives.iter().any(|(m, _)| m == l))?;
    tracing::info!(drive = %letter, root = %root.display(), "virtual drive");
    drives.push((letter, root));
    Some(letter)
}

/// Gives `root` the letter the console fixes (`G:`, `T:`), replacing a
/// mount of that letter.
pub fn mount_at(letter: char, root: &Path) {
    let letter = letter.to_ascii_uppercase();
    if !letter.is_ascii_uppercase() {
        return;
    }
    let root = host_root(root);
    tracing::info!(drive = %letter, root = %root.display(), "virtual drive");
    let mut drives = lock(&DRIVES);
    drives.retain(|(l, _)| *l != letter);
    drives.push((letter, root));
}

/// Forgets every mount (tests).
// No test calls it yet: tests run at the same time over the one list of
// mounts.
#[allow(dead_code)]
pub(crate) fn unmount_all() {
    lock(&DRIVES).clear();
    *lock(&CURRENT_DIR) = None;
}

/// The host root of a mounted drive.
fn drive_root(letter: char) -> Option<PathBuf> {
    let drives = lock(&DRIVES);
    let (_, root) = drives.iter().find(|(l, _)| *l == letter)?;
    Some(root.clone())
}

/// A Windows path, parsed: its drive letter (upper-case) and its components
/// after the root.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WinPath {
    drive: char,
    parts: Vec<String>,
}

/// The drive letter (upper-case) and the rest of a path that starts `X:`.
fn drive_prefix(path: &str) -> Option<(char, &str)> {
    let mut chars = path.chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    (chars.next() == Some(':')).then(|| (letter.to_ascii_uppercase(), &path[2..]))
}

/// `base` with the components of `tail` (separated by `\`) added: `..`
/// removes one (stopping at the root), `.` and empty ones are dropped.
fn join(drive: char, mut parts: Vec<String>, tail: &str) -> WinPath {
    for part in tail.split('\\') {
        match part {
            "" | "." => {}
            ".." => drop(parts.pop()),
            _ => parts.push(part.to_owned()),
        }
    }
    WinPath { drive, parts }
}

/// The title's current directory, parsed.
fn current_dir() -> WinPath {
    let text = lock(&CURRENT_DIR).get_or_insert_with(default_dir).clone();
    match drive_prefix(&text) {
        Some((drive, tail)) => join(drive, Vec::new(), tail),
        None => WinPath {
            drive: 'C',
            parts: Vec::new(),
        },
    }
}

/// The current directory before the title sets one.
fn default_dir() -> String {
    let drives = lock(&DRIVES);
    let letter = if drives.iter().any(|(l, _)| *l == 'G') {
        'G'
    } else {
        drives.first().map_or('C', |(l, _)| *l)
    };
    format!("{letter}:\\")
}

/// A Windows path the title gave, parsed: its drive letter (upper-case) and
/// its components after the root, `..` resolved against the path itself
/// (stopping at the root), `.` and empty components dropped, `/` as `\`.
/// Accepts `X:\a\b`, `X:/a/b`, `X:` (the root), `\\?\X:\a` (the prefix
/// dropped), and a relative path (`a\b`, `.\a`, `..\a`) or a root-relative
/// one (`\a\b`) against the current directory. `None` for a UNC path
/// (`\\server\share`), a device path (`\\.\x`), or a name with a NUL inside.
fn parse(path: &str) -> Option<WinPath> {
    if path.contains('\0') {
        return None;
    }
    let path = path.replace('/', "\\");
    let path = match path.strip_prefix(r"\\?\") {
        // A verbatim path names a drive or nothing here (`\\?\UNC\..` is a
        // share).
        Some(rest) => {
            drive_prefix(rest)?;
            rest
        }
        None if path.starts_with(r"\\") => return None,
        None => &path,
    };
    // `X:` and `X:name` are relative to the drive's root.
    if let Some((drive, tail)) = drive_prefix(path) {
        return Some(join(drive, Vec::new(), tail));
    }
    let current = current_dir();
    if path.starts_with('\\') {
        return Some(join(current.drive, Vec::new(), path));
    }
    Some(join(current.drive, current.parts, path))
}

/// Whether two names are the same regardless of ASCII and Unicode case.
fn same_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

/// The host's spelling of the entry of `dir` that is `name`: the entry
/// spelt exactly so if there is one (a Linux folder may hold `c.bin` and
/// `C.BIN` both), else the one that is `name` regardless of case.
fn spelling_in(dir: &Path, name: &str) -> Option<String> {
    let names: Vec<String> = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|entry| same_name(entry, name))
        .collect();
    names
        .iter()
        .find(|entry| *entry == name)
        .or(names.first())
        .cloned()
}

/// The name of the entry of `dir` that is `name` regardless of case.
fn matching_entry(dir: &Path, name: &str) -> Option<std::ffi::OsString> {
    let entries = fs::read_dir(dir).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .find(|entry| entry.to_str().is_some_and(|entry| same_name(entry, name)))
}

/// The host path of a Windows path: the drive's root joined with the
/// components, each component matched to an existing entry regardless of
/// ASCII and Unicode case when the exact name does not exist (APFS and
/// Windows are case-insensitive, Linux is not; the title's spelling varies).
/// A component with no match keeps the title's spelling (so a create uses
/// it). `Err(ERROR_PATH_NOT_FOUND)` (3) for an unmounted drive.
fn resolve(path: &WinPath) -> Result<PathBuf, u32> {
    let mut host = drive_root(path.drive).ok_or(ERROR_PATH_NOT_FOUND)?;
    // Once a component is missing, nothing under it exists to match.
    let mut exists = true;
    for part in &path.parts {
        let exact = host.join(part);
        if exists && fs::symlink_metadata(&exact).is_err() {
            if let Some(name) = matching_entry(&host, part) {
                host.push(name);
                continue;
            }
            exists = false;
        }
        host = exact;
    }
    Ok(host)
}

/// The Windows form `X:\a\b` of a parsed path (`X:\` for a root).
fn windows_form(path: &WinPath) -> String {
    let mut out = format!("{}:", path.drive);
    if path.parts.is_empty() {
        out.push('\\');
    }
    for part in &path.parts {
        out.push('\\');
        out.push_str(part);
    }
    out
}

/// The parsed path and host path of a name the title gave. An empty or
/// unparsable name (UNC, device) is `ERROR_PATH_NOT_FOUND`, as a path on no
/// mounted drive.
fn locate(name: Option<String>) -> Result<(WinPath, PathBuf), u32> {
    let name = name.ok_or(ERROR_INVALID_PARAMETER)?;
    if name.is_empty() {
        return Err(ERROR_PATH_NOT_FOUND);
    }
    let path = parse(&name).ok_or(ERROR_PATH_NOT_FOUND)?;
    let host = resolve(&path)?;
    Ok((path, host))
}

/// The title path `path` on our drives, opened for reading by the host's
/// own means, and its host path (to open it again): what DirectStorage
/// reads its files through. The error is the Win32 code `locate` or the
/// open gives.
pub(crate) fn open_for_reading(path: &str) -> Result<(File, PathBuf), u32> {
    let (_, host) = locate(Some(path.to_owned()))?;
    let file = File::open(&host).map_err(|error| io_error(&error))?;
    Ok((file, host))
}

// Title strings.

/// A NUL-terminated UTF-16 string from the title (lossy); none for null.
fn wide(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated string.
    unsafe {
        let len = (0..).take_while(|&i| *ptr.add(i) != 0).count();
        Some(String::from_utf16_lossy(std::slice::from_raw_parts(
            ptr, len,
        )))
    }
}

/// A NUL-terminated narrow string from the title, read as UTF-8 (lossy):
/// the title's ANSI code page is not known here. None for null.
fn narrow(ptr: *const u8) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated string.
    let text = unsafe { CStr::from_ptr(ptr.cast()) };
    Some(text.to_string_lossy().into_owned())
}

/// Writes `text` and a NUL to the title's buffer of `len` u16s when it fits
/// and returns its length without the NUL; else writes nothing and returns
/// the length with the NUL, as Windows' directory functions do.
fn copy_out(text: &str, buffer: *mut u16, len: u32) -> u32 {
    let wide: Vec<u16> = text.encode_utf16().collect();
    let needed = wide.len() + 1;
    if buffer.is_null() || (len as usize) < needed {
        return needed as u32;
    }
    // SAFETY: the title's buffer holds `len` u16s, at least `needed`.
    unsafe {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), buffer, wide.len());
        *buffer.add(wide.len()) = 0;
    }
    wide.len() as u32
}

// Errors.

/// The Windows code for a host I/O error.
fn io_error(error: &io::Error) -> u32 {
    use io::ErrorKind::*;
    match error.kind() {
        NotFound => ERROR_FILE_NOT_FOUND,
        PermissionDenied | IsADirectory => ERROR_ACCESS_DENIED,
        AlreadyExists => ERROR_ALREADY_EXISTS,
        DirectoryNotEmpty => ERROR_DIR_NOT_EMPTY,
        NotADirectory => ERROR_DIRECTORY,
        InvalidInput => ERROR_INVALID_PARAMETER,
        _ => ERROR_GEN_FAILURE,
    }
}

/// See [`io_error`]; a missing `path` whose parent is missing too is
/// `ERROR_PATH_NOT_FOUND`.
fn path_error(error: &io::Error, path: &Path) -> u32 {
    let code = io_error(error);
    if code == ERROR_FILE_NOT_FOUND && path.parent().is_some_and(|parent| !parent.is_dir()) {
        return ERROR_PATH_NOT_FOUND;
    }
    code
}

// Objects.

/// Which of the host's standard streams a handle is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdStream {
    In,
    Out,
    Err,
}

/// What a file handle names.
enum Kind {
    File {
        file: File,
        /// The host path, for a delete on close.
        path: PathBuf,
        /// Whether the title asked to read, or to write (an attributes-only
        /// open asks for neither).
        can_read: bool,
        can_write: bool,
        /// Writes go to the end (`FILE_APPEND_DATA` without write access).
        append: bool,
        /// Opened with `FILE_FLAG_OVERLAPPED`: an I/O at an offset leaves
        /// the position.
        overlapped: bool,
        delete_on_close: AtomicBool,
    },
    Directory {
        path: PathBuf,
        delete_on_close: AtomicBool,
    },
    /// The `NUL` device: reads nothing, swallows writes.
    Null,
    Std(StdStream),
}

/// A file handle's object (kind "file").
struct FileObject {
    kind: Kind,
    /// The file pointer.
    position: AtomicU64,
    /// The path the title opened, in its Windows form (`X:\a\b`).
    windows_path: String,
}

impl FileObject {
    fn new(kind: Kind, windows_path: String) -> Self {
        FileObject {
            kind,
            position: AtomicU64::new(0),
            windows_path,
        }
    }

    /// Reads at `offset`, or at the position (which then moves).
    fn read(&self, buffer: &mut [u8], offset: Option<u64>) -> Result<usize, u32> {
        match &self.kind {
            Kind::File {
                file,
                can_read,
                overlapped,
                ..
            } => {
                if !can_read {
                    return Err(ERROR_ACCESS_DENIED);
                }
                let at = offset.unwrap_or_else(|| self.position.load(Ordering::Relaxed));
                let done = read_at(file, buffer, at)?;
                // A synchronous handle's position follows an I/O at an
                // offset too, as on Windows.
                if offset.is_none() || !overlapped {
                    self.position
                        .store(at.saturating_add(done as u64), Ordering::Relaxed);
                }
                Ok(done)
            }
            Kind::Null | Kind::Std(StdStream::In) => Ok(0),
            Kind::Directory { .. } | Kind::Std(_) => Err(ERROR_ACCESS_DENIED),
        }
    }

    /// Writes at `offset` (`u64::MAX`: the end), or at the position (which
    /// then moves); an append handle writes at the end.
    fn write(&self, buffer: &[u8], offset: Option<u64>) -> Result<usize, u32> {
        match &self.kind {
            Kind::File {
                file,
                can_write,
                append,
                overlapped,
                ..
            } => {
                if !can_write && !append {
                    return Err(ERROR_ACCESS_DENIED);
                }
                let at = if *append || offset == Some(u64::MAX) {
                    file.metadata().map_err(|e| io_error(&e))?.len()
                } else {
                    offset.unwrap_or_else(|| self.position.load(Ordering::Relaxed))
                };
                file.write_all_at(buffer, at).map_err(|e| io_error(&e))?;
                if *append || offset.is_none() || !overlapped {
                    let end = at.saturating_add(buffer.len() as u64);
                    self.position.store(end, Ordering::Relaxed);
                }
                Ok(buffer.len())
            }
            Kind::Null => Ok(buffer.len()),
            Kind::Std(StdStream::Out) => {
                let mut out = io::stdout().lock();
                out.write_all(buffer).map_err(|e| io_error(&e))?;
                Ok(buffer.len())
            }
            Kind::Std(StdStream::Err) => {
                io::stderr().write_all(buffer).map_err(|e| io_error(&e))?;
                Ok(buffer.len())
            }
            Kind::Directory { .. } | Kind::Std(StdStream::In) => Err(ERROR_ACCESS_DENIED),
        }
    }

    /// The host's metadata of a file or directory.
    fn metadata(&self) -> Result<Metadata, u32> {
        match &self.kind {
            Kind::File { file, .. } => file.metadata().map_err(|e| io_error(&e)),
            Kind::Directory { path, .. } => fs::metadata(path).map_err(|e| io_error(&e)),
            Kind::Null | Kind::Std(_) => Err(ERROR_INVALID_FUNCTION),
        }
    }

    /// The size the file pointer's `FILE_END` counts from (0 for devices).
    fn size(&self) -> Result<u64, u32> {
        match &self.kind {
            Kind::File { .. } | Kind::Directory { .. } => Ok(self.metadata()?.len()),
            Kind::Null | Kind::Std(_) => Ok(0),
        }
    }

    /// The delete-on-close flag of a file or directory.
    fn delete_on_close(&self) -> Option<&AtomicBool> {
        match &self.kind {
            Kind::File {
                delete_on_close, ..
            }
            | Kind::Directory {
                delete_on_close, ..
            } => Some(delete_on_close),
            Kind::Null | Kind::Std(_) => None,
        }
    }
}

impl Drop for FileObject {
    fn drop(&mut self) {
        // A failed delete leaves the file, as nothing can report it.
        match &self.kind {
            Kind::File {
                path,
                delete_on_close,
                ..
            } if delete_on_close.load(Ordering::Relaxed) => drop(fs::remove_file(path)),
            Kind::Directory {
                path,
                delete_on_close,
            } if delete_on_close.load(Ordering::Relaxed) => drop(fs::remove_dir(path)),
            _ => {}
        }
    }
}

impl Object for FileObject {
    fn kind(&self) -> &'static str {
        "file"
    }
}

/// One entry a find handle lists.
struct Entry {
    name: String,
    metadata: Option<Metadata>,
    /// `.` or `..`, a directory even if its metadata could not be read.
    is_dot: bool,
}

/// A find handle's object (kind "find"): the entries not yet returned.
struct FindObject {
    entries: Mutex<VecDeque<Entry>>,
}

impl Object for FindObject {
    fn kind(&self) -> &'static str {
        "find"
    }
}

/// Runs `body` on the object a handle names if it is a `T`; else
/// `ERROR_INVALID_HANDLE`.
fn with<T: Object, R>(handle: HANDLE, body: impl FnOnce(&T) -> Result<R, u32>) -> Result<R, u32> {
    let object = handles::get(handle as usize).ok_or(ERROR_INVALID_HANDLE)?;
    let any: &dyn Any = &*object;
    body(any.downcast_ref::<T>().ok_or(ERROR_INVALID_HANDLE)?)
}

/// See [`with`], for file handles.
fn with_file<R>(
    handle: HANDLE,
    body: impl FnOnce(&FileObject) -> Result<R, u32>,
) -> Result<R, u32> {
    with::<FileObject, R>(handle, body)
}

/// Reads at `at` until `buffer` is full or the file ends.
fn read_at(file: &File, buffer: &mut [u8], at: u64) -> Result<usize, u32> {
    let mut done = 0;
    while done < buffer.len() {
        match file.read_at(&mut buffer[done..], at.saturating_add(done as u64)) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_error(&e)),
        }
    }
    Ok(done)
}

// Opening files.

/// The device a name names: `NUL` (any case, with or without a drive or
/// directory), `CONIN$` and `CONOUT$`.
fn device(name: &str) -> Option<Kind> {
    let last = name.rsplit(['\\', '/', ':']).next().unwrap_or(name);
    if last.eq_ignore_ascii_case("NUL") {
        Some(Kind::Null)
    } else if last.eq_ignore_ascii_case("CONIN$") {
        Some(Kind::Std(StdStream::In))
    } else if last.eq_ignore_ascii_case("CONOUT$") {
        Some(Kind::Std(StdStream::Out))
    } else {
        None
    }
}

/// Whether the host file is read-only: its owner may not write it. The host
/// may let us write it anyway (as root), so this is checked here.
fn read_only(metadata: &Metadata) -> bool {
    metadata.permissions().mode() & 0o200 == 0
}

/// The object `CreateFile` opens, and whether the file existed.
fn open_object(
    name: Option<String>,
    access: u32,
    disposition: u32,
    flags: u32,
) -> Result<(FileObject, bool), u32> {
    let name = name.ok_or(ERROR_PATH_NOT_FOUND)?;
    if let Some(kind) = device(&name) {
        return Ok((FileObject::new(kind, name), false));
    }
    let (path, host) = locate(Some(name))?;
    let windows_path = windows_form(&path);
    let can_read = access & (GENERIC_READ | FILE_READ_DATA | GENERIC_ALL) != 0;
    let can_write = access & (GENERIC_WRITE | FILE_WRITE_DATA | GENERIC_ALL) != 0;
    let append = !can_write && access & FILE_APPEND_DATA != 0;
    let delete_on_close = AtomicBool::new(flags & FILE_FLAG_DELETE_ON_CLOSE != 0);
    let existing = fs::metadata(&host).ok();
    let existed = existing.is_some();
    if let Some(metadata) = &existing {
        if metadata.is_dir() {
            if flags & FILE_FLAG_BACKUP_SEMANTICS == 0 {
                return Err(ERROR_ACCESS_DENIED);
            }
            return match disposition {
                OPEN_EXISTING | OPEN_ALWAYS => {
                    let kind = Kind::Directory {
                        path: host,
                        delete_on_close,
                    };
                    Ok((FileObject::new(kind, windows_path), true))
                }
                CREATE_NEW => Err(ERROR_FILE_EXISTS),
                CREATE_ALWAYS | TRUNCATE_EXISTING => Err(ERROR_ACCESS_DENIED),
                _ => Err(ERROR_INVALID_PARAMETER),
            };
        }
        if disposition == CREATE_NEW {
            return Err(ERROR_FILE_EXISTS);
        }
        let truncates = matches!(disposition, CREATE_ALWAYS | TRUNCATE_EXISTING);
        if (can_write || append || truncates) && read_only(metadata) {
            return Err(ERROR_ACCESS_DENIED);
        }
    }
    let creates = match disposition {
        CREATE_NEW | CREATE_ALWAYS | TRUNCATE_EXISTING => true,
        OPEN_ALWAYS => !existed,
        OPEN_EXISTING => false,
        _ => return Err(ERROR_INVALID_PARAMETER),
    };
    // The host needs write access to create or truncate, whatever the title
    // asked for; what the title may do is checked against its own access.
    let host_write = can_write || append || creates;
    let mut options = OpenOptions::new();
    options.read(can_read || !host_write).write(host_write);
    match disposition {
        CREATE_NEW => options.create_new(true),
        CREATE_ALWAYS => options.create(true).truncate(true),
        OPEN_ALWAYS if !existed => options.create(true).truncate(false),
        TRUNCATE_EXISTING => options.truncate(true),
        _ => &mut options,
    };
    let file = options.open(&host).map_err(|e| path_error(&e, &host))?;
    let kind = Kind::File {
        file,
        path: host,
        can_read,
        can_write,
        append,
        overlapped: flags & FILE_FLAG_OVERLAPPED != 0,
        delete_on_close,
    };
    Ok((FileObject::new(kind, windows_path), existed))
}

/// `CreateFile`'s body: a handle, or `INVALID_HANDLE_VALUE` and the error.
fn create_file(name: Option<String>, access: u32, disposition: u32, flags: u32) -> HANDLE {
    match open_object(name, access, disposition, flags) {
        Ok((object, existed)) => {
            let reopened = existed && matches!(disposition, CREATE_ALWAYS | OPEN_ALWAYS);
            set_last_error(if reopened { ERROR_ALREADY_EXISTS } else { 0 });
            handles::insert(Arc::new(object)) as HANDLE
        }
        Err(code) => fail(code, invalid()),
    }
}

/// Opens or creates a file, a directory (with `FILE_FLAG_BACKUP_SEMANTICS`)
/// or a device (`NUL`, `CONIN$`, `CONOUT$`). Sharing is not enforced; the
/// buffering and access-pattern flags and the attributes are ignored.
pub extern "win64" fn CreateFileW(
    name: *const u16,
    access: DWORD,
    _share: DWORD,
    _security: *const c_void,
    disposition: DWORD,
    flags: DWORD,
    _template: HANDLE,
) -> HANDLE {
    create_file(wide(name), access, disposition, flags)
}

/// See [`CreateFileW`].
pub extern "win64" fn CreateFileA(
    name: *const u8,
    access: DWORD,
    _share: DWORD,
    _security: *const c_void,
    disposition: DWORD,
    flags: DWORD,
    _template: HANDLE,
) -> HANDLE {
    create_file(narrow(name), access, disposition, flags)
}

/// `CreateFile2`'s optional parameters.
#[repr(C)]
pub struct CREATEFILE2_EXTENDED_PARAMETERS {
    pub size: u32,
    pub attributes: u32,
    pub flags: u32,
    pub security_qos: u32,
    pub security: *const c_void,
    pub template: HANDLE,
}

const _: () = assert!(std::mem::size_of::<CREATEFILE2_EXTENDED_PARAMETERS>() == 32);

/// See [`CreateFileW`]; the flags are in `params`, which may be null.
pub extern "win64" fn CreateFile2(
    name: *const u16,
    access: DWORD,
    _share: DWORD,
    disposition: DWORD,
    params: *const CREATEFILE2_EXTENDED_PARAMETERS,
) -> HANDLE {
    let flags = if params.is_null() {
        0
    } else {
        // SAFETY: checked non-null; the title passes its parameters.
        unsafe { (*params).flags }
    };
    create_file(wide(name), access, disposition, flags)
}

// Reading and writing.

/// An asynchronous I/O's place in the file and its result.
#[repr(C)]
pub struct OVERLAPPED {
    /// The status: `STATUS_SUCCESS`, or `STATUS_END_OF_FILE`.
    pub internal: usize,
    /// The bytes moved.
    pub internal_high: usize,
    pub offset: u32,
    pub offset_high: u32,
    /// Signaled when the I/O completes.
    pub event: HANDLE,
}

const _: () = assert!(std::mem::size_of::<OVERLAPPED>() == 32);

/// The routine a `ReadFileEx` or `WriteFileEx` completion calls.
type CompletionRoutine = Option<extern "win64" fn(DWORD, DWORD, *mut OVERLAPPED)>;

/// What a read or write did: the bytes moved, and whether a read found the
/// end.
struct Done {
    bytes: u32,
    end: bool,
}

/// Reads or writes `len` bytes of the title's `buffer`: at the offset in
/// `overlapped` when there is one (which then reports the result and has
/// its event signaled: the I/O is complete), else at the position.
fn transfer(
    handle: HANDLE,
    buffer: *mut u8,
    len: u32,
    overlapped: *mut OVERLAPPED,
    write: bool,
) -> Result<Done, u32> {
    if buffer.is_null() && len != 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let offset = (!overlapped.is_null()).then(|| {
        // SAFETY: checked non-null; the title passes its OVERLAPPED.
        let (low, high) = unsafe { ((*overlapped).offset, (*overlapped).offset_high) };
        u64::from(high) << 32 | u64::from(low)
    });
    let bytes = with_file(handle, |object| {
        if len == 0 {
            return if write {
                object.write(&[], offset)
            } else {
                object.read(&mut [], offset)
            };
        }
        if write {
            // SAFETY: checked non-null; the title's buffer holds `len` bytes.
            let data = unsafe { std::slice::from_raw_parts(buffer, len as usize) };
            object.write(data, offset)
        } else {
            // SAFETY: checked non-null; the title's buffer holds `len` bytes.
            let data = unsafe { std::slice::from_raw_parts_mut(buffer, len as usize) };
            object.read(data, offset)
        }
    })? as u32;
    let end = !write && len != 0 && bytes == 0;
    if !overlapped.is_null() {
        // SAFETY: checked non-null; the title's OVERLAPPED.
        let event = unsafe {
            (*overlapped).internal = if end {
                STATUS_END_OF_FILE
            } else {
                STATUS_SUCCESS
            };
            (*overlapped).internal_high = bytes as usize;
            (*overlapped).event
        };
        if !event.is_null() {
            SetEvent(event);
        }
    }
    Ok(Done { bytes, end })
}

/// `ReadFile` and `WriteFile`'s body.
fn read_or_write(
    handle: HANDLE,
    buffer: *mut u8,
    len: u32,
    moved: *mut DWORD,
    overlapped: *mut OVERLAPPED,
    write: bool,
) -> BOOL {
    if !moved.is_null() {
        // SAFETY: checked non-null; the title's count.
        unsafe { *moved = 0 };
    }
    if overlapped.is_null() && moved.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    match transfer(handle, buffer, len, overlapped, write) {
        Ok(done) => {
            if !moved.is_null() {
                // SAFETY: checked non-null; the title's count.
                unsafe { *moved = done.bytes };
            }
            // A completed overlapped read past the end fails, as Windows
            // reports it; a synchronous one reads 0 bytes.
            if done.end && !overlapped.is_null() {
                return fail(ERROR_HANDLE_EOF, FALSE);
            }
            TRUE
        }
        Err(code) => fail(code, FALSE),
    }
}

/// Reads from a file, at `overlapped`'s offset when given (completed at
/// once), else at the file pointer.
pub extern "win64" fn ReadFile(
    handle: HANDLE,
    buffer: *mut c_void,
    to_read: DWORD,
    read: *mut DWORD,
    overlapped: *mut OVERLAPPED,
) -> BOOL {
    read_or_write(handle, buffer.cast(), to_read, read, overlapped, false)
}

/// Writes to a file, at `overlapped`'s offset when given (completed at
/// once), else at the file pointer; `NUL` swallows it, the console streams
/// go to the host's.
pub extern "win64" fn WriteFile(
    handle: HANDLE,
    buffer: *const c_void,
    to_write: DWORD,
    written: *mut DWORD,
    overlapped: *mut OVERLAPPED,
) -> BOOL {
    read_or_write(
        handle,
        buffer as *mut u8,
        to_write,
        written,
        overlapped,
        true,
    )
}

/// `ReadFileEx` and `WriteFileEx`'s body: the I/O, then the completion
/// routine at once (Windows queues it as an APC for the thread's next
/// alertable wait: there are no APCs here). A read at the end completes with
/// `ERROR_HANDLE_EOF`; an I/O that fails to start calls nothing.
fn io_ex(
    handle: HANDLE,
    buffer: *mut u8,
    len: u32,
    overlapped: *mut OVERLAPPED,
    completion: CompletionRoutine,
    write: bool,
) -> BOOL {
    if overlapped.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    match transfer(handle, buffer, len, overlapped, write) {
        Ok(done) => {
            if let Some(completion) = completion {
                let code = if done.end { ERROR_HANDLE_EOF } else { 0 };
                completion(code, done.bytes, overlapped);
            }
            TRUE
        }
        Err(code) => fail(code, FALSE),
    }
}

/// See [`ReadFile`] and [`io_ex`].
pub extern "win64" fn ReadFileEx(
    handle: HANDLE,
    buffer: *mut c_void,
    to_read: DWORD,
    overlapped: *mut OVERLAPPED,
    completion: CompletionRoutine,
) -> BOOL {
    io_ex(
        handle,
        buffer.cast(),
        to_read,
        overlapped,
        completion,
        false,
    )
}

/// See [`WriteFile`] and [`io_ex`].
pub extern "win64" fn WriteFileEx(
    handle: HANDLE,
    buffer: *const c_void,
    to_write: DWORD,
    overlapped: *mut OVERLAPPED,
    completion: CompletionRoutine,
) -> BOOL {
    io_ex(
        handle,
        buffer as *mut u8,
        to_write,
        overlapped,
        completion,
        true,
    )
}

/// Moves the file pointer by `distance` from `method`'s base; the new
/// position.
fn seek(handle: HANDLE, distance: i64, method: u32) -> Result<u64, u32> {
    with_file(handle, |object| {
        let base = match method {
            FILE_BEGIN => 0,
            FILE_CURRENT => object.position.load(Ordering::Relaxed),
            FILE_END => object.size()?,
            _ => return Err(ERROR_INVALID_PARAMETER),
        };
        let position = i64::try_from(base)
            .ok()
            .and_then(|base| base.checked_add(distance))
            .ok_or(ERROR_INVALID_PARAMETER)?;
        let position = u64::try_from(position).map_err(|_| ERROR_NEGATIVE_SEEK)?;
        object.position.store(position, Ordering::Relaxed);
        Ok(position)
    })
}

/// Moves the file pointer; the 64-bit distance is `distance` and
/// `*distance_high` when that is given, which then receives the high part
/// of the position. The low part, or `INVALID_SET_FILE_POINTER`.
pub extern "win64" fn SetFilePointer(
    handle: HANDLE,
    distance: i32,
    distance_high: *mut i32,
    method: DWORD,
) -> DWORD {
    let distance = if distance_high.is_null() {
        i64::from(distance)
    } else {
        // SAFETY: checked non-null; the title's high part.
        let high = unsafe { *distance_high };
        i64::from(high) << 32 | i64::from(distance as u32)
    };
    match seek(handle, distance, method) {
        Ok(position) => {
            if !distance_high.is_null() {
                // SAFETY: checked non-null.
                unsafe { *distance_high = (position >> 32) as i32 };
            }
            // A low part of 0xFFFFFFFF is told from a failure by this.
            set_last_error(0);
            position as u32
        }
        Err(code) => fail(code, INVALID_SET_FILE_POINTER),
    }
}

/// Moves the file pointer; the new position to `new_position` if given.
pub extern "win64" fn SetFilePointerEx(
    handle: HANDLE,
    distance: i64,
    new_position: *mut i64,
    method: DWORD,
) -> BOOL {
    status(seek(handle, distance, method).map(|position| {
        if !new_position.is_null() {
            // SAFETY: checked non-null; the title's output.
            unsafe { new_position.write_unaligned(position as i64) };
        }
    }))
}

/// The file's size: the low part, the high part to `high` if given.
pub extern "win64" fn GetFileSize(handle: HANDLE, high: *mut DWORD) -> DWORD {
    match with_file(handle, FileObject::size) {
        Ok(size) => {
            if !high.is_null() {
                // SAFETY: checked non-null; the title's output.
                unsafe { *high = (size >> 32) as u32 };
            }
            set_last_error(0);
            size as u32
        }
        Err(code) => fail(code, INVALID_FILE_SIZE),
    }
}

/// The file's size.
pub extern "win64" fn GetFileSizeEx(handle: HANDLE, size: *mut i64) -> BOOL {
    if size.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    status(with_file(handle, FileObject::size).map(|value| {
        // SAFETY: checked non-null; the title's output.
        unsafe { size.write_unaligned(value as i64) };
    }))
}

/// A disk file (files and directories) or a character device (`NUL`, the
/// console streams).
pub extern "win64" fn GetFileType(handle: HANDLE) -> DWORD {
    let kind = with_file(handle, |object| {
        Ok(match object.kind {
            Kind::File { .. } | Kind::Directory { .. } => FILE_TYPE_DISK,
            Kind::Null | Kind::Std(_) => FILE_TYPE_CHAR,
        })
    });
    kind.unwrap_or_else(|code| fail(code, FILE_TYPE_UNKNOWN))
}

/// Ends the file at the file pointer.
pub extern "win64" fn SetEndOfFile(handle: HANDLE) -> BOOL {
    status(with_file(handle, |object| match &object.kind {
        Kind::File {
            file,
            can_write,
            append,
            ..
        } => {
            if !can_write && !append {
                return Err(ERROR_ACCESS_DENIED);
            }
            let position = object.position.load(Ordering::Relaxed);
            file.set_len(position).map_err(|e| io_error(&e))
        }
        Kind::Null => Ok(()),
        Kind::Directory { .. } => Err(ERROR_ACCESS_DENIED),
        Kind::Std(_) => Err(ERROR_INVALID_FUNCTION),
    }))
}

/// Writes the file's data to the host disk (the console streams: flushes
/// them).
pub extern "win64" fn FlushFileBuffers(handle: HANDLE) -> BOOL {
    status(with_file(handle, |object| match &object.kind {
        Kind::File { file, .. } => file.sync_all().map_err(|e| io_error(&e)),
        Kind::Std(StdStream::Out) => io::stdout().flush().map_err(|e| io_error(&e)),
        Kind::Std(StdStream::Err) => io::stderr().flush().map_err(|e| io_error(&e)),
        _ => Ok(()),
    }))
}

// Information.

/// A file's Windows attributes from its host metadata.
fn attributes(metadata: &Metadata) -> u32 {
    let mut attributes = 0;
    if metadata.is_dir() {
        attributes |= FILE_ATTRIBUTE_DIRECTORY;
    } else {
        attributes |= FILE_ATTRIBUTE_ARCHIVE;
    }
    if read_only(metadata) {
        attributes |= FILE_ATTRIBUTE_READONLY;
    }
    if attributes == 0 {
        attributes = FILE_ATTRIBUTE_NORMAL;
    }
    attributes
}

/// Creation (the last write where the host keeps none), last access and
/// last write times.
fn times(metadata: &Metadata) -> [FILETIME; 3] {
    let write = metadata.modified().unwrap_or(UNIX_EPOCH);
    let access = metadata.accessed().unwrap_or(write);
    let creation = metadata.created().unwrap_or(write);
    [
        file_time_of(creation),
        file_time_of(access),
        file_time_of(write),
    ]
}

/// A file time as the 64-bit count `FILE_BASIC_INFO` holds.
fn time_value(time: FILETIME) -> i64 {
    (u64::from(time.high) << 32 | u64::from(time.low)) as i64
}

/// The host's change time (inode change) as a file time.
fn change_time(metadata: &Metadata) -> FILETIME {
    let seconds = u64::try_from(metadata.ctime()).unwrap_or(0);
    let nanos = u32::try_from(metadata.ctime_nsec()).unwrap_or(0);
    let time = UNIX_EPOCH.checked_add(std::time::Duration::new(seconds, nanos));
    file_time_of(time.unwrap_or(UNIX_EPOCH))
}

/// What `GetFileInformationByHandle` returns.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct BY_HANDLE_FILE_INFORMATION {
    pub attributes: u32,
    pub creation: FILETIME,
    pub last_access: FILETIME,
    pub last_write: FILETIME,
    pub volume_serial: u32,
    pub size_high: u32,
    pub size_low: u32,
    pub links: u32,
    pub index_high: u32,
    pub index_low: u32,
}

const _: () = assert!(std::mem::size_of::<BY_HANDLE_FILE_INFORMATION>() == 52);

/// A file's attributes, times, size, link count and index (the host inode).
pub extern "win64" fn GetFileInformationByHandle(
    handle: HANDLE,
    info: *mut BY_HANDLE_FILE_INFORMATION,
) -> BOOL {
    if info.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    status(with_file(handle, |object| {
        let metadata = object.metadata()?;
        let [creation, last_access, last_write] = times(&metadata);
        let size = metadata.len();
        let index = metadata.ino();
        let value = BY_HANDLE_FILE_INFORMATION {
            attributes: attributes(&metadata),
            creation,
            last_access,
            last_write,
            volume_serial: VOLUME_SERIAL,
            size_high: (size >> 32) as u32,
            size_low: size as u32,
            links: u32::try_from(metadata.nlink()).unwrap_or(u32::MAX),
            index_high: (index >> 32) as u32,
            index_low: index as u32,
        };
        // SAFETY: checked non-null; the title's output.
        unsafe { info.write_unaligned(value) };
        Ok(())
    }))
}

#[repr(C)]
struct FILE_BASIC_INFO_DATA {
    creation: i64,
    last_access: i64,
    last_write: i64,
    change: i64,
    attributes: u32,
}

const _: () = assert!(std::mem::size_of::<FILE_BASIC_INFO_DATA>() == 40);

#[repr(C)]
struct FILE_STANDARD_INFO_DATA {
    allocation: i64,
    end_of_file: i64,
    links: u32,
    delete_pending: u8,
    directory: u8,
}

const _: () = assert!(std::mem::size_of::<FILE_STANDARD_INFO_DATA>() == 24);

#[repr(C)]
struct FILE_ATTRIBUTE_TAG_INFO_DATA {
    attributes: u32,
    reparse_tag: u32,
}

#[repr(C)]
struct FILE_ID_INFO_DATA {
    volume_serial: u64,
    file_id: [u8; 16],
}

const _: () = assert!(std::mem::size_of::<FILE_ID_INFO_DATA>() == 24);

/// Writes `value` to the title's buffer of `size` bytes if it fits, else
/// `ERROR_INSUFFICIENT_BUFFER`.
fn put<T>(info: *mut c_void, size: u32, value: T) -> Result<(), u32> {
    if (size as usize) < std::mem::size_of::<T>() {
        return Err(ERROR_INSUFFICIENT_BUFFER);
    }
    // SAFETY: the caller checked `info` non-null; it holds `size` bytes,
    // enough for a `T`.
    unsafe { info.cast::<T>().write_unaligned(value) };
    Ok(())
}

/// `FileNameInfo`: the path without the drive (`\dir\file`), its length in
/// bytes first; `ERROR_MORE_DATA` (and as much as fits) when it does not
/// fit.
fn put_name(object: &FileObject, info: *mut c_void, size: u32) -> Result<(), u32> {
    if size < 8 {
        return Err(ERROR_INSUFFICIENT_BUFFER);
    }
    let name: Vec<u16> = object
        .windows_path
        .get(2..)
        .unwrap_or_default()
        .encode_utf16()
        .collect();
    let bytes = name.len() * 2;
    let room = (size as usize - 4) / 2;
    let fits = name.len().min(room);
    // SAFETY: the caller checked `info` non-null; it holds `size` bytes: the
    // length and `room` u16s.
    unsafe {
        info.cast::<u32>().write_unaligned(bytes as u32);
        let out = info.cast::<u8>().add(4).cast::<u16>();
        for (i, c) in name[..fits].iter().enumerate() {
            out.add(i).write_unaligned(*c);
        }
    }
    if fits < name.len() {
        return Err(ERROR_MORE_DATA);
    }
    Ok(())
}

/// Information of the `class` asked for: basic, standard, name, attribute
/// tag and id.
pub extern "win64" fn GetFileInformationByHandleEx(
    handle: HANDLE,
    class: i32,
    info: *mut c_void,
    size: DWORD,
) -> BOOL {
    if info.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    status(with_file(handle, |object| {
        if class == FILE_NAME_INFO {
            return put_name(object, info, size);
        }
        if !matches!(
            class,
            FILE_BASIC_INFO | FILE_STANDARD_INFO | FILE_ATTRIBUTE_TAG_INFO | FILE_ID_INFO
        ) {
            return Err(ERROR_INVALID_PARAMETER);
        }
        let metadata = object.metadata()?;
        match class {
            FILE_BASIC_INFO => {
                let [creation, last_access, last_write] = times(&metadata);
                let value = FILE_BASIC_INFO_DATA {
                    creation: time_value(creation),
                    last_access: time_value(last_access),
                    last_write: time_value(last_write),
                    change: time_value(change_time(&metadata)),
                    attributes: attributes(&metadata),
                };
                put(info, size, value)
            }
            FILE_STANDARD_INFO => {
                let delete_pending = object
                    .delete_on_close()
                    .is_some_and(|flag| flag.load(Ordering::Relaxed));
                let value = FILE_STANDARD_INFO_DATA {
                    allocation: metadata.blocks().saturating_mul(512) as i64,
                    end_of_file: metadata.len() as i64,
                    links: u32::try_from(metadata.nlink()).unwrap_or(u32::MAX),
                    delete_pending: u8::from(delete_pending),
                    directory: u8::from(metadata.is_dir()),
                };
                put(info, size, value)
            }
            FILE_ATTRIBUTE_TAG_INFO => {
                let value = FILE_ATTRIBUTE_TAG_INFO_DATA {
                    attributes: attributes(&metadata),
                    reparse_tag: 0,
                };
                put(info, size, value)
            }
            _ => {
                let mut file_id = [0; 16];
                file_id[..8].copy_from_slice(&metadata.ino().to_le_bytes());
                let value = FILE_ID_INFO_DATA {
                    volume_serial: u64::from(VOLUME_SERIAL),
                    file_id,
                };
                put(info, size, value)
            }
        }
    }))
}

/// Sets the end of file, or the delete-on-close flag; the allocation size
/// and the basic information (times, attributes) are accepted and not set.
pub extern "win64" fn SetFileInformationByHandle(
    handle: HANDLE,
    class: i32,
    info: *const c_void,
    size: DWORD,
) -> BOOL {
    if info.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    status(with_file(handle, |object| match class {
        FILE_END_OF_FILE_INFO => {
            if size < 8 {
                return Err(ERROR_INSUFFICIENT_BUFFER);
            }
            // SAFETY: checked non-null and large enough; the title's value.
            let end = unsafe { info.cast::<i64>().read_unaligned() };
            let end = u64::try_from(end).map_err(|_| ERROR_INVALID_PARAMETER)?;
            match &object.kind {
                Kind::File {
                    file,
                    can_write,
                    append,
                    ..
                } => {
                    if !can_write && !append {
                        return Err(ERROR_ACCESS_DENIED);
                    }
                    file.set_len(end).map_err(|e| io_error(&e))
                }
                _ => Err(ERROR_INVALID_PARAMETER),
            }
        }
        FILE_DISPOSITION_INFO => {
            if size < 1 {
                return Err(ERROR_INSUFFICIENT_BUFFER);
            }
            // SAFETY: checked non-null and large enough; the title's flag.
            let delete = unsafe { info.cast::<u8>().read() } != 0;
            let flag = object.delete_on_close().ok_or(ERROR_INVALID_PARAMETER)?;
            flag.store(delete, Ordering::Relaxed);
            Ok(())
        }
        FILE_ALLOCATION_INFO | FILE_BASIC_INFO => Ok(()),
        _ => Err(ERROR_INVALID_PARAMETER),
    }))
}

/// The file's creation, last access and last write times, to the outputs
/// that are given.
pub extern "win64" fn GetFileTime(
    handle: HANDLE,
    creation: *mut FILETIME,
    access: *mut FILETIME,
    write: *mut FILETIME,
) -> BOOL {
    status(with_file(handle, |object| {
        let metadata = object.metadata()?;
        for (out, time) in [creation, access, write].into_iter().zip(times(&metadata)) {
            if !out.is_null() {
                // SAFETY: checked non-null; the title's output.
                unsafe { out.write_unaligned(time) };
            }
        }
        Ok(())
    }))
}

/// Accepted; the host's times are not set.
pub extern "win64" fn SetFileTime(
    handle: HANDLE,
    _creation: *const FILETIME,
    _access: *const FILETIME,
    _write: *const FILETIME,
) -> BOOL {
    status(with_file(handle, |_| Ok(())))
}

/// `\\?\X:\a\b`, the path the handle was opened by, whatever `flags` asks
/// for (`VOLUME_NAME_DOS`, `FILE_NAME_OPENED`; the other volume forms get
/// the same).
pub extern "win64" fn GetFinalPathNameByHandleW(
    handle: HANDLE,
    buffer: *mut u16,
    len: DWORD,
    _flags: DWORD,
) -> DWORD {
    let path = with_file(handle, |object| match object.kind {
        Kind::File { .. } | Kind::Directory { .. } => Ok(format!(r"\\?\{}", object.windows_path)),
        Kind::Null | Kind::Std(_) => Err(ERROR_INVALID_FUNCTION),
    });
    match path {
        Ok(path) => copy_out(&path, buffer, len),
        Err(code) => fail(code, 0),
    }
}

/// The host metadata of a name the title gave.
fn metadata_of(name: Option<String>) -> Result<Metadata, u32> {
    let (_, host) = locate(name)?;
    fs::metadata(&host).map_err(|e| path_error(&e, &host))
}

/// `GetFileAttributes`' body.
fn file_attributes(name: Option<String>) -> DWORD {
    match metadata_of(name) {
        Ok(metadata) => attributes(&metadata),
        Err(code) => fail(code, INVALID_FILE_ATTRIBUTES),
    }
}

/// A file's attributes: directory, read-only (the owner may not write it),
/// archive (a file).
pub extern "win64" fn GetFileAttributesW(name: *const u16) -> DWORD {
    file_attributes(wide(name))
}

/// See [`GetFileAttributesW`].
pub extern "win64" fn GetFileAttributesA(name: *const u8) -> DWORD {
    file_attributes(narrow(name))
}

/// What `GetFileAttributesExW` returns.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct WIN32_FILE_ATTRIBUTE_DATA {
    pub attributes: u32,
    pub creation: FILETIME,
    pub last_access: FILETIME,
    pub last_write: FILETIME,
    pub size_high: u32,
    pub size_low: u32,
}

const _: () = assert!(std::mem::size_of::<WIN32_FILE_ATTRIBUTE_DATA>() == 36);

/// A file's attributes, times and size (`GetFileExInfoStandard`, 0).
pub extern "win64" fn GetFileAttributesExW(
    name: *const u16,
    level: i32,
    out: *mut WIN32_FILE_ATTRIBUTE_DATA,
) -> BOOL {
    if level != 0 || out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    status(metadata_of(wide(name)).map(|metadata| {
        let [creation, last_access, last_write] = times(&metadata);
        let size = metadata.len();
        let value = WIN32_FILE_ATTRIBUTE_DATA {
            attributes: attributes(&metadata),
            creation,
            last_access,
            last_write,
            size_high: (size >> 32) as u32,
            size_low: size as u32,
        };
        // SAFETY: checked non-null; the title's output.
        unsafe { out.write_unaligned(value) };
    }))
}

/// `FILE_ATTRIBUTE_READONLY` sets or clears the owner's write bit; the
/// other attributes are accepted and not kept.
pub extern "win64" fn SetFileAttributesW(name: *const u16, attributes: DWORD) -> BOOL {
    status(locate(wide(name)).and_then(|(_, host)| {
        let metadata = fs::metadata(&host).map_err(|e| path_error(&e, &host))?;
        let mut permissions = metadata.permissions();
        let mode = permissions.mode();
        let mode = if attributes & FILE_ATTRIBUTE_READONLY != 0 {
            mode & !0o200
        } else {
            mode | 0o200
        };
        if mode != permissions.mode() {
            permissions.set_mode(mode);
            fs::set_permissions(&host, permissions).map_err(|e| io_error(&e))?;
        }
        Ok(())
    }))
}

/// `DeleteFile`'s body: a directory or a read-only file is refused.
fn delete_file(name: Option<String>) -> BOOL {
    status(locate(name).and_then(|(_, host)| {
        let metadata = fs::symlink_metadata(&host).map_err(|e| path_error(&e, &host))?;
        if metadata.is_dir() || read_only(&metadata) {
            return Err(ERROR_ACCESS_DENIED);
        }
        fs::remove_file(&host).map_err(|e| path_error(&e, &host))
    }))
}

/// Deletes a file.
pub extern "win64" fn DeleteFileW(name: *const u16) -> BOOL {
    delete_file(wide(name))
}

/// See [`DeleteFileW`].
pub extern "win64" fn DeleteFileA(name: *const u8) -> BOOL {
    delete_file(narrow(name))
}

/// Removes an empty directory.
pub extern "win64" fn RemoveDirectoryW(name: *const u16) -> BOOL {
    status(
        locate(wide(name))
            .and_then(|(_, host)| fs::remove_dir(&host).map_err(|e| path_error(&e, &host))),
    )
}

/// `CreateDirectory`'s body.
fn create_directory(name: Option<String>) -> BOOL {
    status(
        locate(name).and_then(|(_, host)| fs::create_dir(&host).map_err(|e| path_error(&e, &host))),
    )
}

/// Creates a directory (its parent must exist).
pub extern "win64" fn CreateDirectoryW(name: *const u16, _security: *const c_void) -> BOOL {
    create_directory(wide(name))
}

/// See [`CreateDirectoryW`].
pub extern "win64" fn CreateDirectoryA(name: *const u8, _security: *const c_void) -> BOOL {
    create_directory(narrow(name))
}

/// Renames a file or directory; an existing target is replaced only with
/// `MOVEFILE_REPLACE_EXISTING`, and only when it is not a directory.
/// Moving between drives works only where the host can rename between
/// their folders (`MOVEFILE_COPY_ALLOWED` is not honored).
pub extern "win64" fn MoveFileExW(from: *const u16, to: *const u16, flags: DWORD) -> BOOL {
    let result = locate(wide(from)).and_then(|(_, source)| {
        let (_, target) = locate(wide(to))?;
        fs::symlink_metadata(&source).map_err(|e| path_error(&e, &source))?;
        if let Ok(existing) = fs::metadata(&target) {
            if flags & MOVEFILE_REPLACE_EXISTING == 0 {
                return Err(ERROR_ALREADY_EXISTS);
            }
            if existing.is_dir() {
                return Err(ERROR_ACCESS_DENIED);
            }
        }
        fs::rename(&source, &target).map_err(|e| path_error(&e, &target))
    });
    status(result)
}

/// See [`MoveFileExW`], without replacing.
pub extern "win64" fn MoveFileW(from: *const u16, to: *const u16) -> BOOL {
    MoveFileExW(from, to, 0)
}

/// Copies a file; an existing target is replaced unless `fail_if_exists`.
pub extern "win64" fn CopyFileW(from: *const u16, to: *const u16, fail_if_exists: BOOL) -> BOOL {
    let result = locate(wide(from)).and_then(|(_, source)| {
        let (_, target) = locate(wide(to))?;
        if fail_if_exists != 0 && fs::symlink_metadata(&target).is_ok() {
            return Err(ERROR_FILE_EXISTS);
        }
        let metadata = fs::metadata(&source).map_err(|e| path_error(&e, &source))?;
        if metadata.is_dir() {
            return Err(ERROR_ACCESS_DENIED);
        }
        fs::copy(&source, &target)
            .map(drop)
            .map_err(|e| path_error(&e, &target))
    });
    status(result)
}

/// The full Windows form of a path, against the current directory; nothing
/// is looked up on disk. `file_part` (if given) points at the last
/// component in the buffer, or is null for a root.
pub extern "win64" fn GetFullPathNameW(
    name: *const u16,
    len: DWORD,
    buffer: *mut u16,
    file_part: *mut *mut u16,
) -> DWORD {
    let Some(name) = wide(name) else {
        return fail(ERROR_INVALID_PARAMETER, 0);
    };
    let Some(path) = parse(&name).filter(|_| !name.is_empty()) else {
        return fail(ERROR_PATH_NOT_FOUND, 0);
    };
    let text = windows_form(&path);
    let written = copy_out(&text, buffer, len);
    let length = text.encode_utf16().count() as u32;
    if written == length && !file_part.is_null() {
        let part = path.parts.last().map_or(std::ptr::null_mut(), |last| {
            let start = length as usize - last.encode_utf16().count();
            // SAFETY: within the buffer just written.
            unsafe { buffer.add(start) }
        });
        // SAFETY: checked non-null; the title's output.
        unsafe { *file_part = part };
    }
    written
}

/// The title's current directory.
pub extern "win64" fn GetCurrentDirectoryW(len: DWORD, buffer: *mut u16) -> DWORD {
    copy_out(&windows_form(&current_dir()), buffer, len)
}

/// `SetCurrentDirectory`'s body: the path must be an existing directory.
fn set_current_directory(name: Option<String>) -> BOOL {
    status(locate(name).and_then(|(path, host)| {
        if !host.is_dir() {
            return Err(ERROR_PATH_NOT_FOUND);
        }
        *lock(&CURRENT_DIR) = Some(windows_form(&path));
        Ok(())
    }))
}

/// Sets the title's current directory.
pub extern "win64" fn SetCurrentDirectoryW(name: *const u16) -> BOOL {
    set_current_directory(wide(name))
}

/// See [`SetCurrentDirectoryW`].
pub extern "win64" fn SetCurrentDirectoryA(name: *const u8) -> BOOL {
    set_current_directory(narrow(name))
}

/// `T:\` when `T:` is mounted, else the current directory's drive root,
/// with the trailing backslash.
pub extern "win64" fn GetTempPathW(len: DWORD, buffer: *mut u16) -> DWORD {
    let letter = if drive_root('T').is_some() {
        'T'
    } else {
        current_dir().drive
    };
    copy_out(&format!("{letter}:\\"), buffer, len)
}

/// Mixed into the numbers `GetTempFileNameW` makes up.
static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

/// `path\PREuuuu.TMP` to `out` (MAX_PATH u16s): `PRE` the first three
/// characters of `prefix`, `uuuu` the hex of `unique`, or with `unique` 0 a
/// fresh number for which the file is created empty here. The number, 0 on
/// failure.
pub extern "win64" fn GetTempFileNameW(
    path: *const u16,
    prefix: *const u16,
    unique: UINT,
    out: *mut u16,
) -> UINT {
    if out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    let (dir, host) = match locate(wide(path)) {
        Ok(found) => found,
        Err(code) => return fail(code, 0),
    };
    if !host.is_dir() {
        return fail(ERROR_DIRECTORY, 0);
    }
    let prefix: String = wide(prefix).unwrap_or_default().chars().take(3).collect();
    let mut dir_text = windows_form(&dir);
    if !dir_text.ends_with('\\') {
        dir_text.push('\\');
    }
    let name = |number: u32| format!("{prefix}{number:04X}.TMP");
    let number = if unique != 0 {
        unique & 0xFFFF
    } else {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let start = TEMP_COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(nanos);
        let mut created = None;
        for i in 0..0x1_0000 {
            let number = (start.wrapping_add(i) & 0xFFFF).max(1);
            let file = host.join(name(number));
            match OpenOptions::new().write(true).create_new(true).open(&file) {
                Ok(_) => {
                    created = Some(number);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return fail(path_error(&e, &file), 0),
            }
        }
        match created {
            Some(number) => number,
            None => return fail(ERROR_ALREADY_EXISTS, 0),
        }
    };
    let full = format!("{dir_text}{}", name(number));
    if copy_out(&full, out, MAX_PATH as u32) as usize >= MAX_PATH {
        return fail(ERROR_BUFFER_OVERFLOW, 0);
    }
    number
}

/// The host directory a disk-space query is for: the resolved path (the
/// current directory when null), which must be a directory.
fn space_dir(path: *const u16) -> Result<PathBuf, u32> {
    let host = match wide(path) {
        Some(path) => locate(Some(path))?.1,
        None => resolve(&current_dir())?,
    };
    if !host.is_dir() {
        return Err(ERROR_PATH_NOT_FOUND);
    }
    Ok(host)
}

/// Free bytes (to the caller and in all) and the total, from the host disk
/// under the directory.
pub extern "win64" fn GetDiskFreeSpaceExW(
    path: *const u16,
    free_to_caller: *mut u64,
    total: *mut u64,
    free: *mut u64,
) -> BOOL {
    status(space_dir(path).map(|dir| {
        let available = crate::modules::storage::disk_free(&dir);
        // TODO: std has no portable way to read a disk's total size; twice
        // the free space stands for it.
        let size = available.saturating_mul(2);
        for (out, value) in [
            (free_to_caller, available),
            (total, size),
            (free, available),
        ] {
            if !out.is_null() {
                // SAFETY: checked non-null; the title's output.
                unsafe { out.write_unaligned(value) };
            }
        }
    }))
}

/// See [`GetDiskFreeSpaceExW`], in clusters of 8 sectors of 512 bytes
/// (counts saturate at `u32::MAX`).
pub extern "win64" fn GetDiskFreeSpaceW(
    path: *const u16,
    sectors_per_cluster: *mut DWORD,
    bytes_per_sector: *mut DWORD,
    free_clusters: *mut DWORD,
    total_clusters: *mut DWORD,
) -> BOOL {
    status(space_dir(path).map(|dir| {
        let cluster = u64::from(SECTORS_PER_CLUSTER * BYTES_PER_SECTOR);
        let free = crate::modules::storage::disk_free(&dir) / cluster;
        // TODO: as in GetDiskFreeSpaceExW, twice the free space stands for
        // the total.
        let total = free.saturating_mul(2);
        let clusters = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
        let values = [
            (sectors_per_cluster, SECTORS_PER_CLUSTER),
            (bytes_per_sector, BYTES_PER_SECTOR),
            (free_clusters, clusters(free)),
            (total_clusters, clusters(total)),
        ];
        for (out, value) in values {
            if !out.is_null() {
                // SAFETY: checked non-null; the title's output.
                unsafe { out.write_unaligned(value) };
            }
        }
    }))
}

/// The drive a root path (or, when null, the current directory) is on, if
/// it is mounted.
fn mounted_drive(root: *const u16) -> Result<char, u32> {
    let drive = match wide(root) {
        Some(root) => parse(&root).ok_or(ERROR_PATH_NOT_FOUND)?.drive,
        None => current_dir().drive,
    };
    drive_root(drive).map(|_| drive).ok_or(ERROR_PATH_NOT_FOUND)
}

/// Every mounted volume is "victory", serial "VICT", an NTFS volume with
/// 255-character names. A name that does not fit is `ERROR_INSUFFICIENT_BUFFER`.
pub extern "win64" fn GetVolumeInformationW(
    root: *const u16,
    name: *mut u16,
    name_len: DWORD,
    serial: *mut DWORD,
    max_component: *mut DWORD,
    flags: *mut DWORD,
    fs_name: *mut u16,
    fs_len: DWORD,
) -> BOOL {
    status(mounted_drive(root).and_then(|_| {
        for (buffer, len, text) in [(name, name_len, "victory"), (fs_name, fs_len, "NTFS")] {
            if !buffer.is_null() && copy_out(text, buffer, len) as usize != text.len() {
                return Err(ERROR_INSUFFICIENT_BUFFER);
            }
        }
        let values = [
            (serial, VOLUME_SERIAL),
            (max_component, 255),
            (
                flags,
                FILE_CASE_PRESERVED_NAMES | FILE_UNICODE_ON_DISK | FILE_SUPPORTS_SPARSE_FILES,
            ),
        ];
        for (out, value) in values {
            if !out.is_null() {
                // SAFETY: checked non-null; the title's output.
                unsafe { out.write_unaligned(value) };
            }
        }
        Ok(())
    }))
}

/// `X:\`, the root of the drive the path is on.
pub extern "win64" fn GetVolumePathNameW(path: *const u16, out: *mut u16, len: DWORD) -> BOOL {
    let Some(path) = wide(path) else {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    };
    let Some(path) = parse(&path) else {
        return fail(ERROR_PATH_NOT_FOUND, FALSE);
    };
    let root = format!("{}:\\", path.drive);
    if copy_out(&root, out, len) as usize != root.len() {
        return fail(ERROR_FILENAME_EXCED_RANGE, FALSE);
    }
    TRUE
}

/// The mounted letters, sorted.
fn letters() -> Vec<char> {
    let mut letters: Vec<char> = lock(&DRIVES).iter().map(|(l, _)| *l).collect();
    letters.sort_unstable();
    letters
}

/// A bit per mounted drive (bit 0 is `A:`).
pub extern "win64" fn GetLogicalDrives() -> DWORD {
    letters()
        .into_iter()
        .fold(0, |bits, letter| bits | 1 << (letter as u32 - 'A' as u32))
}

/// `X:\` and a NUL per mounted drive, and a NUL after the last; the length
/// without that last NUL, or the size needed.
pub extern "win64" fn GetLogicalDriveStringsW(len: DWORD, buffer: *mut u16) -> DWORD {
    let text: String = letters().into_iter().map(|l| format!("{l}:\\\0")).collect();
    copy_out(&text, buffer, len)
}

/// A fixed disk for a mounted drive, else no root directory.
pub extern "win64" fn GetDriveTypeW(root: *const u16) -> UINT {
    match mounted_drive(root) {
        Ok(_) => DRIVE_FIXED,
        Err(_) => DRIVE_NO_ROOT_DIR,
    }
}

// Finding files.

/// One entry of a directory listing.
#[repr(C)]
pub struct WIN32_FIND_DATAW {
    pub attributes: u32,
    pub creation: FILETIME,
    pub last_access: FILETIME,
    pub last_write: FILETIME,
    pub size_high: u32,
    pub size_low: u32,
    pub reserved0: u32,
    pub reserved1: u32,
    pub file_name: [u16; MAX_PATH],
    pub alternate_file_name: [u16; 14],
}

const _: () = assert!(std::mem::size_of::<WIN32_FIND_DATAW>() == 592);

/// Whether `name` matches the wildcard `pattern` (`*` any run, `?` one
/// character), regardless of case; `*.*` matches every name, as on Windows.
fn wildcard(pattern: &str, name: &str) -> bool {
    if pattern == "*.*" {
        return true;
    }
    let pattern: Vec<char> = pattern.to_uppercase().chars().collect();
    let name: Vec<char> = name.to_uppercase().chars().collect();
    let (mut p, mut n) = (0, 0);
    // Where the last `*` was, and where in the name it matches up to.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, n));
            p += 1;
        } else if let Some((star_p, star_n)) = star {
            p = star_p + 1;
            n = star_n + 1;
            star = Some((star_p, star_n + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// The entry for `path` (following links), named `name`.
fn entry(name: String, path: &Path, is_dot: bool) -> Entry {
    let metadata = fs::metadata(path)
        .or_else(|_| fs::symlink_metadata(path))
        .ok();
    Entry {
        name,
        metadata,
        is_dot,
    }
}

/// The entries a pattern lists: its last component (which may hold `*` and
/// `?`) matched in the directory the rest names; `.` and `..` first when
/// they match and the directory is not a drive root.
fn list(pattern: Option<String>) -> Result<VecDeque<Entry>, u32> {
    let pattern = pattern.ok_or(ERROR_INVALID_PARAMETER)?;
    let mut path = parse(&pattern).ok_or(ERROR_PATH_NOT_FOUND)?;
    let last = path.parts.pop().ok_or(ERROR_FILE_NOT_FOUND)?;
    let dir = resolve(&path)?;
    if !dir.is_dir() {
        return Err(ERROR_PATH_NOT_FOUND);
    }
    if !last.contains(['*', '?']) {
        // One name: the entry itself, under the host's spelling, read from
        // the directory (a case-insensitive host, APFS, finds `C.BIN` as
        // `c.bin` and would otherwise answer with the title's spelling).
        let name = spelling_in(&dir, &last).ok_or(ERROR_FILE_NOT_FOUND)?;
        let host = dir.join(&name);
        return Ok(VecDeque::from([entry(name, &host, false)]));
    }
    let mut names: Vec<String> = fs::read_dir(&dir)
        .map_err(|e| path_error(&e, &dir))?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| wildcard(&last, name))
        .collect();
    names.sort_by_cached_key(|name| (name.to_uppercase(), name.clone()));
    let mut entries = VecDeque::new();
    if !path.parts.is_empty() {
        for (dot, target) in [(".", Some(dir.as_path())), ("..", dir.parent())] {
            if wildcard(&last, dot) {
                let target = target.unwrap_or(&dir);
                entries.push_back(entry(dot.to_owned(), target, true));
            }
        }
    }
    entries.extend(names.into_iter().map(|name| {
        let host = dir.join(&name);
        entry(name, &host, false)
    }));
    if entries.is_empty() {
        return Err(ERROR_FILE_NOT_FOUND);
    }
    Ok(entries)
}

/// Writes an entry to the title's find data.
fn put_entry(entry: &Entry, data: *mut WIN32_FIND_DATAW) {
    let (attributes, times, size) = match &entry.metadata {
        Some(metadata) => (attributes(metadata), times(metadata), metadata.len()),
        None if entry.is_dot => (FILE_ATTRIBUTE_DIRECTORY, [FILETIME::default(); 3], 0),
        None => (FILE_ATTRIBUTE_NORMAL, [FILETIME::default(); 3], 0),
    };
    let mut file_name = [0; MAX_PATH];
    for (out, c) in file_name
        .iter_mut()
        .zip(entry.name.encode_utf16().take(MAX_PATH - 1))
    {
        *out = c;
    }
    let value = WIN32_FIND_DATAW {
        attributes,
        creation: times[0],
        last_access: times[1],
        last_write: times[2],
        size_high: (size >> 32) as u32,
        size_low: size as u32,
        reserved0: 0,
        reserved1: 0,
        file_name,
        alternate_file_name: [0; 14],
    };
    // SAFETY: the caller checked `data` non-null; the title's find data.
    unsafe { data.write_unaligned(value) };
}

/// Lists the entries a pattern matches: the first to `data`, the rest
/// through the handle. See [`FindFirstFileExW`].
pub extern "win64" fn FindFirstFileW(pattern: *const u16, data: *mut WIN32_FIND_DATAW) -> HANDLE {
    FindFirstFileExW(pattern, 0, data.cast(), 0, std::ptr::null_mut(), 0)
}

/// See [`FindFirstFileW`]: both information levels fill the same data; the
/// search may list more than directories when it asks for them only
/// (Windows treats that as advice too); the flags are ignored. A directory
/// that does not exist is `ERROR_PATH_NOT_FOUND`, no match
/// `ERROR_FILE_NOT_FOUND`.
pub extern "win64" fn FindFirstFileExW(
    pattern: *const u16,
    _info_level: i32,
    data: LPVOID,
    _search_op: i32,
    _filter: LPVOID,
    _flags: DWORD,
) -> HANDLE {
    if data.is_null() {
        return fail(ERROR_INVALID_PARAMETER, invalid());
    }
    match list(wide(pattern)) {
        Ok(mut entries) => {
            if let Some(first) = entries.pop_front() {
                put_entry(&first, data.cast());
            }
            let object = FindObject {
                entries: Mutex::new(entries),
            };
            handles::insert(Arc::new(object)) as HANDLE
        }
        Err(code) => fail(code, invalid()),
    }
}

/// The next entry, or FALSE and `ERROR_NO_MORE_FILES`.
pub extern "win64" fn FindNextFileW(handle: HANDLE, data: *mut WIN32_FIND_DATAW) -> BOOL {
    if data.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    status(with::<FindObject, ()>(handle, |find| {
        let next = lock(&find.entries).pop_front();
        let entry = next.ok_or(ERROR_NO_MORE_FILES)?;
        put_entry(&entry, data);
        Ok(())
    }))
}

/// Closes a find handle.
pub extern "win64" fn FindClose(handle: HANDLE) -> BOOL {
    status(with::<FindObject, ()>(handle, |_| Ok(())).and_then(|()| {
        handles::remove(handle as usize)
            .then_some(())
            .ok_or(ERROR_INVALID_HANDLE)
    }))
}

// The console.

/// The handles of the standard streams: made, and put in the table, once.
fn std_handles() -> &'static [usize; 3] {
    static HANDLES: OnceLock<[usize; 3]> = OnceLock::new();
    HANDLES.get_or_init(|| {
        [
            (StdStream::In, "CONIN$"),
            (StdStream::Out, "CONOUT$"),
            (StdStream::Err, "CONOUT$"),
        ]
        .map(|(stream, name)| {
            let object = FileObject::new(Kind::Std(stream), name.to_owned());
            handles::insert(Arc::new(object))
        })
    })
}

/// The handle of a standard stream: the host's stdin, stdout or stderr.
pub extern "win64" fn GetStdHandle(id: DWORD) -> HANDLE {
    let index = match id {
        STD_INPUT_HANDLE => 0,
        STD_OUTPUT_HANDLE => 1,
        STD_ERROR_HANDLE => 2,
        _ => return fail(ERROR_INVALID_PARAMETER, invalid()),
    };
    std_handles()[index] as HANDLE
}

/// Accepted; the standard handles stay the host's streams.
pub extern "win64" fn SetStdHandle(_id: DWORD, _handle: HANDLE) -> BOOL {
    TRUE
}

/// Writes `bytes` to the console stream the handle is; `chars` counted.
fn write_console(handle: HANDLE, bytes: &[u8], chars: DWORD, written: *mut DWORD) -> BOOL {
    status(with_file(handle, |object| {
        if !matches!(object.kind, Kind::Std(StdStream::Out | StdStream::Err)) {
            return Err(ERROR_INVALID_HANDLE);
        }
        object.write(bytes, None)?;
        if !written.is_null() {
            // SAFETY: checked non-null; the title's count.
            unsafe { *written = chars };
        }
        Ok(())
    }))
}

/// Writes UTF-16 text to a console stream, as UTF-8 (lossily).
pub extern "win64" fn WriteConsoleW(
    handle: HANDLE,
    buffer: *const u16,
    chars: DWORD,
    written: *mut DWORD,
    _reserved: LPVOID,
) -> BOOL {
    if buffer.is_null() && chars != 0 {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    let text = if chars == 0 {
        String::new()
    } else {
        // SAFETY: checked non-null; the title's buffer holds `chars` u16s.
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(buffer, chars as usize) })
    };
    write_console(handle, text.as_bytes(), chars, written)
}

/// Writes narrow text to a console stream, its bytes as they are.
pub extern "win64" fn WriteConsoleA(
    handle: HANDLE,
    buffer: *const u8,
    chars: DWORD,
    written: *mut DWORD,
    _reserved: LPVOID,
) -> BOOL {
    if buffer.is_null() && chars != 0 {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    let bytes = if chars == 0 {
        &[][..]
    } else {
        // SAFETY: checked non-null; the title's buffer holds `chars` bytes.
        unsafe { std::slice::from_raw_parts(buffer, chars as usize) }
    };
    write_console(handle, bytes, chars, written)
}

/// There is no console: `ERROR_INVALID_HANDLE`, as for a redirected stream.
pub extern "win64" fn GetConsoleMode(_handle: HANDLE, _mode: *mut DWORD) -> BOOL {
    fail(ERROR_INVALID_HANDLE, FALSE)
}

/// See [`GetConsoleMode`].
pub extern "win64" fn SetConsoleMode(_handle: HANDLE, _mode: DWORD) -> BOOL {
    fail(ERROR_INVALID_HANDLE, FALSE)
}

/// UTF-8.
pub extern "win64" fn GetConsoleOutputCP() -> UINT {
    CP_UTF8
}

/// UTF-8.
pub extern "win64" fn GetConsoleCP() -> UINT {
    CP_UTF8
}

// Asynchronous I/O, which completes at once here.

/// The bytes an overlapped I/O moved; FALSE and `ERROR_HANDLE_EOF` for a
/// read past the end.
fn overlapped_result(overlapped: *mut OVERLAPPED, transferred: *mut DWORD) -> BOOL {
    if overlapped.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    // SAFETY: checked non-null; the title's OVERLAPPED.
    let (internal, bytes) = unsafe { ((*overlapped).internal, (*overlapped).internal_high) };
    if !transferred.is_null() {
        // SAFETY: checked non-null; the title's count.
        unsafe { *transferred = bytes as u32 };
    }
    if internal == STATUS_END_OF_FILE {
        return fail(ERROR_HANDLE_EOF, FALSE);
    }
    TRUE
}

/// The result of a completed overlapped I/O (all are complete: `wait` does
/// not matter).
pub extern "win64" fn GetOverlappedResult(
    _handle: HANDLE,
    overlapped: *mut OVERLAPPED,
    transferred: *mut DWORD,
    _wait: BOOL,
) -> BOOL {
    overlapped_result(overlapped, transferred)
}

/// See [`GetOverlappedResult`].
pub extern "win64" fn GetOverlappedResultEx(
    _handle: HANDLE,
    overlapped: *mut OVERLAPPED,
    transferred: *mut DWORD,
    _ms: DWORD,
    _alertable: BOOL,
) -> BOOL {
    overlapped_result(overlapped, transferred)
}

/// Nothing is pending to cancel.
pub extern "win64" fn CancelIo(_handle: HANDLE) -> BOOL {
    TRUE
}

/// Nothing is pending to cancel: `ERROR_NOT_FOUND`.
pub extern "win64" fn CancelIoEx(_handle: HANDLE, _overlapped: *mut OVERLAPPED) -> BOOL {
    fail(ERROR_NOT_FOUND, FALSE)
}

/// Accepted; ranges are not locked (one process uses the files).
pub extern "win64" fn LockFile(
    _handle: HANDLE,
    _offset_low: DWORD,
    _offset_high: DWORD,
    _bytes_low: DWORD,
    _bytes_high: DWORD,
) -> BOOL {
    TRUE
}

/// See [`LockFile`].
pub extern "win64" fn UnlockFile(
    _handle: HANDLE,
    _offset_low: DWORD,
    _offset_high: DWORD,
    _bytes_low: DWORD,
    _bytes_high: DWORD,
) -> BOOL {
    TRUE
}

/// See [`LockFile`].
pub extern "win64" fn LockFileEx(
    _handle: HANDLE,
    _flags: DWORD,
    _reserved: DWORD,
    _bytes_low: DWORD,
    _bytes_high: DWORD,
    _overlapped: *mut OVERLAPPED,
) -> BOOL {
    TRUE
}

/// See [`LockFile`].
pub extern "win64" fn UnlockFileEx(
    _handle: HANDLE,
    _reserved: DWORD,
    _bytes_low: DWORD,
    _bytes_high: DWORD,
    _overlapped: *mut OVERLAPPED,
) -> BOOL {
    TRUE
}

/// No device controls: `ERROR_INVALID_FUNCTION`.
pub extern "win64" fn DeviceIoControl(
    _handle: HANDLE,
    _code: DWORD,
    _in_buffer: LPVOID,
    _in_size: DWORD,
    _out_buffer: LPVOID,
    _out_size: DWORD,
    _returned: *mut DWORD,
    _overlapped: *mut OVERLAPPED,
) -> BOOL {
    fail(ERROR_INVALID_FUNCTION, FALSE)
}

/// Narrow names are read as UTF-8 here, but titles ask this only to choose
/// a conversion, and TRUE is Windows' default.
pub extern "win64" fn AreFileApisANSI() -> BOOL {
    TRUE
}

/// Directories are not watched: `ERROR_INVALID_FUNCTION`.
pub extern "win64" fn ReadDirectoryChangesW(
    _handle: HANDLE,
    _buffer: LPVOID,
    _len: DWORD,
    _watch_subtree: BOOL,
    _filter: DWORD,
    _returned: *mut DWORD,
    _overlapped: *mut OVERLAPPED,
    _completion: CompletionRoutine,
) -> BOOL {
    fail(ERROR_INVALID_FUNCTION, FALSE)
}

/// See [`ReadDirectoryChangesW`].
pub extern "win64" fn FindFirstChangeNotificationW(
    _path: *const u16,
    _watch_subtree: BOOL,
    _filter: DWORD,
) -> HANDLE {
    fail(ERROR_INVALID_FUNCTION, invalid())
}

/// The path as it is: there are no short names to expand.
pub extern "win64" fn GetLongPathNameW(short: *const u16, long: *mut u16, len: DWORD) -> DWORD {
    match wide(short) {
        Some(path) => copy_out(&path, long, len),
        None => fail(ERROR_INVALID_PARAMETER, 0),
    }
}

/// The path as it is: there are no short names.
pub extern "win64" fn GetShortPathNameW(long: *const u16, short: *mut u16, len: DWORD) -> DWORD {
    match wide(long) {
        Some(path) => copy_out(&path, short, len),
        None => fail(ERROR_INVALID_PARAMETER, 0),
    }
}

/// The full path of `name` (with `ext` when it has no extension) in `path`,
/// or the current directory when that is null, if it exists.
pub extern "win64" fn SearchPathW(
    path: *const u16,
    name: *const u16,
    ext: *const u16,
    len: DWORD,
    buffer: *mut u16,
    file_part: *mut *mut u16,
) -> DWORD {
    let Some(mut name) = wide(name) else {
        return fail(ERROR_INVALID_PARAMETER, 0);
    };
    let last = name.rsplit(['\\', '/']).next().unwrap_or_default();
    if let Some(ext) = wide(ext).filter(|_| !last.contains('.')) {
        name.push_str(&ext);
    }
    let absolute = name.starts_with(['\\', '/']) || drive_prefix(&name).is_some();
    let full = match wide(path) {
        Some(dir) if !absolute => format!("{dir}\\{name}"),
        _ => name,
    };
    let found = locate(Some(full)).and_then(|(path, host)| {
        fs::symlink_metadata(&host).map_err(|_| ERROR_FILE_NOT_FOUND)?;
        Ok(path)
    });
    let path = match found {
        Ok(path) => path,
        Err(_) => return fail(ERROR_FILE_NOT_FOUND, 0),
    };
    let text = windows_form(&path);
    let written = copy_out(&text, buffer, len);
    let length = text.encode_utf16().count() as u32;
    if written == length && !file_part.is_null() {
        let part = path.parts.last().map_or(std::ptr::null_mut(), |last| {
            let start = length as usize - last.encode_utf16().count();
            // SAFETY: within the buffer just written.
            unsafe { buffer.add(start) }
        });
        // SAFETY: checked non-null; the title's output.
        unsafe { *file_part = part };
    }
    written
}

/// Links are not made: `ERROR_INVALID_FUNCTION`.
pub extern "win64" fn CreateHardLinkW(
    _new: *const u16,
    _existing: *const u16,
    _security: *const c_void,
) -> BOOL {
    fail(ERROR_INVALID_FUNCTION, FALSE)
}

/// See [`CreateHardLinkW`]; Windows returns a `BOOLEAN` here.
pub extern "win64" fn CreateSymbolicLinkW(
    _link: *const u16,
    _target: *const u16,
    _flags: DWORD,
) -> u8 {
    fail(ERROR_INVALID_FUNCTION, 0)
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(CreateFileW),
        hle_export!(CreateFileA),
        hle_export!(CreateFile2),
        hle_export!(ReadFile),
        hle_export!(WriteFile),
        hle_export!(ReadFileEx),
        hle_export!(WriteFileEx),
        hle_export!(SetFilePointer),
        hle_export!(SetFilePointerEx),
        hle_export!(GetFileSize),
        hle_export!(GetFileSizeEx),
        hle_export!(GetFileType),
        hle_export!(SetEndOfFile),
        hle_export!(FlushFileBuffers),
        hle_export!(GetFileInformationByHandle),
        hle_export!(GetFileInformationByHandleEx),
        hle_export!(SetFileInformationByHandle),
        hle_export!(GetFileTime),
        hle_export!(SetFileTime),
        hle_export!(GetFinalPathNameByHandleW),
        hle_export!(GetFileAttributesW),
        hle_export!(GetFileAttributesA),
        hle_export!(GetFileAttributesExW),
        hle_export!(SetFileAttributesW),
        hle_export!(DeleteFileW),
        hle_export!(DeleteFileA),
        hle_export!(RemoveDirectoryW),
        hle_export!(CreateDirectoryW),
        hle_export!(CreateDirectoryA),
        hle_export!(MoveFileW),
        hle_export!(MoveFileExW),
        hle_export!(CopyFileW),
        hle_export!(GetFullPathNameW),
        hle_export!(GetCurrentDirectoryW),
        hle_export!(SetCurrentDirectoryW),
        hle_export!(SetCurrentDirectoryA),
        hle_export!(GetTempPathW),
        hle_export!(GetTempFileNameW),
        hle_export!(GetDiskFreeSpaceExW),
        hle_export!(GetDiskFreeSpaceW),
        hle_export!(GetVolumeInformationW),
        hle_export!(GetVolumePathNameW),
        hle_export!(GetLogicalDrives),
        hle_export!(GetLogicalDriveStringsW),
        hle_export!(GetDriveTypeW),
        hle_export!(FindFirstFileW),
        hle_export!(FindFirstFileExW),
        hle_export!(FindNextFileW),
        hle_export!(FindClose),
        hle_export!(GetStdHandle),
        hle_export!(SetStdHandle),
        hle_export!(WriteConsoleW),
        hle_export!(WriteConsoleA),
        hle_export!(GetConsoleMode),
        hle_export!(SetConsoleMode),
        hle_export!(GetConsoleOutputCP),
        hle_export!(GetConsoleCP),
        hle_export!(GetOverlappedResult),
        hle_export!(GetOverlappedResultEx),
        hle_export!(CancelIo),
        hle_export!(CancelIoEx),
        hle_export!(LockFile),
        hle_export!(UnlockFile),
        hle_export!(LockFileEx),
        hle_export!(UnlockFileEx),
        hle_export!(DeviceIoControl),
        hle_export!(AreFileApisANSI),
        hle_export!(ReadDirectoryChangesW),
        hle_export!(FindFirstChangeNotificationW),
        hle_export!(GetLongPathNameW),
        hle_export!(GetShortPathNameW),
        hle_export!(SearchPathW),
        hle_export!(CreateHardLinkW),
        hle_export!(CreateSymbolicLinkW),
    ]
}

#[cfg(test)]
mod tests {
    use super::super::handles::{CloseHandle, GetLastError};
    use super::super::sync::{CreateEventW, WAIT_OBJECT_0, WaitForSingleObject};
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// A new empty host folder of the test's own, mounted; its letter.
    fn folder(name: &str) -> (PathBuf, char) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "victory-win32-files-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let letter = mount(&dir).unwrap();
        (dir, letter)
    }

    fn open(path: &str, access: u32, disposition: u32, flags: u32) -> HANDLE {
        let null = std::ptr::null_mut();
        CreateFileW(w(path).as_ptr(), access, 0, null, disposition, flags, null)
    }

    const RW: u32 = GENERIC_READ | GENERIC_WRITE;

    #[test]
    fn files_are_created_written_read_and_sized() {
        victory_loader::teb::attach_current_thread();
        let (dir, x) = folder("io");
        let file = format!(r"{x}:\sub\a.txt");
        assert_eq!(open(&file, RW, CREATE_NEW, 0), invalid());
        assert_eq!(GetLastError(), ERROR_PATH_NOT_FOUND);
        let sub = w(&format!(r"{x}:\sub"));
        assert_eq!(CreateDirectoryW(sub.as_ptr(), std::ptr::null()), TRUE);
        assert_eq!(CreateDirectoryW(sub.as_ptr(), std::ptr::null()), FALSE);
        assert_eq!(GetLastError(), ERROR_ALREADY_EXISTS);

        let handle = open(&file, RW, CREATE_NEW, 0);
        assert_ne!(handle, invalid());
        let mut moved = 0;
        let null = std::ptr::null_mut();
        let data = b"0123456789";
        assert_eq!(
            WriteFile(handle, data.as_ptr().cast(), 10, &mut moved, null),
            TRUE
        );
        assert_eq!(moved, 10);
        assert_eq!(SetFilePointerEx(handle, 0, null.cast(), FILE_BEGIN), TRUE);
        let mut buffer = [0u8; 4];
        assert_eq!(
            ReadFile(handle, buffer.as_mut_ptr().cast(), 4, &mut moved, null),
            TRUE
        );
        assert_eq!((moved, &buffer), (4, b"0123"));
        let mut size = 0;
        assert_eq!(GetFileSizeEx(handle, &mut size), TRUE);
        assert_eq!(size, 10);
        assert_eq!(SetFilePointer(handle, -3, null.cast(), FILE_END), 7);
        assert_eq!(GetFileType(handle), FILE_TYPE_DISK);
        assert_eq!(SetEndOfFile(handle), TRUE);
        assert_eq!(GetFileSizeEx(handle, &mut size), TRUE);
        assert_eq!(size, 7);
        assert_eq!(open(&file, RW, CREATE_NEW, 0), invalid());
        assert_eq!(GetLastError(), ERROR_FILE_EXISTS);

        let overlapped_handle = open(&file, GENERIC_READ, OPEN_EXISTING, FILE_FLAG_OVERLAPPED);
        assert_ne!(overlapped_handle, invalid());
        let event = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
        let mut overlapped = OVERLAPPED {
            internal: 0,
            internal_high: 0,
            offset: 4,
            offset_high: 0,
            event,
        };
        let mut buffer = [0u8; 8];
        let read = ReadFile(
            overlapped_handle,
            buffer.as_mut_ptr().cast(),
            8,
            &mut moved,
            &mut overlapped,
        );
        assert_eq!(read, TRUE);
        assert_eq!((moved, &buffer[..3]), (3, &b"456"[..]));
        assert_eq!(WaitForSingleObject(event, 0), WAIT_OBJECT_0);
        assert_eq!(
            SetFilePointer(overlapped_handle, 0, null.cast(), FILE_CURRENT),
            0
        );
        let mut transferred = 0;
        assert_eq!(
            GetOverlappedResult(overlapped_handle, &mut overlapped, &mut transferred, TRUE),
            TRUE
        );
        assert_eq!(transferred, 3);
        overlapped.offset = 100;
        let read = ReadFile(
            overlapped_handle,
            buffer.as_mut_ptr().cast(),
            8,
            &mut moved,
            &mut overlapped,
        );
        assert_eq!(read, FALSE);
        assert_eq!(GetLastError(), ERROR_HANDLE_EOF);
        assert_eq!(CloseHandle(overlapped_handle), TRUE);
        assert_eq!(CloseHandle(handle), TRUE);
        assert_eq!(CloseHandle(event), TRUE);

        assert_eq!(DeleteFileW(w(&file).as_ptr()), TRUE);
        assert_eq!(
            GetFileAttributesW(w(&file).as_ptr()),
            INVALID_FILE_ATTRIBUTES
        );
        assert_eq!(GetLastError(), ERROR_FILE_NOT_FOUND);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn paths_are_case_insensitive_and_normalized() {
        victory_loader::teb::attach_current_thread();
        let (dir, x) = folder("case");
        fs::create_dir(dir.join("Dir")).unwrap();
        fs::write(dir.join("Dir").join("File.txt"), b"x").unwrap();
        for path in [
            format!("{x}:/dir/FILE.TXT"),
            format!(r"{x}:\dir\..\dir\.\file.txt"),
            format!(r"\\?\{x}:\dir\file.txt"),
        ] {
            let handle = open(&path, GENERIC_READ, OPEN_EXISTING, 0);
            assert_ne!(handle, invalid(), "{path}");
            assert_eq!(CloseHandle(handle), TRUE);
        }
        assert_eq!(
            SetCurrentDirectoryW(w(&format!(r"{x}:\dir")).as_ptr()),
            TRUE
        );
        for path in ["file.txt", r"..\dir\file.txt"] {
            let handle = open(path, GENERIC_READ, OPEN_EXISTING, 0);
            assert_ne!(handle, invalid(), "{path}");
            assert_eq!(CloseHandle(handle), TRUE);
        }
        let mut buffer = [0u16; 64];
        let len = GetCurrentDirectoryW(64, buffer.as_mut_ptr());
        assert_eq!(
            String::from_utf16_lossy(&buffer[..len as usize]),
            format!(r"{x}:\dir")
        );

        let mut part = std::ptr::null_mut();
        let name = w(r"..\other\x.bin");
        let len = GetFullPathNameW(name.as_ptr(), 64, buffer.as_mut_ptr(), &mut part);
        let full = String::from_utf16_lossy(&buffer[..len as usize]);
        assert_eq!(full, format!(r"{x}:\other\x.bin"));
        // SAFETY: `part` points into `buffer`.
        let offset = unsafe { part.offset_from(buffer.as_ptr()) };
        assert_eq!(offset as u32, len - 5);
        let name = w(r"..\..\..\dir");
        let len = GetFullPathNameW(name.as_ptr(), 64, buffer.as_mut_ptr(), &mut part);
        assert_eq!(
            String::from_utf16_lossy(&buffer[..len as usize]),
            format!(r"{x}:\dir")
        );

        assert_eq!(
            open(r"\\server\share\file.txt", GENERIC_READ, OPEN_EXISTING, 0),
            invalid()
        );
        assert_eq!(GetLastError(), ERROR_PATH_NOT_FOUND);
        // No test mounts `A:` (`mount` hands out `D` to `Z`).
        let path = r"A:\file.txt";
        assert_eq!(open(path, GENERIC_READ, OPEN_EXISTING, 0), invalid());
        assert_eq!(GetLastError(), ERROR_PATH_NOT_FOUND);
        let _ = fs::remove_dir_all(dir);
    }

    /// The names a find handle lists, and the attributes of each.
    fn find_all(pattern: &str) -> Result<Vec<(String, u32)>, u32> {
        // SAFETY: plain data; zeroes are a valid value.
        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        let handle = FindFirstFileW(w(pattern).as_ptr(), &mut data);
        if handle == invalid() {
            return Err(GetLastError());
        }
        let mut names = Vec::new();
        loop {
            let len = data.file_name.iter().position(|c| *c == 0).unwrap();
            let name = String::from_utf16_lossy(&data.file_name[..len]);
            names.push((name, data.attributes));
            if FindNextFileW(handle, &mut data) == FALSE {
                assert_eq!(GetLastError(), ERROR_NO_MORE_FILES);
                break;
            }
        }
        assert_eq!(FindClose(handle), TRUE);
        Ok(names)
    }

    /// A free letter is never one of the console's fixed drives, which
    /// `mount_at` gives: a mount of `T:` would otherwise take the drive of a
    /// folder mounted before it.
    #[test]
    fn free_letters_skip_the_fixed_drives() {
        let (dir, letter) = folder("letters");
        assert!(letter != 'G' && letter != 'T', "{letter}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn directories_are_listed_with_wildcards() {
        victory_loader::teb::attach_current_thread();
        let (dir, x) = folder("find");
        let list = dir.join("list");
        fs::create_dir(&list).unwrap();
        for name in ["b.txt", "A.txt", "c.bin"] {
            fs::write(list.join(name), b"").unwrap();
        }
        fs::create_dir(list.join("d")).unwrap();

        let all = find_all(&format!(r"{x}:\list\*")).unwrap();
        let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, [".", "..", "A.txt", "b.txt", "c.bin", "d"]);
        assert!(all[5].1 & FILE_ATTRIBUTE_DIRECTORY != 0);
        assert!(all[2].1 & FILE_ATTRIBUTE_DIRECTORY == 0);
        // A drive root lists no dots.
        let root = find_all(&format!(r"{x}:\*")).unwrap();
        assert_eq!(
            root.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["list"]
        );

        let txt = find_all(&format!(r"{x}:\list\*.txt")).unwrap();
        let names: Vec<&str> = txt.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["A.txt", "b.txt"]);
        let bin = find_all(&format!(r"{x}:\list\?.bin")).unwrap();
        assert_eq!(bin[0].0, "c.bin");
        assert_eq!(bin.len(), 1);
        assert_eq!(
            find_all(&format!(r"{x}:\list\none*")),
            Err(ERROR_FILE_NOT_FOUND)
        );
        assert_eq!(
            find_all(&format!(r"{x}:\nodir\*")),
            Err(ERROR_PATH_NOT_FOUND)
        );
        let one = find_all(&format!(r"{x}:\LIST\C.BIN")).unwrap();
        assert_eq!(one[0].0, "c.bin");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn attributes_and_information_match_the_host() {
        victory_loader::teb::attach_current_thread();
        let (dir, x) = folder("info");
        fs::create_dir(dir.join("dir")).unwrap();
        fs::write(dir.join("dir").join("file.txt"), b"hello").unwrap();
        let file = format!(r"{x}:\dir\file.txt");
        let name = w(&file);

        let root = GetFileAttributesW(w(&format!(r"{x}:\")).as_ptr());
        assert!(root != INVALID_FILE_ATTRIBUTES && root & FILE_ATTRIBUTE_DIRECTORY != 0);
        let attributes = GetFileAttributesW(name.as_ptr());
        assert!(attributes & FILE_ATTRIBUTE_ARCHIVE != 0);
        assert!(attributes & FILE_ATTRIBUTE_DIRECTORY == 0);
        assert_eq!(
            SetFileAttributesW(name.as_ptr(), FILE_ATTRIBUTE_READONLY),
            TRUE
        );
        assert!(GetFileAttributesW(name.as_ptr()) & FILE_ATTRIBUTE_READONLY != 0);
        assert_eq!(open(&file, GENERIC_WRITE, OPEN_EXISTING, 0), invalid());
        assert_eq!(GetLastError(), ERROR_ACCESS_DENIED);
        assert_eq!(
            SetFileAttributesW(name.as_ptr(), FILE_ATTRIBUTE_NORMAL),
            TRUE
        );
        assert!(GetFileAttributesW(name.as_ptr()) & FILE_ATTRIBUTE_READONLY == 0);

        let mut data = WIN32_FILE_ATTRIBUTE_DATA::default();
        assert_eq!(GetFileAttributesExW(name.as_ptr(), 0, &mut data), TRUE);
        assert_eq!((data.size_high, data.size_low), (0, 5));

        let handle = open(&file, GENERIC_READ, OPEN_EXISTING, 0);
        assert_ne!(handle, invalid());
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        assert_eq!(GetFileInformationByHandle(handle, &mut info), TRUE);
        assert_eq!((info.size_low, info.links), (5, 1));

        let mut standard = [0u8; 24];
        let class = FILE_STANDARD_INFO;
        let ok = GetFileInformationByHandleEx(handle, class, standard.as_mut_ptr().cast(), 24);
        assert_eq!(ok, TRUE);
        assert_eq!(i64::from_le_bytes(standard[8..16].try_into().unwrap()), 5);
        let mut buffer = [0u8; 4 + 2 * 64];
        let size = buffer.len() as u32;
        let ok =
            GetFileInformationByHandleEx(handle, FILE_NAME_INFO, buffer.as_mut_ptr().cast(), size);
        assert_eq!(ok, TRUE);
        let bytes = u32::from_le_bytes(buffer[..4].try_into().unwrap()) as usize;
        let name_info: Vec<u16> = buffer[4..4 + bytes]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(String::from_utf16_lossy(&name_info), r"\dir\file.txt");

        let mut path = [0u16; 64];
        let len = GetFinalPathNameByHandleW(handle, path.as_mut_ptr(), 64, 0);
        let final_path = String::from_utf16_lossy(&path[..len as usize]);
        assert_eq!(final_path, format!(r"\\?\{x}:\dir\file.txt"));
        assert_eq!(CloseHandle(handle), TRUE);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn moves_copies_deletes_and_temp_names() {
        victory_loader::teb::attach_current_thread();
        let (dir, x) = folder("move");
        fs::write(dir.join("a"), b"a").unwrap();
        fs::write(dir.join("c"), b"c").unwrap();
        let path = |name: &str| w(&format!(r"{x}:\{name}"));
        assert_eq!(MoveFileExW(path("a").as_ptr(), path("b").as_ptr(), 0), TRUE);
        assert!(dir.join("b").exists() && !dir.join("a").exists());
        assert_eq!(
            MoveFileExW(path("b").as_ptr(), path("c").as_ptr(), 0),
            FALSE
        );
        assert_eq!(GetLastError(), ERROR_ALREADY_EXISTS);
        let replace = MOVEFILE_REPLACE_EXISTING;
        assert_eq!(
            MoveFileExW(path("b").as_ptr(), path("c").as_ptr(), replace),
            TRUE
        );
        assert_eq!(fs::read(dir.join("c")).unwrap(), b"a");
        fs::write(dir.join("d"), b"d").unwrap();
        assert_eq!(
            CopyFileW(path("c").as_ptr(), path("d").as_ptr(), TRUE),
            FALSE
        );
        assert_eq!(GetLastError(), ERROR_FILE_EXISTS);
        assert_eq!(
            CopyFileW(path("c").as_ptr(), path("d").as_ptr(), FALSE),
            TRUE
        );
        assert_eq!(fs::read(dir.join("d")).unwrap(), b"a");

        let temp = dir.join("temp");
        fs::create_dir(&temp).unwrap();
        mount_at('T', &temp);
        let mut buffer = [0u16; MAX_PATH];
        let len = GetTempPathW(MAX_PATH as u32, buffer.as_mut_ptr());
        assert_eq!(String::from_utf16_lossy(&buffer[..len as usize]), r"T:\");
        let number = GetTempFileNameW(
            w(r"T:\").as_ptr(),
            w("abc").as_ptr(),
            0,
            buffer.as_mut_ptr(),
        );
        assert_ne!(number, 0);
        let len = buffer.iter().position(|c| *c == 0).unwrap();
        let name = String::from_utf16_lossy(&buffer[..len]);
        assert_eq!(name, format!(r"T:\abc{number:04X}.TMP"));
        assert!(temp.join(&name[3..]).is_file());

        fs::create_dir(dir.join("full")).unwrap();
        fs::write(dir.join("full").join("f"), b"").unwrap();
        fs::create_dir(dir.join("empty")).unwrap();
        assert_eq!(RemoveDirectoryW(path("full").as_ptr()), FALSE);
        assert_eq!(GetLastError(), ERROR_DIR_NOT_EMPTY);
        assert_eq!(RemoveDirectoryW(path("empty").as_ptr()), TRUE);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn std_handles_and_nul_exist() {
        victory_loader::teb::attach_current_thread();
        let (dir, x) = folder("std");
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        assert_ne!(out, invalid());
        assert_eq!(GetStdHandle(STD_OUTPUT_HANDLE), out);
        let mut moved = 1;
        let null = std::ptr::null_mut();
        assert_eq!(
            WriteFile(out, b"".as_ptr().cast(), 0, &mut moved, null),
            TRUE
        );
        assert_eq!(moved, 0);

        let nul = open("NUL", RW, OPEN_EXISTING, 0);
        assert_ne!(nul, invalid());
        let mut buffer = [0u8; 4];
        assert_eq!(
            ReadFile(nul, buffer.as_mut_ptr().cast(), 4, &mut moved, null),
            TRUE
        );
        assert_eq!(moved, 0);
        assert_eq!(
            WriteFile(nul, b"abc".as_ptr().cast(), 3, &mut moved, null),
            TRUE
        );
        assert_eq!(moved, 3);
        assert_eq!(GetFileType(nul), FILE_TYPE_CHAR);
        assert_eq!(CloseHandle(nul), TRUE);

        let bit = 1 << (x as u32 - 'A' as u32);
        assert!(GetLogicalDrives() & bit != 0);
        assert_eq!(GetDriveTypeW(w(&format!(r"{x}:\")).as_ptr()), DRIVE_FIXED);
        // (`A:` is never mounted: `mount` hands out `D` to `Z`.)
        let root = w(r"A:\");
        assert_eq!(GetDriveTypeW(root.as_ptr()), DRIVE_NO_ROOT_DIR);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 74);
    }
}
