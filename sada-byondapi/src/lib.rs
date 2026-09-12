#![feature(macro_attr, negative_impls, custom_inner_attributes)]

//! Low and high level bindings to the BYONDAPI.

mod bindings;
pub mod macros;
pub mod sys;

use std::sync::LazyLock;

pub use paste;

use crate::sys::Byondapi;

/// Byondapi's function table, resolved the first time something reaches for it.
pub static BYONDAPI: LazyLock<Byondapi> = LazyLock::new(|| init().expect("failed to initialize byondapi"));

/// Look up byondapi's exports in whatever loaded this library.
fn init() -> Result<Byondapi, Box<dyn std::error::Error>> {
    #[cfg(target_os = "windows")]
    let library = libloading::os::windows::Library::open_already_loaded("byondcore.dll")?;
    #[cfg(not(target_os = "windows"))]
    let library = libloading::os::unix::Library::this();

    Ok(unsafe { Byondapi::from_library(library)? })
}
