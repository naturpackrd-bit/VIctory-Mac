//! Runs a title's original x86-64 code natively, inside our process.
//!
//! Xbox Series X|S executables are x86-64 PE32+ images built for a Zen 2
//! CPU, so the machine code needs no translation. What differs is the
//! platform underneath, so the loader's job is to:
//!
//! 1. [`prepare()`] a lightly patched copy of the image (pure, portable): mark
//!    it as a DLL, remove its entry point and import directory, and detach
//!    its TLS callbacks, so the Windows loader maps it, relocates it,
//!    allocates its static TLS and registers its unwind tables, but runs
//!    none of its code and resolves none of its imports;
//! 2. load that copy as a real module (debuggers, profilers, crash dumps
//!    and stack walks all see it); this step is Windows-only;
//! 3. map the DLLs the title ships beside it the same way
//!    (`load_title_dlls`);
//! 4. bind every import ourselves through an [`ImportResolver`], our
//!    runtime (`victory-hle`), the host or the title's DLLs, and route
//!    anything unresolved to a generated stub that reports exactly what
//!    was called;
//! 5. initialise the title's DLLs, then run TLS callbacks and the entry
//!    point on a thread with the stack size the image asks for; from then
//!    on, pass every thread start and end on to the attached images.
//!
//! Steps 2 to 5 are Windows-only so far (macOS: docs/macos.md). Elsewhere
//! the portable helpers only they call are unused, and are let be; the
//! Windows build is the whole one, and still finds code that is dead.
#![cfg_attr(not(windows), allow(dead_code))]
//!
//! The runtime runs the title's x86-64 code in our process, so it is built
//! for x86-64 only: on Apple silicon under Rosetta 2, with the tools built
//! natively beside it (docs/macos.md). Elsewhere this crate is empty.
#![cfg(target_arch = "x86_64")]

#[cfg(windows)]
mod console_insn;
#[cfg(windows)]
mod crash;
#[cfg(windows)]
pub mod descriptor_sites;
#[cfg(not(windows))]
pub mod faults;
pub mod frame_rate;
pub mod images;
pub mod map;
pub mod prepare;
pub mod stubs;
#[cfg(not(windows))]
pub mod teb;
pub mod unwind;
pub(crate) mod publish;
pub mod resolution;
pub mod resolve;
#[cfg(not(windows))]
pub mod seh;
#[cfg(all(windows, feature = "diagnostics"))]
mod profile;
#[cfg(all(windows, feature = "diagnostics"))]
mod stacks;
#[cfg(windows)]
mod threads;
#[cfg(windows)]
mod thunks;
#[cfg(windows)]
mod tls;
#[cfg(windows)]
pub mod write_sites;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use crash::{
    locate, log_crash, log_crashes_to, note_title_output, report_crashes, report_unhandled,
};
#[cfg(all(windows, feature = "diagnostics"))]
pub use crash::save_on_crash;
pub use map::{MapError, MappedImage};
pub use prepare::{PrepareError, PreparedImage, prepare};
pub use resolve::{ImportResolver, Provenance, Resolution};
#[cfg(all(windows, feature = "diagnostics"))]
pub use stacks::{dump_stacks, dump_stacks_on_schedule};
#[cfg(all(windows, feature = "diagnostics"))]
pub use profile::profile_on_schedule;
#[cfg(windows)]
pub use threads::{disable_thread_calls, set_before_thread_shutdown};
#[cfg(windows)]
pub use thunks::{UnimplementedPolicy, unimplemented_call_count};
#[cfg(windows)]
pub use windows::{
    BindReport, LoadedImage, MissingImport, WithTitleDlls, load, load_title_dll, load_title_dlls,
};

/// `DLL_PROCESS_ATTACH`, for TLS callbacks.
pub const DLL_PROCESS_ATTACH: u32 = 1;
/// `DLL_PROCESS_DETACH`.
pub const DLL_PROCESS_DETACH: u32 = 0;
/// `DLL_THREAD_ATTACH`: a thread starts.
pub const DLL_THREAD_ATTACH: u32 = 2;
/// `DLL_THREAD_DETACH`: a thread ends.
pub const DLL_THREAD_DETACH: u32 = 3;
