#![feature(macro_attr)]
#![allow(unused, missing_docs, clippy::missing_docs_in_private_items)]

use sada_byondapi::{BYONDAPI, byond, sys::CByondValue};

#[unsafe(no_mangle)]
pub extern "C-unwind" fn hello_world(argc: u32, argv: *mut CByondValue, waiting_proc: CByondValue) {
    let [arg1] = sada_byondapi::macros::__parse_args(argc, argv);

    println!("arg1 rc:\t{:?}", byond::ref_count(&arg1));

    byond::value_incref(&arg1);

    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(1));

        byond::sync::with_main_forget(move || {
            let retval = byond::new(c"/datum/hello", &[arg1]);
            byond::value_decref(&arg1);

            unsafe { BYONDAPI.Byond_Return(&waiting_proc, &retval) };
            byond::value_decref(&retval);
        })
    });
}
