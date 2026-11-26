//! The Windows API the title calls into the host's own DLLs for on Windows
//! (`HostPolicy`: kernel32, kernelbase, ntdll, the api-ms-win-core-* sets),
//! implemented here for hosts that have none (macOS; Linux for tests). One
//! file per area; each lists its exports, and this module serves them
//! under the DLL names titles import them from (docs/macos.md, M2b).

pub mod files;
pub mod handles;
pub mod memory;
pub mod sync;
pub mod threads;
pub mod time;

use crate::HleModule;

pub(crate) fn module() -> HleModule {
    let mut exports = Vec::new();
    exports.extend(handles::exports());
    exports.extend(sync::exports());
    exports.extend(threads::exports());
    exports.extend(memory::exports());
    exports.extend(time::exports());
    exports.extend(files::exports());
    HleModule {
        name: "win32",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "ntdll.dll",
            "api-ms-win-core-*",
            "api-ms-win-security-*",
            "ext-ms-win-*",
        ],
        exports,
    }
}
