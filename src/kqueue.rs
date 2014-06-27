extern crate libc;

use libc::{c_int, c_uint, close, read, write};
use libc::consts::os::posix88::{EEXIST, EBADF, ENOENT, EPERM};
use std::mem::{transmute, uninitialized};
use std::os::{errno};
use std::sync::{Arc, RWLock, RWLockReadGuard, Semaphore};
use std::sync::atomics::{AtomicBool, Relaxed};
use std::task::{TaskBuilder};
use native::task::{NativeTaskBuilder};

static EPOLLIN: c_int = 1;
static EPOLLOUT: c_int = 4;

static EPOLL_CTL_ADD: c_int = 1;
static EPOLL_CTL_DEL: c_int = 2;
static EPOLL_CTL_MOD: c_int = 3;

static EFD_NONBLOCK: c_int = 2048;

#[repr(C)]
#[allow(non_camel_case_types)]
struct epoll_data {
    pub data: [u64, ..1u],
}

impl epoll_data {
    fn fd(&self) -> *c_int {
        unsafe { transmute(self) }
    }

    fn fd_mut(&mut self) -> *mut c_int {
        unsafe { transmute(self) }
    }
}

#[repr(C)]
#[allow(non_camel_case_types)]
struct epoll_event {
    events: u32,
    data: epoll_data,
}

extern "C" {
    fn epoll_create1(flags: c_int) -> c_int;
    fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
    fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int,
                  timeout: c_int) -> c_int;
    fn eventfd(initval: c_uint, flags: c_int) -> c_int;
}

#[deriving(Show)]
pub enum Error {
    Waiting,
    Resources,
    Signal,
    BadFD,
    Exists,
    DoesntExist,
    Unsupported,
}

pub enum Type {
    Read = EPOLLIN,
    Write = EPOLLOUT,
    ReadWrite = EPOLLIN | EPOLLOUT,
}

pub type Event = epoll_event;

impl epoll_event {
    pub fn fd(&self) -> c_int {
        unsafe { *self.data.fd() }
    }

    pub fn read(&self) -> bool {
        self.events & (EPOLLIN as u32) != 0
    }

    pub fn write(&self) -> bool {
        self.events & (EPOLLOUT as u32) != 0
    }
}

struct FD {
    fd: c_int,
}

impl Drop for FD {
    fn drop(&mut self) {
        unsafe { close(self.fd); }
    }
}

pub struct Events<'a> {
    lock: RWLockReadGuard<'a, Vec<Event>>,
}

impl<'a> Events<'a> {
    pub fn slice<'b>(&'b self) -> &'b [Event] {
        self.lock.as_slice()
    }
}

pub struct FDPoll {
    pub rcv: Receiver<Result<(), Error>>,

    running: Arc<AtomicBool>,
    sem: Arc<Semaphore>,
    epfd: FD,
    events: Arc<RWLock<Vec<Event>>>,
    abort: FD,
    stop: Arc<AtomicBool>,
}

