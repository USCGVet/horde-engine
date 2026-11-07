//! Socket setup shared by both endpoints.

use std::io;
use std::net::{SocketAddr, UdpSocket};

/// Socket buffer size. A snapshot for a big battle is dozens of datagrams sent in one burst; the
/// operating system's default (64 KB on Windows) would drop most of it.
const BUFFER_BYTES: i32 = 8 << 20;

/// A non-blocking UDP socket with large send and receive buffers.
pub fn open(bind: SocketAddr) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(bind)?;
    socket.set_nonblocking(true)?;
    set_buffers(&socket, BUFFER_BYTES);
    Ok(socket)
}

/// True for receive errors that only mean "nothing more right now" or that can be ignored. On Windows a
/// datagram sent to a closed port makes the next receive fail with ConnectionReset; that's not fatal for
/// a connectionless socket.
pub fn is_benign(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::ConnectionReset | io::ErrorKind::Interrupted)
}

#[cfg(windows)]
fn set_buffers(socket: &UdpSocket, bytes: i32) {
    use std::os::windows::io::AsRawSocket;
    const SOL_SOCKET: i32 = 0xFFFF;
    const SO_SNDBUF: i32 = 0x1001;
    const SO_RCVBUF: i32 = 0x1002;
    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn setsockopt(socket: usize, level: i32, name: i32, value: *const i32, len: i32) -> i32;
    }
    let raw = socket.as_raw_socket() as usize;
    for option in [SO_SNDBUF, SO_RCVBUF] {
        // SAFETY: `raw` is the live socket owned by `socket`, and `value` points at an i32 of the length
        // passed. A failure just leaves the default buffer size.
        unsafe { setsockopt(raw, SOL_SOCKET, option, &bytes, 4) };
    }
}

#[cfg(not(windows))]
fn set_buffers(_socket: &UdpSocket, _bytes: i32) {}
