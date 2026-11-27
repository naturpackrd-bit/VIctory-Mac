//! Which DLLs come from the title's own folder: what `run` maps from it and
//! `census` looks for in it.

/// DLLs the host or our runtime provide are never taken from the title.
pub(crate) fn title_dll(host: &victory_hle::HostPolicy, dll: &str) -> bool {
    !host.allows(dll) && !victory_hle::registry().iter().any(|m| m.serves(dll))
}
