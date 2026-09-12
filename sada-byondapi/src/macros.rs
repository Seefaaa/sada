//! The macro that turns a Rust function into something BYOND can call.

use std::{backtrace::Backtrace, borrow::Cow, cell::RefCell, sync::Once};

use crate::sys::CByondValue;

/// Define a function BYOND can call as `call_ext(lib, "byond:name")`.
///
/// The generated export takes byond's argument array by converting each argument through [`From`].
/// Missing arguments arrive as null rather than as an error.
///
/// The body runs inside `catch_unwind`: unwinding across the FFI boundary is undefined behaviour, so a panic is turned
/// into a DM runtime error through [`CRASH`](crate::sys::crash) instead.
#[macro_export]
macro_rules! byond_fn {
    (@args $argc:ident, $argv:ident,) => {};
    (@args $argc:ident, $argv:ident, $($arg:ident : $arg_ty:ty),+) => {
        let [$($arg),*] = $crate::macros::__parse_args::<{ $crate::__count_args!($($arg),*) }>($argc, $argv);
        $(let $arg = <$arg_ty as From<$crate::sys::CByondValue>>::from($arg);)*
    };

    (@res $res:ident -> $ret:ty) => {{ <$crate::sys::CByondValue as From<$ret>>::from($res) }};
    (@res $res:ident ->) => {{ $crate::sys::CByondValue::NULL }};

    (@catch $body:block) => {{
        $crate::macros::__setup_panic_hook();
        ::std::panic::catch_unwind(|| $body).unwrap_or_else(|_| {
            $crate::sys::crash($crate::macros::__panic_msg());
        })
    }};

    (
        $(#[$met:meta])*
        fn $name:ident($($arg:ident : $arg_ty:ty),* $(,)?) $(-> $ret:ty)? $body:block
    ) => {
        $crate::paste::paste! {
            #[unsafe(no_mangle)]
            #[allow(missing_docs, clippy::missing_safety_doc)]
            pub extern "C" fn $name(
                __argc: u32, __argv: *mut $crate::sys::CByondValue,
            ) -> $crate::sys::CByondValue {
                $crate::byond_fn!(@catch {
                    $crate::byond_fn!(@args __argc, __argv, $($arg : $arg_ty),*);
                    let __res = [<__ $name>]($($arg),*);
                    $crate::byond_fn!(@res __res -> $($ret)?)
                })
            }
            $(#[$met])*
            #[inline(always)]
            fn [<__ $name>]($($arg : $arg_ty),*) $(-> $ret)? $body
        }
    };

    attr() ($($tt:tt)*) => { $crate::byond_fn!($($tt)*); };
}

/// Count the identifiers it is handed, at compile time.
///
/// [`byond_fn`] needs that count as a const generic for [`__parse_args`].
#[doc(hidden)]
#[macro_export]
macro_rules! __count_args {
    () => { 0 };
    ($head:ident $(, $tail:ident)*) => { 1 + $crate::__count_args!($($tail),*) };
}

/// Copy BYOND's argument array into a fixed-size one.
///
/// A short call is ordinary rather than exceptional: DM lets a caller pass fewer arguments than the proc reads, so
/// whatever BYOND did not supply stays null instead of being an error, and anything past what the function declared
/// is ignored. Taking the smaller of the two counts is also what keeps this from reading past the array BYOND
/// described, which is why it can be safe despite the raw pointer.
#[doc(hidden)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn __parse_args<const N: usize>(argc: u32, argv: *mut CByondValue) -> [CByondValue; N] {
    let mut args = [CByondValue::NULL; N];
    for (i, arg) in args.iter_mut().enumerate().take((argc as usize).min(N)) {
        *arg = unsafe { *argv.add(i) };
    }
    args
}

thread_local! {
    /// What the last panic said, as message, location and backtrace, waiting to be turned into a DM runtime error.
    static LAST_PANIC: RefCell<Option<(String, String, String)>> = const { RefCell::new(None) };
}

/// Install the hook that puts a panic somewhere [`__panic_msg`] can find it.
///
/// The capture has to happen here rather than where the panic is caught: by the time `catch_unwind` hands back, the
/// location and the backtrace are gone and only the payload is left. This replaces the default hook rather than
/// wrapping it, so a panic no longer prints to a stderr nobody is reading; it reaches the player as a runtime error
/// instead.
#[doc(hidden)]
pub fn __setup_panic_hook() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let msg = info
                .payload()
                .downcast_ref::<&str>()
                .map(|&s| Cow::Borrowed(s))
                .or_else(|| info.payload().downcast_ref::<String>().map(|s| Cow::Owned(s.clone())))
                .unwrap_or(Cow::Borrowed("<unknown panic>"));

            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_default();
            let backtrace = Backtrace::force_capture();

            LAST_PANIC.set(Some((msg.into_owned(), location, backtrace.to_string())));
        }));
    });
}

/// Take the stashed panic and render it for [`CRASH`](crate::sys::crash).
#[doc(hidden)]
pub fn __panic_msg() -> String {
    if let Some((msg, loc, bt)) = LAST_PANIC.with(|p| p.borrow_mut().take()) {
        return format!(
            "library '{}' panicked at: {loc}:\n{msg}\nstack backtrace:\n{bt}",
            env!("CARGO_CRATE_NAME")
        );
    }
    String::new()
}
