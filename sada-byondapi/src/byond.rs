//! Calls into BYOND, wrapped so the rest of the workspace never touches the raw bindings.

use std::ffi::CStr;

use crate::{BYONDAPI, sys::CByondValue};

/// Build an instance of the DM type `type`, passing `args` to its `New`.
pub fn new(r#type: &CStr, args: &[CByondValue]) -> CByondValue {
    let mut type_ = CByondValue::NULL;
    let str_id = unsafe { BYONDAPI.Byond_AddGetStrId(r#type.as_ptr() as _) };
    unsafe { BYONDAPI.ByondValue_SetStrId(&mut type_, str_id) };

    let mut result = CByondValue::NULL;
    let success = unsafe { BYONDAPI.Byond_New(&type_, args.as_ptr(), args.len() as _, &mut result) };

    if !success {
        panic!("failed to create '{}'", r#type.to_string_lossy());
    }

    result
}

/// Drop one reference.
#[inline]
pub fn value_decref(value: &CByondValue) { unsafe { BYONDAPI.ByondValue_DecRef(value) }; }

/// Add one reference.
#[inline]
pub fn value_incref(value: &CByondValue) { unsafe { BYONDAPI.ByondValue_IncRef(value) }; }

/// Get the reference count of a value.
#[inline]
pub fn ref_count(value: &CByondValue) -> Option<u32> {
    let mut res = 0;
    unsafe { BYONDAPI.Byond_Refcount(value as _, &mut res) }.then_some(res)
}

/// Call a byondapi read that fills a caller-provided buffer, growing the buffer until it fits.
///
/// Read elements are referenced, so the caller must decref them when done.
pub fn read_to_vec<T>(func: impl Fn(*mut T, *mut u32) -> bool, initial: usize) -> Vec<T>
where
    T: Default + Copy,
{
    let mut buf = vec![T::default(); initial];
    let mut len = buf.len() as u32;

    while !func(buf.as_mut_ptr(), &mut len) {
        if len <= buf.len() as u32 {
            return Vec::new();
        }
        buf.resize(len as usize, T::default());
    }

    buf.truncate(len as usize);

    buf
}

/// Read a list into a [`Vec<CByondValue>`], growing the vector until it fits.
///
/// Does not check that the value is a list, so the caller must do that first.
///
/// Read elements are referenced, so the caller must decref them when done.
#[inline]
pub fn read_list(list: &CByondValue, with_capacity: usize) -> Vec<CByondValue> {
    read_to_vec(
        |buf, len| unsafe { BYONDAPI.Byond_ReadList(list, buf, len) },
        with_capacity,
    )
}

/// Read a list of key-value pairs into a zipped [`Vec<CByondValue>`] (key1, value1, key2, value2, ...), growing the
/// vector until it fits.
///
/// Does not check that the value is a list, so the caller must do that first.
///
/// Read elements are referenced, so the caller must decref them when done.
#[inline]
pub fn read_list_assoc(list: &CByondValue, with_capacity: usize) -> Vec<CByondValue> {
    read_to_vec(
        |buf, len| unsafe { BYONDAPI.Byond_ReadListAssoc(list, buf as _, len) },
        with_capacity,
    )
}

/// Check whether a value is a list.
#[inline]
pub fn value_islist(value: &CByondValue) -> bool { unsafe { BYONDAPI.ByondValue_IsList(value) } }

/// Check whether a value is a string.
#[inline]
pub fn value_isstr(value: &CByondValue) -> bool { unsafe { BYONDAPI.ByondValue_IsStr(value) } }

/// Check whether a value is a number.
#[inline]
pub fn value_isnum(value: &CByondValue) -> bool { unsafe { BYONDAPI.ByondValue_IsNum(value) } }

/// Render any value the way DM's `"[value]"` would, or an empty string if it cannot be rendered.
pub fn to_string(value: &CByondValue) -> String {
    let mut buf = read_to_vec(|buf, len| unsafe { BYONDAPI.Byond_ToString(value, buf as _, len) }, 24);
    buf.pop();
    buf.try_into().unwrap_or_default()
}

/// Read a variable from a datum, returning `None` when the datum does not have it.
pub fn read_var(value: &CByondValue, var: &CStr) -> Option<CByondValue> {
    let mut result = CByondValue::NULL;
    let success = unsafe { BYONDAPI.Byond_ReadVar(value, var.as_ptr() as _, &mut result) };
    success.then_some(result)
}

/// Return a value to BYOND, from an async `call_ext`.
///
/// This is used to return a value from an asynchronous callback to BYOND.
///
/// You need to decref the value after returning it if you want it to be freed:
///
/// ```no_run
/// # use sada_byondapi::{byond, sys::CByondValue};
/// # let waiting_proc = CByondValue::NULL;
/// let retval = byond::new(c"/datum/hello", &[]);
/// byond::r#return(waiting_proc, &retval);
/// byond::value_decref(&retval);
/// ```
#[cfg(feature = "byond-await")]
#[inline]
pub fn r#return(waiting_proc: CByondValue, value: &CByondValue) {
    unsafe { BYONDAPI.Byond_Return(&waiting_proc, value) };
}

/// Running work on BYOND's own thread from somewhere else.
///
/// Byondapi may only be touched from the main thread, so anything on the async runtime that needs it hands a closure
/// back through [`Byond_ThreadSync`](crate::sys::Byondapi::Byond_ThreadSync) rather than calling directly.
#[cfg(feature = "byond-await")]
pub mod sync {
    use std::{ffi::c_void, future::Future};

    use tokio::sync::oneshot;

    use crate::{BYONDAPI, sys::CByondValue};

    /// What is handed through [`Byond_ThreadSync`](crate::sys::Byondapi::Byond_ThreadSync) as its opaque argument: the
    /// work, and where to send its result.
    type DataPtr<R> = *mut (Box<dyn FnOnce() -> R>, oneshot::Sender<R>);

    /// Run `f` on BYOND's thread and wait for what it returns.
    pub fn with_main<F, R>(f: F) -> impl Future<Output = R>
    where
        F: FnOnce() -> R + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let func: Box<dyn FnOnce() -> R> = Box::new(f);
        let data: DataPtr<R> = Box::into_raw(Box::new((func, tx)));
        unsafe { BYONDAPI.Byond_ThreadSync(Some(thread_sync_callback::<R>), data as _, false) };
        async { rx.await.expect("failed to receive result from main_thread_callback") }
    }

    /// Callback [`with_main`] gives BYOND: run the work, answer whoever is waiting.
    extern "C-unwind" fn thread_sync_callback<R>(data: *mut c_void) -> CByondValue {
        let (f, tx) = *unsafe { Box::from_raw(data as DataPtr<R>) };
        let _ = tx.send(f());
        CByondValue::NULL
    }

    /// Run `f` on BYOND's thread without waiting for it.
    pub fn with_main_forget<F>(f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let func: Box<dyn FnOnce()> = Box::new(f);
        let data = Box::into_raw(Box::new(func));
        unsafe { BYONDAPI.Byond_ThreadSync(Some(thread_sync_callback_forget), data as _, false) };
    }

    /// Callback [`with_main_forget`] gives BYOND: run the work, drop the result.
    extern "C-unwind" fn thread_sync_callback_forget(data: *mut c_void) -> CByondValue {
        (unsafe { Box::from_raw(data as *mut Box<dyn FnOnce()>) })();
        CByondValue::NULL
    }

    /// [`byond::to_string`](super::to_string), from off the game thread.
    pub async fn to_string(value: CByondValue) -> String { with_main(move || super::to_string(&value)).await }
}
