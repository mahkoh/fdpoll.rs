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
    data: [u64, ..1u],
}

impl epoll_data {
    fn fd(&self) -> *const c_int {
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

extern {
    fn epoll_create1(flags: c_int) -> c_int;
    fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
    fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int,
                  timeout: c_int) -> c_int;
    fn eventfd(initval: c_uint, flags: c_int) -> c_int;
}

#[deriving(Show)]
/// Possible errors.
pub enum Error {
    /// Modification of the set failed because it's currently waiting for events.
    Waiting,
    /// Out of resources or some other error not specified here.
    Resources,
    /// `wait` was interrupted by a signal.
    Signal,
    /// Invalid file descriptor.
    BadFD,
    /// `add` was called with a file descriptor already in the set.
    Exists,
    /// `move` or `del` was called with a file descriptor not in the set.
    DoesntExist,
    /// The file descriptor doesn't support this operation.
    Unsupported,
    /// The process doesn't have the permission to perform this action.
    NoPermission,
}

/// Watch types.
pub enum Type {
    /// Wait for the descriptor to become read-ready.
    Read = EPOLLIN as int,
    /// Wait for the descriptor to become write-ready.
    Write = EPOLLOUT as int,
    /// Wait for the descriptor to become write- or read-ready.
    ReadWrite = EPOLLIN as int | EPOLLOUT as int,
}

/// A single event.
pub type Event = epoll_event;

impl epoll_event {
    /// Returns the file descriptor of this event.
    pub fn fd(&self) -> c_int {
        unsafe { *self.data.fd() }
    }

    /// Returns if the file descriptor is readable.
    pub fn read(&self) -> bool {
        self.events & (EPOLLIN as u32) != 0
    }

    /// Returns if the file descriptor is writable.
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

/// Container for the events made available by the last wait.
pub struct Events<'a> {
    lock: RWLockReadGuard<'a, Vec<Event>>,
}

impl<'a> Events<'a> {
    /// Get the events create by the last wait.
    pub fn slice<'b>(&'b self) -> &'b [Event] {
        self.lock.as_slice()
    }
}

/// Poll set.
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
    /// Create a new set that can handle at most `max` events at the same time.
    ///
    /// Note that you can add arbitrarily many file descriptors to the set (as far as the
    /// operating system allows,) but every `wait()` stores at most `max` events.
    /// 
    /// Returns `Err` if the underlying structure couldn't be created.
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
                        let mut buf: [u8, ..8] = unsafe { uninitialized() };
                        if unsafe { read(abort2, buf.as_mut_ptr() as *mut _, 8) <= 0 } {
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

    /// Abort the current `wait()`.
    /// 
    /// Returns `Err` if `wait()` isn't running.
    pub fn abort(&self) -> Result<(), ()> {
        if !self.running.load(Relaxed) {
            return Err(());
        }
        unsafe {
            let buf = 1u64;
            write(self.abort.fd, &buf as *const _ as *const _, 8);
        }
        Ok(())
    }

    /// Return the container containing the events generated by the last `wait()`.
    ///
    /// Returns `Err` if `wait()` is currently running.
    pub fn events<'a>(&'a self) -> Result<Events<'a>, ()> {
        if self.running.load(Relaxed) {
            return Err(());
        }
        Ok(Events { lock: self.events.read() })
    }

    /// Start the wait process.
    ///
    /// Returns `Err` if `wait()` is currently running.
    pub fn wait(&self) -> Result<(), ()> {
        if self.running.load(Relaxed) {
            return Err(());
        }
        self.sem.release();
        Ok(())
    }

    /// Add a file descriptor to the set.
    ///
    /// Returns `Err` if `wait()` is currently running or if an underlying error occurs.
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

    /// Modify a file descriptor in the set.
    ///
    /// Returns `Err` if `wait()` is currently running or if an underlying error occurs.
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

    /// Remove a file descriptor from the set.
    ///
    /// Returns `Err` if `wait()` is currently running or if an underlying error occurs.
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
