extern crate fdpoll;
extern crate libc;
extern crate debug;

use fdpoll::{FDPoll, Read, Write};
use std::comm::{Select};

fn main() {
    let mut fds = [0, 0];
    unsafe { libc::pipe(fds.as_mut_ptr()); }
    let fdpoll = FDPoll::new(10).unwrap();
    fdpoll.add(fds[0], Read).unwrap();
    fdpoll.add(fds[1], Write).unwrap();
    fdpoll.wait().unwrap();

    let select = Select::new();
    let mut handle = select.handle(&fdpoll.rcv);
    unsafe { handle.add() };

    loop {
        let ret = select.wait();
        if ret == handle.id() {
            fdpoll.rcv.recv().unwrap();
            for e in fdpoll.events().unwrap().slice().iter() {
                println!("fd: {} read: {} write: {}", e.fd(), e.read(), e.write());
                if e.fd() == 0 {
                    println!(":::{}", std::io::stdin().read_line().ok());
                }
                if e.fd() == fds[1] {
                    fdpoll.delete(fds[1]).unwrap();
                    fdpoll.add(0, Read).unwrap();
                    unsafe { libc::write(fds[1], b"huhu".as_ptr() as *_, 4); } 
                }
                if e.fd() == fds[0] {
                    let mut buf = [0, 0, 0, 0, 0];
                    unsafe { libc::read(fds[0], buf.as_mut_ptr() as *mut _, 4); }
                    unsafe { libc::puts(buf.as_ptr() as *_); }
                }
            }
            fdpoll.wait().unwrap();
        }
    }
}
