//! Stream sockets the agent listens on: vsock in a VM, a unix socket in tests. Both are a file
//! descriptor that reads and writes, so one type serves them.

use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::Path;

/// A connected stream.
pub struct Socket(OwnedFd);

impl Socket {
    pub fn try_clone(&self) -> io::Result<Socket> {
        Ok(Socket(self.0.try_clone()?))
    }

    /// No more writes: the peer reads the end of the stream.
    pub fn shutdown_write(&self) {
        // SAFETY: on a descriptor this owns.
        unsafe { libc::shutdown(self.0.as_raw_fd(), libc::SHUT_WR) };
    }
}

fn check(ret: isize) -> io::Result<usize> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: reads into `buf`, at most its length.
        check(unsafe { libc::read(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) })
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: writes from `buf`, at most its length. MSG_NOSIGNAL: a peer gone is an error,
        // not a SIGPIPE.
        check(unsafe {
            libc::send(
                self.0.as_raw_fd(),
                buf.as_ptr().cast(),
                buf.len(),
                libc::MSG_NOSIGNAL,
            )
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A listening socket.
pub enum Listener {
    Unix(UnixListener),
    Vsock(OwnedFd),
}

impl Listener {
    pub fn unix(path: &Path) -> io::Result<Listener> {
        let _ = std::fs::remove_file(path);
        Ok(Listener::Unix(UnixListener::bind(path)?))
    }

    /// Listens on vsock `port`, for connections from the host.
    pub fn vsock(port: u32) -> io::Result<Listener> {
        // SAFETY: plain socket calls on a descriptor this creates; the address is zeroed, then
        // filled in.
        unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(fd);
            let mut addr: libc::sockaddr_vm = zeroed();
            addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
            addr.svm_port = port;
            addr.svm_cid = libc::VMADDR_CID_ANY;
            let len = size_of::<libc::sockaddr_vm>() as libc::socklen_t;
            if libc::bind(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_vm).cast(),
                len,
            ) < 0
                || libc::listen(fd.as_raw_fd(), 128) < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Listener::Vsock(fd))
        }
    }

    pub fn accept(&self) -> io::Result<Socket> {
        match self {
            Listener::Unix(listener) => Ok(Socket(listener.accept()?.0.into())),
            Listener::Vsock(fd) => {
                // SAFETY: accept4 with no address wanted, on the listening descriptor.
                let conn = unsafe {
                    libc::accept4(
                        fd.as_raw_fd(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        libc::SOCK_CLOEXEC,
                    )
                };
                if conn < 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: a descriptor accept4 just returned, owned by nothing else.
                Ok(Socket(unsafe { OwnedFd::from_raw_fd(conn) }))
            }
        }
    }
}
