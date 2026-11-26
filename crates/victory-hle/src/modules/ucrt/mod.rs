//! The Universal C Runtime (`ucrtbase.dll` and the `api-ms-win-crt-*` API
//! sets) and the pieces of `vcruntime140.dll` that are not exception
//! dispatch: on Windows the host provides them, here they are ours
//! (docs/macos.md, M2b). One file per area; each lists its exports. Stdio
//! (the `printf` and `scanf` engines, streams and file descriptors) is not
//! here yet.

pub mod heap;
pub mod locale;
pub mod math;
pub mod runtime;
pub mod string;
pub mod time;

use crate::HleModule;

pub(crate) fn module() -> HleModule {
    let mut exports = Vec::new();
    exports.extend(runtime::exports());
    exports.extend(heap::exports());
    exports.extend(string::exports());
    exports.extend(math::exports());
    exports.extend(time::exports());
    exports.extend(locale::exports());
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
