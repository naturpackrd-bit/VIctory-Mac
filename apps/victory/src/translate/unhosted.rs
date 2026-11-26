//! Off Windows, the host checks `translate` offers (`--pipelines`,
//! `--validate`, `VICTORY_PAIRS`) have nothing to run on yet: the same names
//! as `victory_shader::emit::{host, validator}`, whose constructors fail.
//!
//! Stub off Windows (docs/macos.md): asking for a check stops `translate`
//! with the error below; translation itself runs as everywhere. The Metal
//! back end (M4) brings pipeline checks of its own.

use std::path::{Path, PathBuf};

use victory_shader::emit::HostShader;

const NO_HOST: &str = "pipeline checks need host D3D12 (Windows) for now";
const NO_VALIDATOR: &str = "DXIL validation needs dxil.dll (Windows) for now";

/// A host device to make pipelines on. There is none: no value of this
/// type can exist, so its methods are never called.
pub(crate) enum Host {}

impl Host {
    pub(crate) fn new(_warp: bool) -> Result<Self, String> {
        Err(NO_HOST.into())
    }

    pub(crate) fn adapter_name(&self) -> Option<String> {
        match *self {}
    }

    pub(crate) fn translation_caps(&self) -> Option<(u32, bool)> {
        match *self {}
    }

    pub(crate) fn pipeline(&self, _: &HostShader, _: &HostShader) -> Result<(), String> {
        match *self {}
    }

    pub(crate) fn compute_pipeline(&self, _: &HostShader) -> Result<(), String> {
        match *self {}
    }

    pub(crate) fn mesh_pipeline(&self, _: &HostShader, _: &HostShader) -> Result<(), String> {
        match *self {}
    }
}

/// The pixel stage a vertex check pairs with. `translate` asks for it only
/// beside a [`Host`] (`Host::new(..).ok().map(|h| (h, stub_ps()))`), and
/// there is never one here.
pub(crate) fn stub_ps() -> HostShader {
    unreachable!("asked for only beside a Host, which cannot exist off Windows")
}

/// The vertex stage a pixel check pairs with: none without a host.
pub(crate) fn stub_vs(_: &HostShader) -> Result<HostShader, String> {
    Err(NO_HOST.into())
}

/// The DXIL validator (`dxil.dll`). There is none: no value of this type
/// can exist.
pub(crate) enum Validator {}

impl Validator {
    pub(crate) fn default_path() -> Option<PathBuf> {
        None
    }

    pub(crate) fn load(_: Option<&Path>) -> Result<Self, String> {
        Err(NO_VALIDATOR.into())
    }

    pub(crate) fn validate(&mut self, _: &mut [u8]) -> Result<(), String> {
        match *self {}
    }
}
