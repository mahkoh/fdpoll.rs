extern crate libc;

use libc::{c_void, c_int, c_uint, intptr_t, uintptr_t, c_short, timespec, close, read,
           write, pipe};
use libc::consts::os::posix88::{EBADF, ENOENT, EACCES};
use std::mem::{uninitialized, zeroed};
use std::os::{errno};
use std::sync::{Arc, RWLock, RWLockReadGuard, Semaphore};
use std::sync::atomics::{AtomicBool, Relaxed};
use std::task::{TaskBuilder};
use native::task::{NativeTaskBuilder};

static EVFILT_READ: c_short = -1;
static EVFILT_WRITE: c_short = -2;

static EV_ADD: c_short = 1;
static EV_DELETE: c_short = 2;

// For testing with libkqueue on linux.
#[cfg(target_os = "linux")]
#[cfg(target_os = "android")]
static O_NONBLOCK: c_int = 0o4000;
#[cfg(target_os = "macos")]
#[cfg(target_os = "ios")]
#[cfg(target_os = "freebsd")]
static O_NONBLOCK: c_int = 4;

#[repr(C)]
#[allow(non_camel_case_types)]
struct kevent {
    ident:  uintptr_t,   /* identifier for this event */
    filter: c_short,     /* filter for event */
    flags:  c_short,     /* action flags for kqueue*/
    fflags: c_uint,      /* filter flag value */
    data:   intptr_t,    /* filter data value */
    udata:  *mut c_void, /* opaque user data identifier */
}

#[cfg(target_os = "linux")]
#[cfg(target_os = "android")]
#[link(name="kqueue")]
extern {
    fn kqueue() -> c_int;
    fn kevent(kq: c_int, changelist: *kevent, nchanges: c_int, eventlist: *mut kevent,
              nevents: c_int, timeout: *timespec) -> c_int;
    fn pipe2(filedes: *mut [c_int, ..2], flags: c_int) -> c_int;
}

#[cfg(target_os = "macos")]
#[cfg(target_os = "ios")]
#[cfg(target_os = "freebsd")]
extern {
    fn kqueue() -> c_int;
    fn kevent(kq: c_int, changelist: *kevent, nchanges: c_int, eventlist: *mut kevent,
              nevents: c_int, timeout: *timespec) -> c_int;
    fn pipe2(filedes: *mut [c_int, ..2], flags: c_int) -> c_int;
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
    Read,
    /// Wait for the descriptor to become write-ready.
    Write,
    /// Wait for the descriptor to become write- or read-ready.
    ReadWrite,
}

/// A single event.
pub type Event = kevent;

impl kevent {
    /// Returns the file descriptor of this event.
    pub fn fd(&self) -> c_int {
        self.ident as c_int
    }

    /// Returns if the file descriptor is readable.
    pub fn read(&self) -> bool {
        self.filter == EVFILT_READ
    }

    /// Returns if the file descriptor is writable.
    pub fn write(&self) -> bool {
        self.filter == EVFILT_WRITE
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
        let epfd = match unsafe { kqueue() } {
            -1 => return Err(Resources),
            n => FD { fd: n },
        };

        // Create abort
        let mut pipe = [0, 0]; 
        let read_end;
        let write_end;
        match unsafe { pipe2(&mut pipe, O_NONBLOCK) } {
            -1 => return Err(Resources),
            _ => {
                read_end = FD { fd: pipe[0] };
                write_end = FD { fd: pipe[1] };
            },
        }

        // Register abort
        let mut e: Event = unsafe { zeroed() };
        e.ident = read_end.fd as uintptr_t;
        e.filter = EVFILT_READ;
        e.flags = EV_ADD;
        if unsafe { kevent(epfd.fd, &e as *_, 1, 0 as *mut _, 0, 0 as *_) == -1 } {
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
                    kevent(epfd2, 0 as *_, 0, events2.as_mut_ptr(),
                           events2.capacity() as c_int, 0 as *_)
                };
                running2.store(false, Relaxed);
                match r {
                    -1 => snd.send(Err(Signal)),
                    n => {
                        unsafe { events2.set_len(n as uint); }
                        let mut buf: [u8, ..8] = unsafe { uninitialized() };
                        let ptr = buf.as_mut_ptr() as *mut _;
                        if unsafe { read(read_end.fd, ptr, 8) <= 0 } {
                            snd.send(Ok(()));
                        } else {
                            while unsafe { read(read_end.fd, ptr, 8) > 0 } { }
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
            abort: write_end,
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
            let buf = [0u8];
            write(self.abort.fd, buf.as_ptr() as *_, 1);
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
        let mut e: [Event, ..2] = unsafe { uninitialized() };
        e[0].ident = fd as uintptr_t;
        e[0].filter = EVFILT_READ;
        e[0].flags = EV_ADD;
        e[1].ident = fd as uintptr_t;
        e[1].filter = EVFILT_WRITE;
        e[1].flags = EV_ADD;
        let (ptr, len) = unsafe {
            match ty {
                Read      => (e.as_ptr(),           1),
                Write     => (e.as_ptr().offset(1), 1),
                ReadWrite => (e.as_ptr(),           2),
            }
        };
        match unsafe { kevent(self.epfd.fd, ptr, len, 0 as *mut _, 0, 0 as *_) } {
            -1 => {
                match errno() as i32 {
                    EBADF => Err(BadFD),
                    EACCES => Err(NoPermission),
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
        self.add(fd, ty)
    }

    /// Remove a file descriptor from the set.
    ///
    /// Returns `Err` if `wait()` is currently running or if an underlying error occurs.
    pub fn delete(&self, fd: c_int) -> Result<(), Error> {
        if self.running.load(Relaxed) {
            return Err(Waiting);
        }
        let mut not_found = false;
        let mut e: [Event, ..1] = unsafe { uninitialized() };
        e[0].ident = fd as uintptr_t;
        e[0].filter = EVFILT_READ;
        e[0].flags = EV_DELETE;
        match unsafe { kevent(self.epfd.fd, e.as_ptr(), 1, 0 as *mut _, 0, 0 as *_) } {
            -1 => match errno() as i32 {
                EBADF => return Err(BadFD),
                ENOENT => not_found = true,
                _ => return Err(Resources),
            },
            _ => { },
        }
        e[0].filter = EVFILT_WRITE;
        match unsafe { kevent(self.epfd.fd, e.as_ptr(), 1, 0 as *mut _, 0, 0 as *_) } {
            -1 => match errno() as i32 {
                EBADF => Err(BadFD),
                ENOENT if not_found => Err(DoesntExist),
                _ => Err(Resources),
            },
            _ => Ok(())
        }
    }
}

impl Drop for FDPoll {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        self.sem.release();
    }
}
