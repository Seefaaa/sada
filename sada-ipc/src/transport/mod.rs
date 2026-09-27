//! The platform transport under a channel.
//!
//! Both modules present the same two things, a [`Listener`] that binds and accepts and a [`connect`], and only the one
//! for the host is compiled. The streams they hand back differ in type but not in what a channel needs of them, so
//! nothing above this module names them.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{Listener, connect};
#[cfg(windows)]
pub use windows::{Listener, connect};
