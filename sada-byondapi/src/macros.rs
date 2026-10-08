//! The macro that turns a Rust function into something BYOND can call.

use std::{backtrace::Backtrace, borrow::Cow, cell::RefCell, panic::PanicHookInfo, sync::Once};
#[cfg(feature = "byond-await")]
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    task::Poll,
};

use crate::sys::CByondValue;
#[cfg(feature = "byond-await")]
use crate::{BYONDAPI, byond};

/// Define a function BYOND can call as `call_ext(lib, "byond:name")`.
///
/// The generated export takes byond's argument array by converting each argument through [`From`].
/// Missing arguments arrive as null rather than as an error.
///
/// The body runs inside `catch_unwind`: unwinding across the FFI boundary is undefined behaviour, so a panic is turned
/// into a DM runtime error through [`CRASH`](crate::sys::crash) instead.
///
/// Two further shapes are called as `call_ext(lib, "byond,await:name")`, which sleeps the calling proc until the
/// answer arrives (feature `byond-await`):
///
/// - `async fn name(..) -> T` runs its whole body on the runtime.
/// - `fn name(..) -> impl Future<Output = T>`, spelled exactly so, runs its body on BYOND's thread and awaits only the
///   future it returns, for work that has to happen in order with the export calls around it.
///
/// A panic in the future, or in turning its output into a value, has no proc left to become a runtime error in, so the
/// waiting proc is answered with the panic message as text instead.
#[macro_export]
macro_rules! byond_fn {
    (@args $argc:ident, $argv:ident,) => {};
    (@args $argc:ident, $argv:ident, $($arg:ident : $arg_ty:ty),+) => {
        let [$($arg),*] = $crate::macros::__parse_args($argc, $argv);
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

    (@doc
        [$($prefix:ident)?] [$name:ident]
        [$($arg0:ident : $arg0_ty:ty $(, $arg:ident : $arg_ty:ty)*)?]
        [$($ret:ty)?]
    ) => {
        concat!(
            $(concat!(stringify!($prefix), " "),)? "fn ", stringify!($name), "(", $(
                stringify!($arg0), ": ", stringify!($arg0_ty),
                $(", ", stringify!($arg), ": ", stringify!($arg_ty),)*
            )? ")", $(concat!(" -> ", stringify!($ret)))?
        )
    };

    (@group [$($doc:tt)*] [$($other:tt)*] #[doc = $d:expr] $($rest:tt)*) => {
        $crate::byond_fn!(@group [$($doc)* #[doc = $d]] [$($other)*] $($rest)* );
    };

    (@group [$($doc:tt)*] [$($other:tt)*] #[$($any:tt)*] $($rest:tt)*) => {
        $crate::byond_fn!(@group [$($doc)*] [$($other)* #[$($any)*]] $($rest)*);
    };

    (@await_export [$(#[doc = $doc:expr])*] [$($signature:tt)*] $name:ident($($arg:ident : $arg_ty:ty),*)) => {
        $crate::paste::paste! {
            $(#[doc = $doc])*
            #[doc = concat!("```ignore\n", $($signature)*, "\n```")]
            #[allow(missing_docs, clippy::missing_safety_doc)]
            #[unsafe(no_mangle)]
            pub extern "C-unwind" fn $name(
                __argc: u32, __argv: *mut $crate::sys::CByondValue, __waiting_proc: $crate::sys::CByondValue,
            ) {
                $crate::byond_fn!(@catch {
                    $crate::byond_fn!(@args __argc, __argv, $($arg : $arg_ty),*);
                    $crate::macros::__spawn([<__ $name>]($($arg),*), __waiting_proc);
                })
            }
        }
    };

    (@export [$(#[doc = $doc:expr])*] [$($signature:tt)*] $name:ident($($arg:ident : $arg_ty:ty),*) $(-> $ret:ty)?) => {
        $crate::paste::paste! {
            $(#[doc = $doc])*
            #[doc = concat!("```ignore\n", $($signature)*, "\n```")]
            #[allow(missing_docs, clippy::missing_safety_doc)]
            #[unsafe(no_mangle)]
            pub extern "C-unwind" fn $name(
                __argc: u32, __argv: *mut $crate::sys::CByondValue,
            ) -> $crate::sys::CByondValue {
                $crate::byond_fn!(@catch {
                    $crate::byond_fn!(@args __argc, __argv, $($arg : $arg_ty),*);
                    let __res = [<__ $name>]($($arg),*);
                    $crate::byond_fn!(@res __res -> $($ret)?)
                })
            }
        }
    };

    (@group
        [$(#[doc = $doc:expr])*]
        [$(#[$met:meta])*]
        fn $name:ident($($arg:ident : $arg_ty:ty),* $(,)?) -> impl Future<Output = $ret:ty> $body:block
    ) => {
        $crate::byond_fn!(@await_export
            [$(#[doc = $doc])*]
            [$crate::byond_fn!(@doc [] [$name] [$($arg : $arg_ty),*] [impl Future<Output = $ret>])]
            $name($($arg : $arg_ty),*)
        );
        $crate::paste::paste! {
            $(#[$met])*
            #[doc(hidden)]
            #[inline(always)]
            fn [<__ $name>]($($arg : $arg_ty),*) -> impl ::std::future::Future<Output = $ret> $body
        }
    };

    (@group
        [$(#[doc = $doc:expr])*]
        [$(#[$met:meta])*]
        async fn $name:ident($($arg:ident : $arg_ty:ty),* $(,)?) $(-> $ret:ty)? $body:block
    ) => {
        $crate::byond_fn!(@await_export
            [$(#[doc = $doc])*]
            [$crate::byond_fn!(@doc [async] [$name] [$($arg : $arg_ty),*] [$($ret)?])]
            $name($($arg : $arg_ty),*)
        );
        $crate::paste::paste! {
            $(#[$met])*
            #[doc(hidden)]
            async fn [<__ $name>]($($arg : $arg_ty),*) $(-> $ret)? $body
        }
    };

    (@group
        [$(#[doc = $doc:expr])*]
        [$(#[$met:meta])*]
        fn $name:ident($($arg:ident : $arg_ty:ty),* $(,)?) $(-> $ret:ty)? $body:block
    ) => {
        $crate::byond_fn!(@export
            [$(#[doc = $doc])*]
            [$crate::byond_fn!(@doc [] [$name] [$($arg : $arg_ty),*] [$($ret)?])]
            $name($($arg : $arg_ty),*)
        );
        $crate::paste::paste! {
            $(#[$met])*
            #[doc(hidden)]
            #[inline(always)]
            fn [<__ $name>]($($arg : $arg_ty),*) $(-> $ret)? $body
        }
    };

    (@group [$($doc:tt)*] [$($other:tt)*] $($rest:tt)*) => {
        compile_error!(concat!(
            "byond_fn takes `fn name(arg: Ty, ..) -> Ty { .. }`, optionally async or returning `impl Future<Output = Ty>`, but got: ",
            stringify!($($rest)*)
        ));
    };

    ($($tt:tt)*) => { $crate::byond_fn!(@group [] [] $($tt)*); };

    attr() ($($tt:tt)*) => { $crate::byond_fn!($($tt)*); };
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
    INIT.call_once(|| std::panic::set_hook(Box::new(stash_panic)));
}

/// The hook [`__setup_panic_hook`] installs: keep what the panic said for [`__panic_msg`].
fn stash_panic(info: &PanicHookInfo) {
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

/// Run `future` on the runtime and answer `waiting_proc` with its output once it is done.
///
/// Used by the async shapes of [`byond_fn`].
#[doc(hidden)]
#[cfg(feature = "byond-await")]
pub fn __spawn<F>(future: F, waiting_proc: CByondValue)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    CByondValue: From<F::Output>,
{
    crate::runtime::runtime().spawn(async move {
        let output = __catch(future).await;
        let data: *mut (Result<F::Output, String>, CByondValue) = Box::into_raw(Box::new((output, waiting_proc)));
        unsafe { BYONDAPI.Byond_ThreadSync(Some(__return::<F::Output>), data as _, false) };
    });
}

/// Drive `future`, turning a panic in any of its polls into the message [`__panic_msg`] renders.
#[cfg(feature = "byond-await")]
async fn __catch<F: Future>(future: F) -> Result<F::Output, String> {
    let mut future = std::pin::pin!(future);

    std::future::poll_fn(|cx| match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx))) {
        Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
        Ok(Poll::Pending) => Poll::Pending,
        Err(_) => Poll::Ready(Err(__panic_msg())),
    })
    .await
}

/// Callback [`__spawn`] gives [`Byond_ThreadSync`](crate::sys::Byondapi::Byond_ThreadSync): answer the waiting proc
/// on BYOND's thread.
///
/// This runs on no proc's stack, so nothing here may unwind into BYOND and a panic becomes the answer instead.
#[doc(hidden)]
#[cfg(feature = "byond-await")]
extern "C-unwind" fn __return<R>(data: *mut std::ffi::c_void) -> CByondValue
where
    CByondValue: From<R>,
{
    let (output, waiting_proc) = *unsafe { Box::from_raw(data as *mut (Result<R, String>, CByondValue)) };

    let value = output
        .and_then(|output| catch_unwind(AssertUnwindSafe(|| output.into())).map_err(|_| __panic_msg()))
        .unwrap_or_else(|message| catch_unwind(|| message.into()).unwrap_or(CByondValue::NULL));

    unsafe { BYONDAPI.Byond_Return(&waiting_proc, &value) };

    byond::value_decref(&value);

    CByondValue::NULL
}

#[cfg(all(test, feature = "byond-await"))]
mod tests {
    use super::*;

    /// Keeps the `async fn` shape compiling.
    #[byond_fn]
    async fn an_async_export(value: String) -> String { value }

    /// Keeps the `impl Future` shape compiling.
    #[byond_fn]
    fn a_future_export(value: String) -> impl Future<Output = String> { async move { value } }

    /// Drive `future` to completion, as the runtime would.
    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime for the test")
            .block_on(future)
    }

    #[test]
    fn a_future_that_finishes_hands_over_its_output() {
        assert_eq!(block_on(__catch(async { 7 })), Ok(7));
    }

    /// Install, once for the whole binary, a hook that stashes a panic as the export hook does and still prints it.
    fn stash_and_print_panics() {
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            let print = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                stash_panic(info);
                print(info);
            }));
        });
    }

    #[test]
    fn a_future_that_panics_hands_over_the_message() {
        stash_and_print_panics();

        let caught: Result<(), String> = block_on(__catch(async { panic!("the future gave up") }));

        let message = caught.expect_err("the panic is caught rather than taking the task down");
        assert!(message.contains("the future gave up"), "got {message}");
    }
}
