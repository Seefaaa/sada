use std::{backtrace::Backtrace, borrow::Cow, cell::RefCell, sync::Once};

use crate::ffi::CByondValue;

#[doc(hidden)]
pub fn __parse_args<const N: usize>(argc: u32, argv: *mut CByondValue) -> [CByondValue; N] {
    let mut args = [CByondValue::NULL; N];
    for (i, arg) in args.iter_mut().enumerate().take((argc as usize).min(N)) {
        *arg = unsafe { *argv.add(i) };
    }
    args
}

#[macro_export]
macro_rules! byond_fn {
    (@args $argc:ident, $argv:ident,) => {};
    (@args $argc:ident, $argv:ident, $($arg:ident : $arg_ty:ty),+) => {
        let [$($arg),*] = $crate::macros::__parse_args::<{ $crate::macros::__count_args!($($arg),*) }>($argc, $argv);
        $(let $arg = <$arg_ty as From<$crate::ffi::CByondValue>>::from($arg);)*
    };

    (@res $res:ident -> $ret:ty) => {{ <$crate::ffi::CByondValue as From<$ret>>::from($res) }};
    (@res $res:ident ->) => {{ $crate::ffi::CByondValue::NULL }};

    (@catch $body:block) => {{
        $crate::macros::__setup_panic_hook();
        ::std::panic::catch_unwind(|| $body).unwrap_or_else(|_| {
            ($crate::ffi::BYOND.CRASH)($crate::macros::__panic_msg().as_ptr());
        })
    }};

    attr() (
        $(#[$met:meta])*
        fn $name:ident($($arg:ident : $arg_ty:ty),* $(,)?) $(-> $ret:ty)? $body:block
    ) => {
        ::paste::paste! {
            #[unsafe(no_mangle)]
            pub extern "C-unwind" fn $name(
                __argc: u32, __argv: *mut $crate::ffi::CByondValue,
            ) -> $crate::ffi::CByondValue {
                $crate::byond_fn!(@catch {
                    $crate::byond_fn!(@args __argc, __argv, $($arg : $arg_ty),*);
                    let __res = [<__ $name>]($($arg),*);
                    $crate::byond_fn!(@res __res -> $($ret)?)
                })
            }
            $(#[$met])*
            fn [<__ $name>]($($arg : $arg_ty),*) $(-> $ret)? $body
        }
    };

    attr() (
        $(#[$met:meta])*
        async fn $name:ident($($arg:ident : $arg_ty:ty),* $(,)?) $(-> $ret:ty)? $body:block
    ) => {
        ::paste::paste! {
            #[unsafe(no_mangle)]
            pub extern "C-unwind" fn $name(
                __argc: u32,
                __argv: *mut $crate::ffi::CByondValue,
                __waiting_proc: $crate::ffi::CByondValue,
            ) {
                $crate::byond_fn!(@catch {
                    $crate::byond_fn!(@args __argc, __argv, $($arg : $arg_ty),*);
                    $crate::runtime::runtime().spawn(async move {
                        let __res = [<__ $name>]($($arg),*).await;
                        let __ret = $crate::byond_fn!(@res __res -> $($ret)?);
                        ($crate::ffi::BYOND.Return)(&__waiting_proc, &__ret);
                    });
                });
            }
            $(#[$met])*
            async fn [<__ $name>]($($arg : $arg_ty),*) $(-> $ret)? $body
        }
    };
}

#[doc(hidden)]
macro_rules! __count_args {
    () => { 0 };
    ($head:ident $(, $tail:ident)*) => { 1 + $crate::macros::__count_args!($($tail),*) };
}

pub(crate) use __count_args;

thread_local! {
    static LAST_PANIC: RefCell<Option<(String, String, String)>> = const { RefCell::new(None) };
}

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

#[doc(hidden)]
pub fn __panic_msg() -> String {
    if let Some((msg, loc, bt)) = LAST_PANIC.with(|p| p.borrow_mut().take()) {
        return format!(
            "library '{}' panicked at: {loc}:\n{msg}\nstack backtrace:\n{bt}\0",
            env!("CARGO_CRATE_NAME")
        );
    }
    "\0".to_string()
}
