//! Stream sockets the agent listens on: vsock in a VM, a unix socket in tests. Both are a file
//! descriptor that reads and writes, so one type serves them.

use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixListener;
use std::path::Path;

use rustix::net::addr::{SocketAddrArg, SocketAddrLen, SocketAddrOpaque};
use rustix::net::{AddressFamily, SendFlags, SocketFlags, SocketType};

/// A vsock address: rustix has none of its own.
struct Vsock {
    cid: u32,
    port: u32,
}

// SAFETY: with_sockaddr hands `f` a sockaddr_vm, whole, for the duration of the call.
unsafe impl SocketAddrArg for Vsock {
    unsafe fn with_sockaddr<R>(
        &self,
        f: impl FnOnce(*const SocketAddrOpaque, SocketAddrLen) -> R,
    ) -> R {
        // SAFETY: an all-zero sockaddr_vm is a valid one; then filled in.
        let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = self.port;
        addr.svm_cid = self.cid;
        f(
            (&raw const addr).cast(),
            size_of::<libc::sockaddr_vm>() as SocketAddrLen,
        )
    }
}

fn vsock_socket() -> io::Result<OwnedFd> {
    Ok(rustix::net::socket_with(
        AddressFamily::VSOCK,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?)
}

/// A connected stream.
pub struct Socket(OwnedFd);

impl Socket {
    /// A stream to vsock `port` of `cid` (the host is VMADDR_CID_HOST).
    pub fn connect_vsock(cid: u32, port: u32) -> io::Result<Socket> {
        let fd = vsock_socket()?;
        rustix::net::connect(&fd, &Vsock { cid, port })?;
        Ok(Socket(fd))
    }

    pub fn try_clone(&self) -> io::Result<Socket> {
        Ok(Socket(self.0.try_clone()?))
    }

    /// No more writes: the peer reads the end of the stream.
    pub fn shutdown_write(&self) {
        let _ = rustix::net::shutdown(&self.0, rustix::net::Shutdown::Write);
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(rustix::io::read(&self.0, buf)?)
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // MSG_NOSIGNAL: a peer gone is an error, not a SIGPIPE.
        Ok(rustix::net::send(&self.0, buf, SendFlags::NOSIGNAL)?)
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
        let fd = vsock_socket()?;
        let cid = libc::VMADDR_CID_ANY;
        rustix::net::bind(&fd, &Vsock { cid, port })?;
        rustix::net::listen(&fd, 128)?;
        Ok(Listener::Vsock(fd))
    }

    pub fn accept(&self) -> io::Result<Socket> {
        match self {
            Listener::Unix(listener) => Ok(Socket(listener.accept()?.0.into())),
            Listener::Vsock(fd) => Ok(Socket(rustix::net::accept_with(fd, SocketFlags::CLOEXEC)?)),
        }
    }
}
