//! A PE32+ image mapped by us, not by the host's loader: where the host has
//! none for it (macOS), and in tests everywhere. Headers and sections are
//! copied into fresh memory, base relocations applied when the preferred
//! base is taken, imports bound through an [`ImportResolver`], and each page
//! given the protection of the sections in it. Nothing in the image runs.
//!
//! Off Windows, where the host has no TEB for title code, the image's
//! static TLS is registered and its `gs` accesses are patched (`teb`).
//!
//! Not yet (docs/macos.md, M2a): the exception table, delay imports, TLS
//! callbacks and the entry point, and reporting stubs for imports nobody
//! resolves (mapping fails on those instead).

use victory_pe::layout::{self, OPT_IMAGE_BASE, OPT_SIZE_OF_HEADERS};
use victory_pe::{ImageInfo, PeError, base_relocations};
use victory_platform::{Host, Memory, Protection};

use crate::{ImportResolver, Resolution};

const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const SCN_MEM_READ: u32 = 0x4000_0000;
const SCN_MEM_WRITE: u32 = 0x8000_0000;

/// How many images may have static TLS at once: the slots of a thread's
/// TLS array (`teb`).
pub const TLS_IMAGES: usize = 64;

/// Why an image could not be mapped.
#[derive(Debug, thiserror::Error)]
pub enum MapError {
    #[error(transparent)]
    Pe(#[from] PeError),
    #[error("no memory for an image of {0:#x} bytes")]
    NoMemory(usize),
    #[error("the image cannot be moved from its base {0:#x} (no relocations), which is taken")]
    NotMovable(u64),
    #[error("malformed image: {0}")]
    Malformed(String),
    #[error("imports nobody resolves: {}", .0.join(", "))]
    Unresolved(Vec<String>),
    #[error("the host refused the protection of [{0:#x}, {1:#x})")]
    Protection(usize, usize),
    #[error("more than {TLS_IMAGES} images with static TLS")]
    TlsImages,
    #[error("no free memory within reach of the image for its gs thunks")]
    Thunks,
    #[error("gs accesses the loader cannot patch: {}", .0.join(", "))]
    GsAccesses(Vec<String>),
}

/// An image in our memory, mapped by [`MappedImage::map`]; unmapped when
/// dropped.
#[derive(Debug)]
pub struct MappedImage {
    base: usize,
    size: usize,
    info: ImageInfo,
    /// The image's static TLS index (`teb`), if it has TLS.
    #[cfg(not(windows))]
    tls_index: Option<u32>,
    /// The thunks its patched `gs` accesses call (`teb`), if it had any.
    #[cfg(not(windows))]
    thunks: Option<crate::teb::Thunks>,
}

impl MappedImage {
    /// Maps the image whose file is `file`, binding its imports through
    /// `resolver`.
    pub fn map(file: &[u8], resolver: &dyn ImportResolver) -> Result<MappedImage, MapError> {
        let info = ImageInfo::parse(file)?;
        let page = Host::page_size();
        let size = (info.size_of_image as usize).next_multiple_of(page);
        let preferred = usize::try_from(info.image_base)
            .ok()
            .filter(|b| b % page == 0);
        let base = match preferred.and_then(|at| Host::allocate(size, Some(at))) {
            Some(base) => base,
            None if !info.has_relocations => return Err(MapError::NotMovable(info.image_base)),
            None => Host::allocate(size, None).ok_or(MapError::NoMemory(size))?,
        };
        // From here on, dropping the image gives its memory back.
        #[cfg_attr(windows, allow(unused_mut))]
        let mut image = MappedImage {
            base,
            size,
            info,
            #[cfg(not(windows))]
            tls_index: None,
            #[cfg(not(windows))]
            thunks: None,
        };
        let headers = size_of_headers(file)?.min(file.len());
        image.copy(file, headers)?;
        image.relocate(file)?;
        image.bind(resolver)?;
        #[cfg(not(windows))]
        {
            image.tls_index = image.static_tls()?;
            image.thunks = image.patch_gs()?;
        }
        image.protect(page, headers)?;
        Ok(image)
    }

    /// The address the image is mapped at.
    pub fn base(&self) -> usize {
        self.base
    }

    /// The image's size in memory, rounded up to whole pages.
    pub fn size(&self) -> usize {
        self.size
    }

    pub fn info(&self) -> &ImageInfo {
        &self.info
    }

    /// Where the export `name` is; `None` for one the image lacks and for a
    /// forwarder (another DLL's function).
    pub fn export(&self, name: &str) -> Option<usize> {
        let export = self
            .info
            .exports
            .iter()
            .find(|e| e.name.as_deref() == Some(name))?;
        if export.forward.is_some() {
            return None;
        }
        Some(self.base + export.rva? as usize)
    }

