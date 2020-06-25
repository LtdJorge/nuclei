use std::mem::MaybeUninit;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::{fs::File, os::unix::net::UnixStream, collections::HashMap, time::Duration};
use crate::sys::event::{kevent_ts, kqueue, KEvent};
use futures::channel::oneshot;
use pin_utils::unsafe_pinned;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use lever::prelude::*;

macro_rules! syscall {
    ($fn:ident $args:tt) => {{
        let res = unsafe { libc::$fn $args };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

type CompletionList = Vec<(usize, oneshot::Sender<usize>)>;

pub struct Proactor {
    /// kqueue_fd
    kqueue_fd: RawFd,

    ///
    read_stream: TTas<UnixStream>,

    ///
    write_stream: UnixStream,

    /// Registered events of IOs
    registered: TTas<HashMap<RawFd, usize>>,

    /// Hashmap for holding interested concrete completion callbacks
    completions: TTas<HashMap<RawFd, CompletionList>>
}

impl Proactor {
    fn new() -> io::Result<Proactor> {
        let kqueue_fd = kqueue()?;
        syscall!(fcntl(kqueue_fd, libc::F_SETFD, libc::FD_CLOEXEC))?;
        let (read_stream, write_stream) = UnixStream::pair()?;
        read_stream.set_nonblocking(true)?;
        write_stream.set_nonblocking(true)?;
        let proactor = Proactor {
            kqueue_fd,
            read_stream: TTas::new(read_stream),
            write_stream,
            registered: TTas::new(HashMap::new()),
            completions: TTas::new(HashMap::new())
        };

        let mut rs = proactor.read_stream.lock();
        proactor.reregister(rs.as_raw_fd(), !0)?;
        drop(rs);

        Ok(proactor)
    }

    pub fn register(&self, fd: RawFd, _key: usize) -> io::Result<()> {
        let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
        syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
        Ok(())
    }

    pub fn reregister(&self, fd: RawFd, key: usize) -> io::Result<()> {
        let mut read_flags = libc::EV_ONESHOT | libc::EV_RECEIPT;
        let mut write_flags = libc::EV_ONESHOT | libc::EV_RECEIPT;
        read_flags |= libc::EV_ADD;
        write_flags |= libc::EV_ADD;

        let udata = key as _;
        let changelist = [
            KEvent::new(fd as _, libc::EVFILT_READ, read_flags, 0, 0, udata),
            KEvent::new(fd as _, libc::EVFILT_WRITE, write_flags, 0, 0, udata),
        ];
        let mut eventlist = changelist;
        kevent_ts(self.kqueue_fd, &changelist, &mut eventlist, None)?;
        for ev in &eventlist {
            // Explanation for ignoring EPIPE: https://github.com/tokio-rs/mio/issues/582
            let (flags, data) = (ev.flags(), ev.data());
            if (flags & libc::EV_ERROR) == 1
                && data != 0
                && data != libc::ENOENT as _
                && data != libc::EPIPE as _
            {
                return Err(io::Error::from_raw_os_error(data as _));
            }
        }

        Ok(())
    }

    pub fn deregister(&self, fd: RawFd) -> io::Result<()> {
        let flags = libc::EV_DELETE | libc::EV_RECEIPT;
        let changelist = [
            KEvent::new(fd as _, libc::EVFILT_WRITE, flags, 0, 0, 0),
            KEvent::new(fd as _, libc::EVFILT_READ, flags, 0, 0, 0),
        ];
        let mut eventlist = changelist;
        kevent_ts(self.kqueue_fd, &changelist, &mut eventlist, None)?;
        for ev in &eventlist {
            let (flags, data) = (ev.flags(), ev.data());
            if (flags & libc::EV_ERROR == 1) && data != 0 && data != libc::ENOENT as _ {
                return Err(io::Error::from_raw_os_error(data as _));
            }
        }
        Ok(())
    }

    pub fn wait(&self, maxsize: usize, timeout: Option<Duration>) -> io::Result<usize> {
        let timeout = timeout.map(|t| libc::timespec {
            tv_sec: t.as_secs() as libc::time_t,
            tv_nsec: t.subsec_nanos() as libc::c_long,
        });

        let mut events: Vec<KEvent> = Vec::with_capacity(maxsize);
        events.resize(maxsize, unsafe { MaybeUninit::zeroed().assume_init() });

        let res = kevent_ts(self.kqueue_fd, &[], events.as_mut_slice(), timeout)?;
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut res = res as usize;
        let mut rs = self.read_stream.lock();

        for event in &events[0..res] {
            if event.data() == 0 {
                let _ = rs.read(&mut [0; 64]);
                res -= 1;
                continue;
            }
            let raw_fd = event.data() as _;
            self.dequeue_events(raw_fd, event.udata() as usize);
        }

        Ok(res)
    }

    pub fn wake(&self) -> io::Result<()> {
        let _ = (&self.write_stream).write(&[1]);
        Ok(())
    }

    ///////

    fn register_io(&self, fd: RawFd, evts: usize) -> io::Result<CompletionChan> {
        let mut registered = self.registered.lock();
        let mut completions = self.completions.lock();

        // register/reregister events.
        {
            let mut evts = evts;
            if let Some(reged_evts) = registered.get_mut(&fd) {
                evts |= *reged_evts;
                self.reregister(fd, evts)?;
                *reged_evts = evts;
            } else {
                self.register(fd, evts)?;
                registered.insert(fd, evts);
            }
        }

        let (sender, receiver) = oneshot::channel();
        let comp = completions
            .entry(fd)
            .or_insert(Vec::new());

        comp.push((evts, sender));

        Ok(CompletionChan { recv: receiver })
    }

    fn dequeue_events(&self, fd: RawFd, evts: usize) {
        // acquire locks.
        let mut regs = self.registered.lock();
        let mut completions = self.completions.lock();

        // remove flags from interested events.
        let mut remove_regs = false;
        if let Some(reg_events) = regs.get_mut(&fd) {
            *reg_events &= !evts;
            if *reg_events == 0 {
                remove_regs = true;
            } else {
                let _ = self.reregister(fd, *reg_events);
            }
        }

        // send concrete completion and remove completion interested sources
        let mut ack_removal = false;
        if let Some(completions) = completions.get_mut(&fd) {
            let mut i = 0;
            while i < completions.len() {
                if completions[i].0 & evts != 0 {
                    let (_evts, sender) = completions.remove(i);
                    let _ = sender.send(evts);
                } else {
                    i += 1;
                }
            }

            if completions.is_empty() {
                ack_removal = true;
            }
        }

        if remove_regs {
            regs.remove(&fd);
            self.deregister(fd);
        }

        if ack_removal {
            completions.remove(&fd);
        }

    }
}

//////////////////////////////
//////////////////////////////


pub struct Events {
    list: Box<[KEvent]>,
    len: usize,
}

impl Events {
    pub fn new() -> Events {
        let flags = 0;
        let event = KEvent::new(0, 0, flags, 0, 0, 0);
        let list = vec![event; 1000].into_boxed_slice();
        let len = 0;
        Events { list, len }
    }

    pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
        // On some platforms, closing the read end of a pipe wakes up writers, but the
        // event is reported as EVFILT_READ with the EV_EOF flag.
        //
        // https://github.com/golang/go/commit/23aad448b1e3f7c3b4ba2af90120bde91ac865b4
        self.list[..self.len].iter().map(|ev| Event {
            readable: ev.filter() == libc::EVFILT_READ,
            writable: ev.filter() == libc::EVFILT_WRITE
                || (ev.filter() == libc::EVFILT_READ && (ev.flags() & libc::EV_EOF) != 0),
            key: ev.udata() as usize,
        })
    }
}

pub struct Event {
    pub readable: bool,
    pub writable: bool,
    pub key: usize,
}



//////////////////////////////
//////////////////////////////

pub(crate) struct CompletionChan {
    recv: oneshot::Receiver<usize>,
}

impl CompletionChan {
    unsafe_pinned!(recv: oneshot::Receiver<usize>);
}

impl Future for CompletionChan {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.recv()
            .poll(cx)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "sender has been canceled"))
    }
}
