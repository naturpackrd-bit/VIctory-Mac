//! Implementations, one module per area of the console API.
//!
//! Adding an API: write an `extern "system"` function named exactly like
//! the import, then list it with `hle_export!` in the module's `module()`.
//! Run `victory census` over your titles to see what is still missing.
//!
//! Planned areas (driven by census results, not by completeness):
//! `file` (package paths), `thread` (TLS callbacks for title threads),
//! `xaudio2`, `xstore` stubs.

pub mod apu;
pub mod audio;
pub mod d3d12x;
pub mod display;
pub mod dsp;
pub mod dstorage;
#[cfg(windows)]
pub mod files;
pub mod gameinput;
pub mod kernel;
pub mod lifecycle;
pub mod media;
#[cfg(not(windows))]
pub mod msvcp;
pub mod net;
pub mod rpcrt4;
pub mod speech;
pub mod storage;
pub mod xasync;
pub mod xg;
pub mod xgame;
pub mod xgameruntime;
pub mod xgamesave;
pub mod xgameui;
#[cfg(not(windows))]
pub mod ucrt;
#[cfg(not(windows))]
pub mod win32;
#[cfg(windows)]
pub mod xmem;
#[cfg(not(windows))]
#[path = "xmem_sections.rs"]
pub mod xmem;
pub mod xnetworking;
pub mod xpackage;
pub mod xstore;
pub mod xsystem;
pub mod xuser;

use crate::HleModule;

pub(crate) fn all() -> Vec<HleModule> {
    let mut all = vec![
        kernel::module(),
        xgameruntime::module(),
        gameinput::module(),
        d3d12x::module(),
        audio::module(),
        dsp::module(),
        xg::module(),
    ];
    all.push(apu::module());
    all.push(media::module());
    all.push(dstorage::module());
    #[cfg(windows)]
    all.push(files::module());
    all.push(lifecycle::module());
    all.push(net::module());
    all.push(rpcrt4::module());
    all.push(xmem::module());
    all.push(xsystem::module());
    #[cfg(not(windows))]
    all.push(msvcp::module());
    #[cfg(not(windows))]
    all.push(ucrt::module());
    #[cfg(not(windows))]
    all.push(win32::module());
    all
}