    /// `len` bytes of the image at `rva`, checked to lie inside it.
    fn at(&self, rva: usize, len: usize) -> Result<*mut u8, MapError> {
        match rva.checked_add(len) {
            Some(end) if end <= self.size => Ok((self.base + rva) as *mut u8),
            _ => Err(MapError::Malformed(format!(
                "{len} bytes at rva {rva:#x}, past the image's end ({:#x})",
                self.size
            ))),
        }
    }

    /// Copies the first `headers` bytes of the file and every section's
    /// bytes; what a section has beyond them stays zero.
    fn copy(&self, file: &[u8], headers: usize) -> Result<(), MapError> {
        let to = self.at(0, headers)?;
        // SAFETY: `headers` bytes of our fresh, writable image, checked above.
        unsafe { std::ptr::copy_nonoverlapping(file.as_ptr(), to, headers) };
        for section in &self.info.sections {
            let span = span(section);
            let len = (section.file_size as usize).min(span);
            let from = section.file_offset as usize;
            let bytes = file.get(from..from + len).ok_or_else(|| {
                MapError::Malformed(format!("section {} past the file's end", section.name))
            })?;
            let to = self.at(section.rva as usize, span)?;
            // SAFETY: as above.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), to, len) };
        }
        Ok(())
    }

    /// Adds the image's displacement from its preferred base to every
    /// absolute address in it, and to the `ImageBase` of its mapped headers,
    /// as Windows' loader does.
    fn relocate(&self, file: &[u8]) -> Result<(), MapError> {
        let delta = (self.base as u64).wrapping_sub(self.info.image_base);
        if delta == 0 {
            return Ok(());
        }
        for rva in base_relocations(file)? {
            let at = self.at(rva as usize, 8)?.cast::<u64>();
            // SAFETY: eight bytes of our writable image, checked above; base
            // relocations need not be aligned.
            unsafe { at.write_unaligned(at.read_unaligned().wrapping_add(delta)) };
        }
        let optional = layout::optional_header_offset(file)
            .ok_or_else(|| MapError::Malformed("headers".to_owned()))?;
        let image_base = self.at(optional + OPT_IMAGE_BASE, 8)?.cast::<u64>();
        // SAFETY: as above.
        unsafe { image_base.write_unaligned(self.base as u64) };
        Ok(())
    }

    /// Writes every import's address into its IAT slot.
    fn bind(&self, resolver: &dyn ImportResolver) -> Result<(), MapError> {
        let mut unresolved = Vec::new();
        for module in &self.info.imports {
            for entry in &module.entries {
                match resolver.resolve(&module.dll, &entry.symbol) {
                    Resolution::Found { address, .. } => {
                        let slot = self.at(entry.iat_slot_rva as usize, 8)?.cast::<u64>();
                        // SAFETY: an IAT slot of our writable image, checked
                        // above.
                        unsafe { slot.write_unaligned(address as u64) };
                    }
                    Resolution::Missing => {
                        unresolved.push(format!("{}!{}", module.dll, entry.symbol))
                    }
                }
            }
        }
        if unresolved.is_empty() {
            Ok(())
        } else {
            Err(MapError::Unresolved(unresolved))
        }
    }

    /// Registers the image's static TLS (`teb`), if it has a TLS directory,
    /// and writes the index it got into the image's `_tls_index`.
    #[cfg(not(windows))]
    fn static_tls(&self) -> Result<Option<u32>, MapError> {
        let Some(tls) = &self.info.tls else {
            return Ok(None);
        };
        // IMAGE_TLS_DIRECTORY64: start and end of the raw data, the index's
        // address, the callbacks' address (u64 each), the zero fill (u32).
        let dir = self.at(tls.directory_rva as usize, 40)?;
        // SAFETY: 40 bytes of our image, checked above; relocated already,
        // so its addresses are in our mapping.
        let (start, end, index_va, zero_fill) = unsafe {
            (
                dir.cast::<u64>().read_unaligned(),
                dir.add(8).cast::<u64>().read_unaligned(),
                dir.add(16).cast::<u64>().read_unaligned(),
                dir.add(32).cast::<u32>().read_unaligned() as usize,
            )
        };
        let offset = |va: u64, what: &str| {
            usize::try_from(va)
                .ok()
                .and_then(|va| va.checked_sub(self.base))
                .filter(|o| *o <= self.size)
                .ok_or_else(|| MapError::Malformed(format!("TLS {what} {va:#x} outside the image")))
        };
        let (start, end) = (offset(start, "start")?, offset(end, "end")?);
        let len = end
            .checked_sub(start)
            .ok_or_else(|| MapError::Malformed("TLS data ends before it starts".to_owned()))?;
        // SAFETY: `len` bytes of our image, checked by `at`.
        let init = unsafe { std::slice::from_raw_parts(self.at(start, len)?, len) };
        let index = crate::teb::register_tls(init, zero_fill).ok_or(MapError::TlsImages)?;
        let index_at = self.at(offset(index_va, "index")?, 4)?.cast::<u32>();
        // SAFETY: four bytes of our writable image, checked above.
        unsafe { index_at.write_unaligned(index) };
        Ok(Some(index))
    }

