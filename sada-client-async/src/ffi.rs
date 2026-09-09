//! Minimal Byondapi FFI bindings with runtime symbol resolution.
//!
//! Based on byondapi.h version 516.1674.

use std::{
    ffi::{CStr, CString},
    sync::{LazyLock, OnceLock},
};

pub static BYOND: LazyLock<ByondApi> = LazyLock::new(|| init().expect("failed to initialize byondapi"));

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct CByondValue {
    pub ty: u8,
    pub junk1: u8,
    pub junk2: u8,
    pub junk3: u8,
    pub data: CByondValueData,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub union CByondValueData {
    pub number: f32,
    pub r#ref: usize,
}

impl Default for CByondValueData {
    fn default() -> Self { CByondValueData { number: 0.0 } }
}

#[allow(nonstandard_style)]
pub struct ByondApi {
    pub ToString: extern "C-unwind" fn(*const CByondValue, *mut i8, *mut u32) -> bool,
    pub Value_SetStr: extern "C-unwind" fn(*mut CByondValue, *const i8),
    pub Value_SetNum: extern "C-unwind" fn(*mut CByondValue, f32),
    pub Return: extern "C-unwind" fn(*const CByondValue, *const CByondValue) -> bool,
    pub CRASH: extern "C-unwind" fn(*const u8) -> !,
}

fn init() -> Option<ByondApi> {
    #[cfg(target_os = "windows")]
    let library = libloading::os::windows::Library::open_already_loaded("byondcore.dll").ok()?;
    #[cfg(not(target_os = "windows"))]
    let library = libloading::os::unix::Library::this();

    Some(unsafe {
        ByondApi {
            ToString: *library.get(b"Byond_ToString").ok()?,
            Value_SetStr: *library.get(b"ByondValue_SetStr").ok()?,
            Value_SetNum: *library.get(b"ByondValue_SetNum").ok()?,
            Return: *library.get(b"Byond_Return").ok()?,
            CRASH: *library.get(b"Byond_CRASH").ok()?,
        }
    })
}

impl CByondValue {
    pub const NULL: Self = CByondValue {
        ty: 0,
        junk1: 0,
        junk2: 0,
        junk3: 0,
        data: CByondValueData { number: 0.0 },
    };
}

impl From<()> for CByondValue {
    fn from(_: ()) -> Self { CByondValue::NULL }
}

impl From<i32> for CByondValue {
    fn from(value: i32) -> Self {
        let mut cval = CByondValue::default();
        (BYOND.Value_SetNum)(&mut cval, value as f32);
        cval
    }
}

impl From<CByondValue> for i32 {
    fn from(value: CByondValue) -> Self { (unsafe { value.data.number }) as i32 }
}
