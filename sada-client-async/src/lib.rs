#![feature(macro_attr)]
#![allow(unused, missing_docs, clippy::missing_docs_in_private_items)]

mod ffi;
mod macros;
mod runtime;
mod shutdown;

use std::{
    mem::{MaybeUninit, zeroed},
    sync::OnceLock,
};

use tokio::sync::oneshot;

use crate::{
    ffi::{BYOND, CByondValue},
    runtime::runtime,
    shutdown::Shutdown,
};

#[byond_fn]
async fn example_async_function(num: i32) -> i32 {
    let (tx, rx) = oneshot::channel();

    runtime().spawn(async move {
        let _ = tx.send(num);
    });

    rx.await.unwrap()
}

#[byond_fn]
fn shutdown() { runtime::shutdown(); }

#[byond_fn]
fn panicing() {
    panic!("this is a test panic");
}