    /// Patches every `gs` access in the image's code to call a thunk that
    /// reaches our TEB (`teb`); the thunks, if any access was found.
    #[cfg(not(windows))]
    fn patch_gs(&self) -> Result<Option<crate::teb::Thunks>, MapError> {
        use crate::teb::{PatchError, Thunks, patch_sites};
        let mut thunks: Option<Thunks> = None;
        let mut patched = 0;
        for section in &self.info.sections {
            if section.characteristics & SCN_MEM_EXECUTE == 0 {
                continue;
            }
            let span = span(section);
            // SAFETY: the section's bytes of our writable image, checked by
            // `at`; nothing runs them yet.
            let code = unsafe {
                std::slice::from_raw_parts_mut(self.at(section.rva as usize, span)?, span)
            };
            let sites = victory_pe::gs_sites(code);
            if sites.is_empty() {
                continue;
            }
            let thunks = match &mut thunks {
                Some(thunks) => thunks,
                None => thunks.insert(Thunks::near(self.base, self.size).ok_or(MapError::Thunks)?),
            };
            let at = self.base + section.rva as usize;
            patched += patch_sites(code, at, &sites, thunks).map_err(|e| match e {
                PatchError::Unpatchable(sites) => MapError::GsAccesses(
                    sites
                        .iter()
                        .map(|s| {
                            format!(
                                "{:?} at rva {:#x}",
                                s.access,
                                section.rva as usize + s.offset
                            )
                        })
                        .collect(),
                ),
                PatchError::Full => MapError::Thunks,
            })?;
        }
        if let Some(thunks) = &thunks {
            thunks
                .finish()
                .map_err(|(at, len)| MapError::Protection(at, at + len))?;
        }
        tracing::debug!(patched, "gs accesses patched to reach our TEB");
        Ok(thunks)
    }

    /// Gives each page the protection its sections ask for (all of those it
    /// holds a part of, should sections share a page); the `headers` bytes
    /// at the start are read-only, and what no section covers has no access.
    fn protect(&self, page: usize, headers: usize) -> Result<(), MapError> {
        let pages = self.size / page;
        let mut wanted = vec![0u32; pages];
        for flags in wanted.iter_mut().take(headers.div_ceil(page)) {
            *flags = SCN_MEM_READ;
        }
        for section in &self.info.sections {
            let start = section.rva as usize / page;
            let end = (section.rva as usize + span(section)).div_ceil(page);
            for flags in wanted.iter_mut().take(end.min(pages)).skip(start) {
                *flags |=
                    section.characteristics & (SCN_MEM_EXECUTE | SCN_MEM_READ | SCN_MEM_WRITE);
            }
        }
        let mut run = 0;
        while run < pages {
            let flags = wanted[run];
            let len = wanted[run..].iter().take_while(|&&f| f == flags).count();
            let protection = match (flags & SCN_MEM_EXECUTE != 0, flags & SCN_MEM_WRITE != 0) {
                (true, true) => Protection::ReadWriteExecute,
                (true, false) => Protection::ReadExecute,
                (false, true) => Protection::ReadWrite,
                (false, false) if flags & SCN_MEM_READ != 0 => Protection::Read,
                (false, false) => Protection::NoAccess,
            };
            let (at, bytes) = (self.base + run * page, len * page);
            // SAFETY: pages of our own image, to which nothing holds a
            // reference.
            if !unsafe { Host::protect(at, bytes, protection) } {
                return Err(MapError::Protection(at, at + bytes));
            }
            run += len;
        }
        Ok(())
    }
}

/// The bytes a section takes in memory (a `VirtualSize` of 0 means its
/// raw size).
fn span(section: &victory_pe::SectionInfo) -> usize {
    match section.virtual_size {
        0 => section.file_size as usize,
        n => n as usize,
    }
}

/// `SizeOfHeaders`: how much of the file Windows maps as the headers.
fn size_of_headers(file: &[u8]) -> Result<usize, MapError> {
    let malformed = |what: &str| MapError::Malformed(what.to_owned());
    let optional = layout::optional_header_offset(file).ok_or_else(|| malformed("headers"))?;
    let headers = layout::read_u32(file, optional + OPT_SIZE_OF_HEADERS)
        .ok_or_else(|| malformed("SizeOfHeaders"))?;
    Ok(headers as usize)
}

impl Drop for MappedImage {
    fn drop(&mut self) {
        #[cfg(not(windows))]
        if let Some(index) = self.tls_index {
            crate::teb::unregister_tls(index);
        }
        // SAFETY: the whole allocation `map` made; nothing in it runs any
        // more (the owner keeps the image while its code may).
        unsafe { Host::free(self.base, self.size) };
    }
}
