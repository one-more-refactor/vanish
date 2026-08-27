//! Raw NETLINK_KOBJECT_UEVENT socket.
//!
//! Lifted from `deadhand`, which uses the same socket to trigger a poweroff
//! instead of a lock. Same author, same reasons.
//!
//! The kernel broadcasts a datagram on group 1 for every device add/remove.
//! Reading it directly means we get instant USB-removal events with zero
//! dependency on udevd running or on libudev — which matters on a minimal /
//! amnesic system, and removes a component that could be stopped to blind us.
//!
//! Message wire format: an ASCII header line `ACTION@DEVPATH`, then a sequence
//! of NUL-separated `KEY=VALUE` properties.

use std::collections::HashMap;
use std::io;
use std::os::unix::io::RawFd;

const NETLINK_KOBJECT_UEVENT: libc::c_int = 15;

pub struct UeventSocket {
    fd: RawFd,
    buf: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct Uevent {
    pub action: String,
    pub devpath: String,
    pub subsystem: String,
    pub props: HashMap<String, String>,
}

impl UeventSocket {
    pub fn open() -> io::Result<UeventSocket> {
        // SOCK_DGRAM datagram socket in the kernel-object-uevent family.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                NETLINK_KOBJECT_UEVENT,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Grow the receive buffer so a storm of events can't drop the one that
        // matters. Best-effort.
        let rcvbuf: libc::c_int = 1 << 20;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUFFORCE,
                &rcvbuf as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }

        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as u16;
        addr.nl_groups = 1; // kernel broadcast group
        addr.nl_pid = 0; // let the kernel assign

        let rc = unsafe {
            libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            let err = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }

        Ok(UeventSocket {
            fd,
            buf: vec![0u8; 8192],
        })
    }

    /// Block until the next uevent arrives, then parse it.
    pub fn recv(&mut self) -> io::Result<Uevent> {
        loop {
            let n = unsafe {
                libc::recv(
                    self.fd,
                    self.buf.as_mut_ptr() as *mut libc::c_void,
                    self.buf.len(),
                    0,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            return Ok(parse_uevent(&self.buf[..n as usize]));
        }
    }
}

impl Drop for UeventSocket {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

fn parse_uevent(data: &[u8]) -> Uevent {
    let mut ev = Uevent::default();
    let mut first = true;
    for chunk in data.split(|&b| b == 0) {
        if chunk.is_empty() {
            continue;
        }
        let s = String::from_utf8_lossy(chunk);
        if first {
            // Header: "ACTION@DEVPATH". Ignore the older "libudev\0" monitor
            // prefix if present (won't appear on the kernel group).
            first = false;
            if let Some((action, devpath)) = s.split_once('@') {
                ev.action = action.to_string();
                ev.devpath = devpath.to_string();
            }
            continue;
        }
        if let Some((k, v)) = s.split_once('=') {
            match k {
                "ACTION" => ev.action = v.to_string(),
                "DEVPATH" => ev.devpath = v.to_string(),
                "SUBSYSTEM" => ev.subsystem = v.to_string(),
                _ => {}
            }
            ev.props.insert(k.to_string(), v.to_string());
        }
    }
    ev
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remove_event() {
        let mut data = Vec::new();
        data.extend_from_slice(b"remove@/devices/pci0000:00/usb1/1-1");
        data.push(0);
        data.extend_from_slice(b"ACTION=remove");
        data.push(0);
        data.extend_from_slice(b"DEVPATH=/devices/pci0000:00/usb1/1-1");
        data.push(0);
        data.extend_from_slice(b"SUBSYSTEM=usb");
        data.push(0);
        data.extend_from_slice(b"PRODUCT=1d6b/104/512");
        data.push(0);

        let ev = parse_uevent(&data);
        assert_eq!(ev.action, "remove");
        assert_eq!(ev.subsystem, "usb");
        assert_eq!(ev.devpath, "/devices/pci0000:00/usb1/1-1");
        assert_eq!(ev.props.get("PRODUCT").map(String::as_str), Some("1d6b/104/512"));
    }
}
