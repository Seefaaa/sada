//! The hand-written half of the bindings.
//!
//! Re-exports the generated types and adds what bindgen cannot.

use std::cell::RefCell;

pub use crate::bindings::*;
use crate::{BYONDAPI, byond};

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
    pub const fn number(value: f32) -> Self {
        CByondValue {
            type_: 0x2A,
            junk1: 0,
            junk2: 0,
            junk3: 0,
            data: ByondValueData { num: value },
        }
    }
}

const impl Default for CByondValue {
    fn default() -> Self { CByondValue::NULL }
}

impl std::fmt::Debug for CByondValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hint = match self.type_ {
            0x00 => "NULL",
            0x06 => "str",
            0x2A => "num",
            _ => "ref",
        };

        let lit = if self.type_ == 0x2A {
            format_args!(" ({})", unsafe { self.data.num })
        } else {
            format_args!("")
        };

        f.debug_struct("CByondValue")
            .field("type", &format_args!("0x{:02X} ({hint})", self.type_))
            .field("data", &format_args!("0x{:08X}{lit}", unsafe { self.data.ref_ }))
            .finish()
    }
}

impl PartialEq for CByondValue {
    fn eq(&self, other: &Self) -> bool {
        self.type_ == other.type_ && unsafe { self.data.ref_ } == unsafe { other.data.ref_ }
    }
}

impl From<()> for CByondValue {
    fn from(_: ()) -> Self { CByondValue::NULL }
}

impl From<CByondValue> for () {
    fn from(_: CByondValue) -> Self {}
}

const impl From<bool> for CByondValue {
    fn from(value: bool) -> Self { CByondValue::number(if value { 1.0 } else { 0.0 }) }
}

impl From<CByondValue> for bool {
    fn from(value: CByondValue) -> Self { unsafe { value.data.num != 0. } }
}

const impl From<u16> for CByondValue {
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

impl From<f32> for CByondValue {
    fn from(value: f32) -> Self { CByondValue::number(value) }
}

impl From<CByondValue> for f32 {
    fn from(value: CByondValue) -> Self { unsafe { value.data.num } }
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
    fn from(value: CByondValue) -> Self { byond::to_string(&value) }
}

impl From<&CByondValue> for String {
    fn from(value: &CByondValue) -> Self { byond::to_string(value) }
}

impl<T> From<Option<T>> for CByondValue
where
    T: Into<CByondValue>,
{
    fn from(value: Option<T>) -> Self { value.map(Into::into).unwrap_or(Self::NULL) }
}

#[cfg(feature = "sada")]
impl<T, E> From<Result<T, E>> for CByondValue
where
    T: Into<CByondValue>,
    E: Into<CByondValue>,
{
    fn from(value: Result<T, E>) -> Self {
        use crate::byond;

        let (ok, payload) = match value {
            Ok(ok) => (true, ok.into()),
            Err(err) => (false, err.into()),
        };

        let value = byond::new(c"/datum/sada_result", &[CByondValue::from(ok), payload]);

        byond::value_decref(&payload);

        value
    }
}

thread_local! {
    static LAST_CRASH: RefCell<String> = const { RefCell::new(String::new()) };
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
