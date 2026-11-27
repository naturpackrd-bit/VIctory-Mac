//! The Universal C Runtime (`ucrtbase.dll` and the `api-ms-win-crt-*` API
//! sets) and `vcruntime140.dll`, C++ exception handling included (`cxx`):
//! on Windows the host provides them, here they are ours (docs/macos.md,
//! M2b). One file per area; each lists its exports. Stdio is `format.rs`
//! and `scan.rs` (the `printf` and `scanf` engines), `lowio.rs` (file
//! descriptors and the file system) and `stdio.rs` (streams).

pub mod cxx;
pub mod format;
pub mod heap;
pub mod locale;
pub mod lowio;
pub mod math;
pub mod runtime;
pub mod scan;
pub mod stdio;
pub mod string;
pub mod time;

use crate::HleModule;

pub(crate) fn module() -> HleModule {
    let mut exports = Vec::new();
    exports.extend(runtime::exports());
    exports.extend(cxx::exports());
    exports.extend(heap::exports());
    exports.extend(string::exports());
    exports.extend(math::exports());
    exports.extend(time::exports());
    exports.extend(locale::exports());
    exports.extend(lowio::exports());
    exports.extend(stdio::exports());
    HleModule {
        name: "ucrt",
        dlls: &[
            "ucrtbase.dll",
            "api-ms-win-crt-*",
            "vcruntime140.dll",
            "vcruntime140_1.dll",
        ],
        exports,
    }
}
