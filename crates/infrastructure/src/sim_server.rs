//! 開発・テスト用の疑似 NTP サーバと、疑似 SNMP エージェント（[`SimSnmpAgent`]）。
//!
//! どちらも受信は無期限に待ち、止めるときは自分宛てに空のデータグラムを送って起こす（`ServeThread`）。
//! 受信タイムアウトで停止の合図を確かめる作りにすると、Windows ではタイムアウトと要求の到着が重なったときに
//! 要求を取りこぼす（100 ms のタイムアウトのサーバに 100 ms 間隔で問い合わせると 100 回中 9 回。
//! タイムアウトのない同じサーバでは 0 回）。

mod snmp_agent;

pub use snmp_agent::{SIM_COMMUNITY, SimSnmpAgent};

use alloc::sync::Arc;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use core::sync::atomic::{AtomicBool, Ordering};
use std::io;
use std::net::UdpSocket;
use std::thread::{self, JoinHandle};
use std::time::SystemTime;

use domain::packet_crafting::{NtpMessage, decode, encode};
use domain::shared_kernel::{NtpTimestamp, TimeDelta};

use crate::clock::system_time_to_ntp;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimServerConfig {
    /// PC の壁時計に対して、サーバ時刻をどれだけ進めるか（負なら遅らせる）
    pub offset: TimeDelta,
    pub stratum: u8,
    pub reference_id: [u8; 4],
}

impl Default for SimServerConfig {
    fn default() -> Self {
        Self {
            offset: TimeDelta::ZERO,
            stratum: 1,
            reference_id: *b"SIM\0",
        }
    }
}

/// 疑似 NTP サーバ。クライアント要求（mode 3）に「PC の壁時計 + offset」で応答する。
///
/// 実機のタイムサーバがなくても、計測や TUI の表示を試せる。drop すると止まる。
#[derive(Debug)]
pub struct SimServer {
    thread: ServeThread,
}

impl SimServer {
    /// 疑似サーバを `bind` で起動し、別スレッドで応答させる。
    ///
    /// # Errors
    ///
    /// ソケットを用意できない、またはスレッドを起動できないとき。
    pub fn spawn(bind: SocketAddr, config: SimServerConfig) -> io::Result<Self> {
        let thread = ServeThread::spawn(
            "sim-ntp-server",
            UdpSocket::bind(bind)?,
            move |socket, stop| serve(socket, config, stop),
        )?;
        Ok(Self { thread })
    }

    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.thread.local_addr
    }

    /// サーバのスレッドが終わるまで待つ（CLI で動かし続ける用）。
    pub fn join(self) {
        self.thread.join();
    }
}

/// UDP のソケットで要求を待ち続けるスレッド。drop すると止める。
#[derive(Debug)]
struct ServeThread {
    local_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ServeThread {
    /// `serve` は、停止の合図（第 2 引数）が立つまで受信と応答を続けること。
    fn spawn<F>(name: &str, socket: UdpSocket, serve: F) -> io::Result<Self>
    where
        F: FnOnce(&UdpSocket, &AtomicBool) + Send + 'static,
    {
        let local_addr = socket.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let handle = thread::Builder::new().name(name.into()).spawn({
            let thread_stop = Arc::clone(&stop);
            move || serve(&socket, &thread_stop)
        })?;
        Ok(Self {
            local_addr,
            stop,
            handle: Some(handle),
        })
    }

    fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

impl Drop for ServeThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(handle) = self.handle.take() else {
            return;
        };
        // 起こせなかったときは待たない（待つと戻らなくなる）。スレッドは受信を待ったまま残り、プロセスの終了で片付く
        if wake(self.local_addr).is_ok() {
            drop(handle.join());
        }
    }
}

/// `target` で受信を待っているソケットに、空のデータグラムを送って起こす。
///
/// 待ち受けのアドレスが 0.0.0.0 / `::` ならループバックに送る。ループバックに送るときは、送信元も
/// ループバックに bind する（Windows のファイアウォールの確認を避ける）。
fn wake(target: SocketAddr) -> io::Result<()> {
    let (source, destination) = wake_route(target);
    let socket = UdpSocket::bind(source)?;
    let _: usize = socket.send_to(&[], destination)?;
    Ok(())
}

/// 起こすデータグラムの（送信元, 宛先）。
fn wake_route(target: SocketAddr) -> (SocketAddr, SocketAddr) {
    let (loopback, unspecified): (IpAddr, IpAddr) = if target.is_ipv4() {
        (Ipv4Addr::LOCALHOST.into(), Ipv4Addr::UNSPECIFIED.into())
    } else {
        (Ipv6Addr::LOCALHOST.into(), Ipv6Addr::UNSPECIFIED.into())
    };
    let destination = if target.ip().is_unspecified() {
        SocketAddr::new(loopback, target.port())
    } else {
        target
    };
    let source = if destination.ip().is_loopback() {
        loopback
    } else {
        unspecified
    };
    (SocketAddr::new(source, 0), destination)
}

fn serve(socket: &UdpSocket, config: SimServerConfig, stop: &AtomicBool) {
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        // 相手が消えたあとの ConnectionReset（Windows）は無視して続ける。止めるときの空のデータグラムは
        // 解析できないので読み捨て、ループの先頭で停止の合図を見る
        let Ok((len, peer)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let receive = server_now(config.offset);
        let Ok(request) = decode(&buf[..len]) else {
            continue;
        };
        if request.mode != NtpMessage::MODE_CLIENT {
            continue;
        }
        let reply = NtpMessage {
            leap: 0,
            version: request.version,
            mode: NtpMessage::MODE_SERVER,
            stratum: config.stratum,
            poll: request.poll,
            precision: -20,
            root_delay: 0,
            root_dispersion: 0x0000_0010,
            reference_id: config.reference_id,
            reference_timestamp: receive.wrapping_add(TimeDelta::from_secs(-1)),
            origin_timestamp: request.transmit_timestamp,
            receive_timestamp: receive,
            transmit_timestamp: server_now(config.offset),
            trailing: Vec::new(),
        };
        drop(socket.send_to(&encode(&reply), peer));
    }
}

fn server_now(offset: TimeDelta) -> NtpTimestamp {
    system_time_to_ntp(SystemTime::now()).wrapping_add(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;
    use std::time::Instant;

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn wakes_through_loopback_when_listening_on_any_address() {
        assert_eq!(
            wake_route(addr("0.0.0.0:12300")),
            (addr("127.0.0.1:0"), addr("127.0.0.1:12300"))
        );
        assert_eq!(
            wake_route(addr("[::]:12300")),
            (addr("[::1]:0"), addr("[::1]:12300"))
        );
        assert_eq!(
            wake_route(addr("127.0.0.1:12300")),
            (addr("127.0.0.1:0"), addr("127.0.0.1:12300"))
        );
        assert_eq!(
            wake_route(addr("192.168.1.5:12300")),
            (addr("0.0.0.0:0"), addr("192.168.1.5:12300"))
        );
    }

    #[test]
    fn stops_promptly_without_a_receive_timeout() {
        let server = SimServer::spawn(addr("127.0.0.1:0"), SimServerConfig::default()).unwrap();
        let local = server.local_addr();
        let started = Instant::now();
        drop(server);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "止まるまで {:?}",
            started.elapsed()
        );
        // スレッドが終わってソケットが閉じたので、同じポートでまた待ち受けられる
        drop(UdpSocket::bind(local).unwrap());
    }
}
