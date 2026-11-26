//! The images mapped in this process: which one an address lies in, for
//! naming it as `module+rva` (crash reports, logs) and for its exception
//! table (unwinding). Windows' loader keeps this for the images it maps
//! (`crash::locate`); ours registers the images it maps (`MappedImage`).

use std::sync::{Mutex, MutexGuard};

/// A mapped image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub name: String,
    pub base: usize,
    pub size: usize,
    /// Its exception directory (`.pdata`) in memory: address and length.
    pub exception_table: Option<(usize, usize)>,
}

impl Image {
    pub fn holds(&self, address: usize) -> bool {
        (self.base..self.base + self.size).contains(&address)
    }
}

static IMAGES: Mutex<Vec<Image>> = Mutex::new(Vec::new());

fn lock() -> MutexGuard<'static, Vec<Image>> {
    IMAGES.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn register(image: Image) {
    lock().push(image);
}

pub(crate) fn unregister(base: usize) {
    lock().retain(|i| i.base != base);
}

/// The image `address` lies in.
pub fn find(address: usize) -> Option<Image> {
    lock().iter().find(|i| i.holds(address)).cloned()
}

/// Runs `f` with the image `address` lies in, without allocating or
/// waiting: for a signal handler. `None` when the registry is locked (by
/// the faulting thread, say); then `f` is not run.
pub fn with_image<R>(address: usize, f: impl FnOnce(Option<&Image>) -> R) -> Option<R> {
    let images = IMAGES.try_lock().ok()?;
    Some(f(images.iter().find(|i| i.holds(address))))
}

/// `name+rva` for an address in a mapped image, else the address itself.
pub fn locate(address: usize) -> String {
    match find(address) {
        Some(image) => format!("{}+{:#x}", image.name, address - image.base),
        None => format!("{address:#x}"),
    }
}
