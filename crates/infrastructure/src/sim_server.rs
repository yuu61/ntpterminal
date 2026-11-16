//! 開発・テスト用の疑似 NTP サーバと、疑似 SNMP エージェント（[`SimSnmpAgent`]）。

mod snmp_agent;

pub use snmp_agent::{SIM_COMMUNITY, SimSnmpAgent};

use alloc::sync::Arc;
use core::net::SocketAddr;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
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
    local_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SimServer {
    /// 疑似サーバを `bind` で起動し、別スレッドで応答させる。
    ///
    /// # Errors
    ///
    /// ソケットを用意できない、またはスレッドを起動できないとき。
    pub fn spawn(bind: SocketAddr, config: SimServerConfig) -> io::Result<Self> {
        let socket = UdpSocket::bind(bind)?;
        // 停止の合図を確認するため、定期的に受信待ちから戻る
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let local_addr = socket.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let handle = thread::Builder::new()
            .name("sim-ntp-server".into())
            .spawn({
                let thread_stop = Arc::clone(&stop);
                move || serve(&socket, config, &thread_stop)
            })?;
        Ok(Self {
            local_addr,
            stop,
            handle: Some(handle),
        })
    }

    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// サーバのスレッドが終わるまで待つ（CLI で動かし続ける用）。
    pub fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

impl Drop for SimServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

fn serve(socket: &UdpSocket, config: SimServerConfig, stop: &AtomicBool) {
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        // タイムアウトや、相手が消えたあとの ConnectionReset（Windows）は無視して続ける
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
