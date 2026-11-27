//! The images mapped in this process: which one an address lies in, for
//! naming it as `module+rva` (crash reports, logs) and for its exception
//! table (unwinding). Windows' loader keeps this for the images it maps
//! (`crash::locate`); ours registers the images it maps (`MappedImage`).

use std::sync::atomic::{AtomicUsize, Ordering};
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

/// How many images may be registered at once.
pub const MAX_IMAGES: usize = 64;

/// A slot of [`RANGES`]: an image's base and end; `(0, 0)` when empty.
type Slot = (AtomicUsize, AtomicUsize);

/// The registered images' ranges, for the signal handler, which may take
/// no lock ([`is_title`]): written under [`IMAGES`]' lock, read without.
static RANGES: [Slot; MAX_IMAGES] =
    [const { (AtomicUsize::new(0), AtomicUsize::new(0)) }; MAX_IMAGES];

/// Why an image could not be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("more than {MAX_IMAGES} images mapped at once")]
pub struct TooManyImages;

fn lock() -> MutexGuard<'static, Vec<Image>> {
    IMAGES.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn register(image: Image) -> Result<(), TooManyImages> {
    let mut images = lock();
    if !insert(&RANGES, image.base, image.base + image.size) {
        return Err(TooManyImages);
    }
    images.push(image);
    Ok(())
}

pub(crate) fn unregister(base: usize) {
    let mut images = lock();
    images.retain(|i| i.base != base);
    remove(&RANGES, base);
}

/// Puts `[base, end)` in an empty slot of `ranges`: false when none is.
/// Its end is stored last, so that a reader never sees a slot half
/// written as a range (an end of 0 holds nothing).
fn insert(ranges: &[Slot], base: usize, end: usize) -> bool {
    let Some(slot) = ranges
        .iter()
        .find(|(b, e)| b.load(Ordering::Relaxed) == 0 && e.load(Ordering::Relaxed) == 0)
    else {
        return false;
    };
    slot.0.store(base, Ordering::Release);
    slot.1.store(end, Ordering::Release);
    true
}

/// Empties the slots of `ranges` that start at `base`, their end first.
fn remove(ranges: &[Slot], base: usize) {
    for (b, e) in ranges {
        if b.load(Ordering::Relaxed) == base && e.load(Ordering::Relaxed) != 0 {
            e.store(0, Ordering::Release);
            b.store(0, Ordering::Release);
        }
    }
}

/// The base of the slot of `ranges` that `address` lies in.
fn holds(ranges: &[Slot], address: usize) -> Option<usize> {
    ranges.iter().find_map(|(b, e)| {
        let end = e.load(Ordering::Acquire);
        let base = b.load(Ordering::Acquire);
        (base <= address && address < end).then_some(base)
    })
}

/// Whether `address` lies in a registered image (title code or data):
/// without allocating or locking, for a signal handler.
pub fn is_title(address: usize) -> bool {
    holds(&RANGES, address).is_some()
}

/// The base of the registered image `address` lies in, as [`is_title`]
/// finds it.
pub fn title_base(address: usize) -> Option<usize> {
    holds(&RANGES, address)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_title_sees_registered_images_without_the_lock() {
        // A range no other test registers, and no real memory.
        let (base, size) = (0x6100_0000, 0x2000);
        let image = |name: &str| Image {
            name: name.into(),
            base,
            size,
            exception_table: None,
        };
        register(image("fake")).unwrap();
        {
            // Even while the registry is locked (by another thread's
            // unwind, say).
            let _held = lock();
            assert!(is_title(base));
            assert!(is_title(base + size - 1));
            assert_eq!(title_base(base + 0x123), Some(base));
            assert!(!is_title(base + size));
            assert!(!is_title(base - 1));
        }
        unregister(base);
        assert!(!is_title(base));
    }

    #[test]
    fn the_ranges_take_at_most_their_slots() {
        let ranges: [Slot; MAX_IMAGES] =
            [const { (AtomicUsize::new(0), AtomicUsize::new(0)) }; MAX_IMAGES];
        for i in 0..MAX_IMAGES {
            assert!(insert(&ranges, 0x1000 * (i + 1), 0x1000 * (i + 1) + 0x800));
        }
        assert!(!insert(&ranges, 0x100_0000, 0x100_1000), "a 65th");
        assert_eq!(holds(&ranges, 0x3400), Some(0x3000));
        assert_eq!(holds(&ranges, 0x3800), None);
        remove(&ranges, 0x3000);
        assert_eq!(holds(&ranges, 0x3400), None);
        assert!(insert(&ranges, 0x100_0000, 0x100_1000), "a freed slot");
        assert_eq!(holds(&ranges, 0x100_0fff), Some(0x100_0000));
    }
}
