#![crate_id = "fdpoll"]
#![crate_type = "lib"]
#![feature(globs)]

extern crate libc;
extern crate native;

pub use fdpoll::*;

#[cfg(target_os = "linux")]
#[cfg(target_os = "android")]
#[path = "epoll.rs"]
mod fdpoll;

#[cfg(target_os = "macos")]
#[cfg(target_os = "ios")]
#[cfg(target_os = "freebsd")]
#[path = "kqueue.rs"]
mod fdpoll;
