//! The hand-written half of the bindings.
//!
//! Re-exports the generated types and adds what bindgen cannot.

use std::{cell::RefCell, ffi::CString};

use crate::BYONDAPI;
pub use crate::bindings::*;

impl !Send for CByondValue {}
impl !Sync for CByondValue {}

impl CByondValue {
    /// The null value, which is also what a zeroed struct means.
    pub const NULL: Self = CByondValue {
        type_: 0,
        junk1: 0,
        junk2: 0,
        junk3: 0,
        data: ByondValueData { num: 0.0 },
    };

    /// Wrap a number without calling into BYOND.
    ///
    /// Every field is ours to fill in for this one type, so there is nothing for `ByondValue_SetNum` to do that this
    /// does not; `0x2A` is the tag BYOND reads as a number.
    pub fn number(value: f32) -> Self {
        CByondValue {
            type_: 0x2A,
            junk1: 0,
            junk2: 0,
            junk3: 0,
            data: ByondValueData { num: value },
        }
    }
}

impl std::fmt::Debug for CByondValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let data = match self.type_ {
            0x00 => "data: NULL",
            0x06 => &format!("data: 0x{:08X} (str)", unsafe { self.data.ref_ }),
            0x2A => &format!("data: {} (num)", unsafe { self.data.num }),
            _ => &format!("data: 0x{:08X} (ref)", unsafe { self.data.ref_ }),
        };

        write!(f, "CByondValue {{ type: 0x{:02X}, {data} }}", self.type_)
    }
}

impl From<()> for CByondValue {
    fn from(_: ()) -> Self { CByondValue::NULL }
}

impl From<CByondValue> for () {
    fn from(_: CByondValue) -> Self {}
}

impl From<u16> for CByondValue {
    fn from(value: u16) -> Self { CByondValue::number(value as f32) }
}

impl From<CByondValue> for u16 {
    fn from(value: CByondValue) -> Self { (unsafe { value.data.num }) as u16 }
}

impl From<i16> for CByondValue {
    fn from(value: i16) -> Self { CByondValue::number(value as f32) }
}

impl From<CByondValue> for i16 {
    fn from(value: CByondValue) -> Self { (unsafe { value.data.num }) as i16 }
}

impl From<String> for CByondValue {
    fn from(value: String) -> Self {
        let mut string = value.into_bytes();
        string.push(0);

        let mut value = CByondValue::NULL;
        unsafe { BYONDAPI.ByondValue_SetStr(&mut value, string.as_ptr() as _) };

        value
    }
}

impl From<CByondValue> for String {
    fn from(value: CByondValue) -> Self {
        let mut buf = vec![0u8; 256];
        let mut len = buf.len() as u32;

        while !unsafe { BYONDAPI.Byond_ToString(&value, buf.as_mut_ptr() as _, &mut len) } {
            if len == 0 {
                return String::new();
            }
            buf.resize(len as usize, 0);
        }

        buf.truncate(len as usize);

        CString::from_vec_with_nul(buf)
            .ok()
            .and_then(|cstr| cstr.into_string().ok())
            .unwrap_or_default()
    }
}

thread_local! {
    static LAST_CRASH: RefCell<String> = const {RefCell::new(String::new())};
}

/// Raise a runtime error in the DM proc that called us, and never come back.
///
/// `Byond_CRASH` longjumps out of this frame, so nothing after the call runs and nothing on the way out is dropped.
/// The message has to still be there when BYOND reads it, which is why it is parked in a thread-local and terminated
/// rather than built on the stack.
pub fn crash(msg: String) -> ! {
    let msg = LAST_CRASH.with_borrow_mut(|last| {
        *last = msg;
        last.push('\0');
        last.as_ptr()
    });
    unsafe { BYONDAPI.Byond_CRASH(msg as _) }; // longjump
    unsafe { std::hint::unreachable_unchecked() }
}
