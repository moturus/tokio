// Most (but not all) of the code here is copy-pasted from
// process/unix/mod.rs.
use crate::io::{AsyncRead, AsyncWrite, PollEvented, ReadBuf};
use crate::process::kill::Kill;
use crate::process::SpawnedChild;

use mio::event::Source;
use std::fmt;
use std::fs::File;
use std::future::Future;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, RawFd};
use std::pin::Pin;
use std::process::{Child as StdChild, ExitStatus, Stdio};
use std::task::Context;
use std::task::Poll;

#[derive(Debug)]
pub(super) struct Child {
    std_child: StdChild,
    async_child: PollEvented<Pipe>,
}

impl Child {
    pub(super) fn id(&self) -> u32 {
        self.std_child.id()
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.std_child.try_wait()
    }
}

impl Kill for Child {
    fn kill(&mut self) -> io::Result<()> {
        self.std_child.kill()
    }
}

impl Future for Child {
    type Output = io::Result<ExitStatus>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.async_child.registration().poll_read_ready(cx) {
            std::task::Poll::Ready(_) => std::task::Poll::Ready(
                self.std_child
                    .try_wait()
                    .map(|exit_status| exit_status.unwrap()),
            ),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

pub(crate) fn build_child(mut child: StdChild) -> io::Result<SpawnedChild> {
    let stdin = child.stdin.take().map(stdio).transpose()?;
    let stdout = child.stdout.take().map(stdio).transpose()?;
    let stderr = child.stderr.take().map(stdio).transpose()?;

    use std::os::motor::process::ChildExt;
    let child_fd: RawFd = moto_rt::fs::open(
        format!("handle://{}", child.sys_handle()).as_str(),
        moto_rt::fs::O_HANDLE_CHILD,
    )
    .map_err(map_motor_error)?;

    let pipe = Pipe {
        fd: unsafe { File::from_raw_fd(child_fd) },
    };
    let async_child = PollEvented::new_with_interest(pipe, crate::io::Interest::READABLE)?;

    Ok(SpawnedChild {
        child: Child {
            std_child: child,
            async_child,
        },
        stdin,
        stdout,
        stderr,
    })
}

#[derive(Debug)]
pub(crate) struct Pipe {
    // Actually a pipe is not a File. However, we are reusing `File` to get
    // close on drop. This is a similar trick as `mio`.
    fd: File,
}

impl<T: IntoRawFd> From<T> for Pipe {
    fn from(fd: T) -> Self {
        let fd = unsafe { File::from_raw_fd(fd.into_raw_fd()) };
        Self { fd }
    }
}

impl<'a> io::Read for &'a Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        (&self.fd).read(bytes)
    }
}

impl<'a> io::Write for &'a Pipe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (&self.fd).write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.fd).flush()
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        (&self.fd).write_vectored(bufs)
    }
}

impl AsRawFd for Pipe {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl AsFd for Pipe {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}

fn convert_to_blocking_file(io: ChildStdio) -> io::Result<File> {
    let mut fd = io.inner.into_inner()?.fd;

    // Ensure that the fd to be inherited is set to *blocking* mode, as this
    // is the default that virtually all programs expect to have. Those
    // programs that know how to work with nonblocking stdio will know how to
    // change it to nonblocking mode.
    set_nonblocking(&mut fd, false)?;

    Ok(fd)
}

pub(crate) fn convert_to_stdio(io: ChildStdio) -> io::Result<Stdio> {
    convert_to_blocking_file(io).map(Stdio::from)
}

impl Source for Pipe {
    fn register(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interest: mio::Interest,
    ) -> io::Result<()> {
        let mut ints = 0;
        if interest.is_readable() {
            ints |= moto_rt::poll::POLL_READABLE;
        }
        if interest.is_writable() {
            ints |= moto_rt::poll::POLL_WRITABLE;
        }
        moto_rt::poll::add(
            registry.as_raw_fd(),
            self.fd.as_raw_fd(),
            token.0 as u64,
            ints,
        )
        .map_err(map_motor_error)
    }

    fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interest: mio::Interest,
    ) -> io::Result<()> {
        let mut ints = 0;
        if interest.is_readable() {
            ints |= moto_rt::poll::POLL_READABLE;
        }
        if interest.is_writable() {
            ints |= moto_rt::poll::POLL_WRITABLE;
        }
        moto_rt::poll::set(
            registry.as_raw_fd(),
            self.fd.as_raw_fd(),
            token.0 as u64,
            ints,
        )
        .map_err(map_motor_error)
    }

    fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        moto_rt::poll::del(registry.as_raw_fd(), self.fd.as_raw_fd()).map_err(map_motor_error)
    }
}

pub(crate) struct ChildStdio {
    inner: PollEvented<Pipe>,
}

impl fmt::Debug for ChildStdio {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(fmt)
    }
}

impl AsRawFd for ChildStdio {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl AsFd for ChildStdio {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}

impl AsyncWrite for ChildStdio {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        self.inner.poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }
}

impl AsyncRead for ChildStdio {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Safety: pipes support reading into uninitialized memory
        unsafe { self.inner.poll_read(cx, buf) }
    }
}

fn set_nonblocking<T: AsRawFd>(fd: &mut T, nonblocking: bool) -> io::Result<()> {
    moto_rt::net::set_nonblocking(fd.as_raw_fd(), nonblocking).map_err(map_motor_error)?;

    Ok(())
}

pub(super) fn stdio<T>(io: T) -> io::Result<ChildStdio>
where
    T: IntoRawFd,
{
    // Set the fd to nonblocking before we pass it to the event loop
    let mut pipe = Pipe::from(io);
    set_nonblocking(&mut pipe, true)?;

    PollEvented::new(pipe).map(|inner| ChildStdio { inner })
}

fn map_motor_error(err: moto_rt::ErrorCode) -> std::io::Error {
    std::io::Error::from_raw_os_error(err.into())
}
