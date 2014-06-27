//! A library for integrating native file descriptors into the Rust select pattern.
//!
//! Using this library it is possible to wait for activity on file descriptors and rust
//! channels at the same time.
//! 
//! # Example
//! 
//! ```rust
//! let fdpoll = FDPoll::new(3).unwrap();
//! fdpoll.add(0, Read).unwrap();
//! fdpoll.wait().unwrap();
//! 
//! let select = Select::new();
//! let mut handle = select.handle(&fdpoll.rcv);
//! unsafe { handle.add() };
//!
//! loop {
//!     let ret = select.wait();
//!     if ret == handle.id() {
//!         fdpoll.rcv.recv().unwrap();
//!         for e in fdpoll.events().unwrap().slice().iter() {
//!             println!("fd: {} read: {} write: {}", e.fd(), e.read(), e.write());
//!             if e.fd() == 0 {
//!                 println!(":::{}", std::io::stdin().read_line().ok());
//!             }
//!         }
//!         fdpoll.wait().unwrap();
//!     }
//! }
//! ```
//! This should always print `fd: 0 read: true write: false` when the user inputs a line.

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