impl FDPoll {
    pub fn new(max: uint) -> Result<FDPoll, Error> {
        // Create epoll
        let epfd = match unsafe { epoll_create1(0) } {
            -1 => return Err(Resources),
            n => FD { fd: n },
        };

        // Create abort
        let abort = match unsafe { eventfd(0, EFD_NONBLOCK) } {
            -1 => return Err(Resources),
            n => FD { fd: n },
        };

        // Register abort
        let mut e: Event = unsafe { uninitialized() };
        unsafe { *e.data.fd_mut() = abort.fd; }
        e.events = EPOLLIN as u32;
        if unsafe { epoll_ctl(epfd.fd, EPOLL_CTL_ADD, abort.fd,
                              &mut e as *mut _) } == -1 {
            return Err(Resources);
        }

        let sem = Arc::new(Semaphore::new(0));
        let events = Arc::new(RWLock::new(Vec::with_capacity(max)));
        let running = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (snd, rcv) = channel();

        let sem2     = sem.clone();
        let events2  = events.clone();
        let running2 = running.clone();
        let stop2    = stop.clone();
        let abort2   = abort.fd;
        let epfd2    = epfd.fd;
        TaskBuilder::new().native().spawn(proc() {
            loop {
                sem2.acquire();
                if stop2.load(Relaxed) {
                    break;
                }
                running2.store(true, Relaxed);
                let mut events2 = events2.write();
                unsafe { events2.set_len(0); }
                let r = unsafe {
                    epoll_wait(epfd2, events2.as_mut_ptr(), events2.capacity() as i32, -1)
                };
                running2.store(false, Relaxed);
                match r {
                    -1 => snd.send(Err(Signal)),
                    n => {
                        unsafe { events2.set_len(n as uint); }
                        if events2.mut_iter().any(|e| unsafe { *e.data.fd() } == abort2) {
                            unsafe {
                                let mut buf = [0u8, ..8];
                                read(abort2, buf.as_mut_ptr() as *mut _, 8);
                            }
                        } else {
                            snd.send(Ok(()));
                        }
                    }
                }
            }
        });

        Ok(FDPoll {
            running: running,
            sem: sem,
            epfd: epfd,
            events: events,
            abort: abort,
            stop: stop,
            rcv: rcv,
        })
    }

    pub fn abort(&self) -> Result<(), ()> {
        if !self.running.load(Relaxed) {
            return Err(());
        }
        unsafe {
            let buf = 1u64;
            write(self.abort.fd, &buf as *_ as *_, 8);
        }
        Ok(())
    }

    pub fn events<'a>(&'a self) -> Result<Events<'a>, ()> {
        if self.running.load(Relaxed) {
            return Err(());
        }
        Ok(Events { lock: self.events.read() })
    }

    pub fn wait(&self) -> Result<(), ()> {
        if self.running.load(Relaxed) {
            return Err(());
        }
        self.sem.release();
        Ok(())
    }

    pub fn add(&self, fd: c_int, ty: Type) -> Result<(), Error> {
        if self.running.load(Relaxed) {
            return Err(Waiting);
        }
        let mut e: Event = unsafe { uninitialized() };
        unsafe { *e.data.fd_mut() = fd };
        e.events = ty as u32;
        match unsafe { epoll_ctl(self.epfd.fd, EPOLL_CTL_ADD, fd, &mut e as *mut _) } {
            -1 => {
                match errno() as i32 {
                    EBADF => Err(BadFD),
                    EEXIST => Err(Exists),
                    EPERM => Err(Unsupported),
                    _ => Err(Resources),
                }
            },
            _ => Ok(()),
        }
    }

    pub fn modify(&self, fd: c_int, ty: Type) -> Result<(), Error> {
        if self.running.load(Relaxed) {
            return Err(Waiting);
        }
        let mut e: Event = unsafe { uninitialized() };
        unsafe { *e.data.fd_mut() = fd };
        e.events = ty as u32;
        match unsafe { epoll_ctl(self.epfd.fd, EPOLL_CTL_MOD, fd, &mut e as *mut _) } {
            -1 => match errno() as i32 {
                EBADF => Err(BadFD),
                ENOENT => Err(DoesntExist),
                _ => Err(Resources),
            },
            _ => Ok(()),
        }
    }

    pub fn delete(&self, fd: c_int) -> Result<(), Error> {
        if self.running.load(Relaxed) {
            return Err(Waiting);
        }
        match unsafe { epoll_ctl(self.epfd.fd, EPOLL_CTL_DEL, fd, 0 as *mut _) } {
            -1 => match errno() as i32 {
                EBADF => Err(BadFD),
                ENOENT => Err(DoesntExist),
                _ => Err(Resources),
            },
            _ => Ok(()),
        }
    }
}

impl Drop for FDPoll {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        self.sem.release();
    }
}
