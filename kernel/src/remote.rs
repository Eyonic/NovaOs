//! Remote console for real PCs, which have no serial port to watch: with
//! `nova.remote=IP:PORT` on the kernel command line, Nova connects out to that address
//! (a listener such as `nc -lk PORT` on another machine) and treats the connection as
//! its serial line. Everything written to COM1 goes there too, starting with the whole
//! `dmesg` ring; bytes that come back are typed into the serial input, so the shell
//! works remotely. Nova opens no port: nothing on the network can reach it unasked.
//! The link reconnects when it drops.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::vec::Vec;
use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use smoltcp::wire::Ipv4Address;

use crate::net::{self, SockErr};
use crate::sync::SpinLock;
use crate::{input, pit, task};

/// Output waiting to be sent; the oldest bytes go first when the link is slow.
static TX: SpinLock<VecDeque<u8>> = SpinLock::new(VecDeque::new());
const TX_LIMIT: usize = 1 << 20;
/// Set while a link is up, so serial writes cost one load when there is none.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Called for every byte written to COM1.
pub fn push(bytes: &[u8]) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    // Never wait: output may come from a fault handler that interrupted the sender.
    if let Some(mut tx) = TX.try_lock() {
        let excess = (tx.len() + bytes.len()).saturating_sub(TX_LIMIT);
        let excess = excess.min(tx.len());
        tx.drain(..excess);
        tx.extend(bytes);
    }
}

/// Start the link if the command line asks for one (after networking is up).
pub fn start() {
    let Some(arg) = crate::setup::cmdline_flag("nova.remote") else { return };
    let Some((ip, port)) = parse(arg) else {
        crate::log::warn(&format!("nova.remote={arg}: expected IP:PORT"));
        return;
    };
    crate::println!("  remote console: connecting to {ip}:{port}");
    task::spawn("remote-console", move || loop {
        let _ = session(ip, port);
        ACTIVE.store(false, Ordering::Relaxed);
        task::sleep_ticks(3 * pit::HZ);
    });
}

fn parse(arg: &str) -> Option<(Ipv4Address, u16)> {
    let (ip, port) = arg.rsplit_once(':')?;
    let mut octets = [0u8; 4];
    let mut parts = ip.split('.');
    for o in &mut octets {
        *o = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some((Ipv4Address::new(octets[0], octets[1], octets[2], octets[3]), port.parse().ok()?))
}

/// One connection, until it fails or the peer closes it.
fn session(ip: Ipv4Address, port: u16) -> Result<(), SockErr> {
    if !net::wait_configured(10 * pit::HZ) {
        return Err(SockErr::NoNetwork);
    }
    let h = net::tcp_open()?;
    let r = (|| {
        net::tcp_connect(h, ip, port)?;
        let end = pit::ticks() + 10 * pit::HZ;
        loop {
            match net::tcp_connected(h) {
                Some(true) => break,
                Some(false) => return Err(SockErr::Refused),
                None if pit::ticks() >= end => return Err(SockErr::Refused),
                None => task::sleep_ticks(1),
            }
        }
        // The backlog first, then live output.
        let mut out: Vec<u8> = format!("\r\n[nova] remote console from {}; kernel log so far:\r\n", net::local_ip()).into_bytes();
        for line in crate::log::lines("") {
            out.extend(line.as_bytes());
            out.extend(b"\r\n");
        }
        out.extend(b"[nova] end of log; live output follows. Type commands as on the serial line.\r\n");
        TX.lock().clear();
        ACTIVE.store(true, Ordering::Relaxed);
        let mut buf = [0u8; 512];
        loop {
            if out.is_empty() {
                out.extend(TX.lock().drain(..));
            }
            let mut idle = true;
            if !out.is_empty() {
                match net::tcp_send(h, &out[..out.len().min(16 * 1024)]) {
                    Ok(n) => {
                        out.drain(..n);
                        idle = n == 0;
                    }
                    Err(SockErr::WouldBlock) => {}
                    Err(e) => return Err(e),
                }
            }
            match net::tcp_recv(h, &mut buf, false) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    idle = false;
                    // SERIAL_RX has one producer, the COM1 interrupt: keep it out.
                    without_interrupts(|| buf[..n].iter().for_each(|&b| input::SERIAL_RX.push(b)));
                }
                Err(SockErr::WouldBlock) => {}
                Err(e) => return Err(e),
            }
            if idle {
                task::sleep_ticks(1);
            }
        }
    })();
    ACTIVE.store(false, Ordering::Relaxed);
    net::sock_close(h, false);
    r
}

fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let flags: u64;
    // SAFETY: saving RFLAGS and clearing IF; restored below.
    unsafe { asm!("pushfq; pop {}; cli", out(reg) flags, options(nomem)) };
    let r = f();
    if flags & 0x200 != 0 {
        // SAFETY: interrupts were enabled before.
        unsafe { asm!("sti", options(nomem, nostack)) };
    }
    r
}
