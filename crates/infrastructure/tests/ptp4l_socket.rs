//! ptp4l の管理用ソケットへの送受信の結合テスト（Linux のみ・root 不要）。
//!
//! 一時ディレクトリにファイルのソケットで待ち受ける模擬 ptp4l を立て、ptp4l と同じく「受け取った送り元の
//! アドレスへ」応答を返す。こちらは抽象名前空間の名前で bind しているので、その名前に応答が届くことを確かめる。
#![cfg(target_os = "linux")]

extern crate alloc;

use alloc::sync::Arc;
use core::time::Duration;
use std::io;
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::{env, fs, process, thread};

use application::port::{ClockPort, PacketPort};
use application::usecase::{Ptp4lClient, Ptp4lClientConfig, Ptp4lError};
use domain::ptp_clock::PortState;
use domain::ptp_management::{
    Action, DefaultDataSet, ManagementId, ManagementMessage, ManagementTlv, PortHwclockNp,
    PortIdentity, PortPropertiesNp, decode, encode,
};
use infrastructure::clock::SystemClock;
use infrastructure::ptp4l;

/// 一時ディレクトリの中のソケットの場所（テストごとに別）。
fn socket_path(name: &str) -> io::Result<PathBuf> {
    let dir = env::temp_dir().join(format!("ntpterminal-test-{}-{name}", process::id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir)?;
    Ok(dir.join("ptp4l"))
}

fn remove(path: &Path) {
    if let Some(dir) = path.parent() {
        drop(fs::remove_dir_all(dir));
    }
}

/// 模擬 ptp4l の応答。答えない問い合わせなら `None`。
fn answer(request: &ManagementMessage) -> Option<ManagementMessage> {
    let ManagementTlv::Management { id, .. } = request.tlv else {
        return None;
    };
    let port = PortIdentity {
        clock: [1; 8],
        port: request.target.port,
    };
    let data = match id {
        ManagementId::DEFAULT_DATA_SET => DefaultDataSet {
            number_ports: 1,
            ..DefaultDataSet::default()
        }
        .to_bytes(),
        ManagementId::PORT_PROPERTIES_NP => PortPropertiesNp {
            port,
            port_state: 9,
            timestamping: 1,
            interface: b"eth0".to_vec(),
        }
        .to_bytes(),
        ManagementId::PORT_HWCLOCK_NP => PortHwclockNp {
            port,
            phc_index: 3i32,
            flags: 0,
        }
        .to_bytes(),
        _ => return None,
    };
    Some(ManagementMessage {
        target: request.source,
        action: Action::Response,
        tlv: ManagementTlv::Management { id, data },
        ..request.clone()
    })
}

/// 模擬 ptp4l のスレッドの結果: 受け取った問い合わせと、送り元がすべて抽象名前空間の名前だったか。
type ServerResult = io::Result<(Vec<ManagementMessage>, bool)>;

/// 模擬 ptp4l: `count` 回だけ問い合わせに答え、受け取った問い合わせと、送り元がすべて
/// 抽象名前空間の名前だったかを返す（ポート 1 つ、eth0、/dev/ptp3）。
fn spawn_ptp4l(path: &Path, count: usize) -> io::Result<thread::JoinHandle<ServerResult>> {
    let server = UnixDatagram::bind(path)?;
    server.set_read_timeout(Some(Duration::from_secs(5)))?;
    Ok(thread::spawn(move || {
        let mut received = Vec::new();
        let mut all_abstract = true;
        let mut buffer = [0u8; 1500];
        for _ in 0..count {
            let (len, from) = server.recv_from(&mut buffer)?;
            let request = decode(&buffer[..len])
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            all_abstract &= from
                .as_abstract_name()
                .is_some_and(|name| name.starts_with(b"ntpterminal."));
            if let Some(reply) = answer(&request) {
                // ptp4l と同じく、受け取った送り元のアドレスへ返す
                let _sent = server.send_to_addr(&encode(&reply), &from)?;
            }
            received.push(request);
        }
        Ok((received, all_abstract))
    }))
}

fn client(path: &Path, timeout: Duration) -> io::Result<Ptp4lClient<Box<dyn PacketPort>>> {
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let connection = ptp4l::connect(Some(path), Arc::clone(&clock))?;
    if connection.path != path {
        return Err(io::Error::other("指定した場所につないでいない"));
    }
    Ok(Ptp4lClient::new(
        connection.port,
        clock,
        Ptp4lClientConfig {
            domain: 0,
            timeout,
            client_id: process::id(),
        },
    ))
}

#[test]
fn replies_reach_the_abstract_address() {
    let path = socket_path("reply").unwrap();
    let server = spawn_ptp4l(&path, 3).unwrap();
    let ports = client(&path, Duration::from_secs(2))
        .unwrap()
        .discover_ports()
        .unwrap();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].interface, "eth0");
    assert_eq!(ports[0].state, PortState::Slave);
    assert_eq!(ports[0].hwclock.map(|clock| clock.index), Some(3i32));
    let (received, all_abstract) = server.join().unwrap().unwrap();
    assert!(all_abstract, "送り元は抽象名前空間の名前のはず");
    assert_eq!(received.len(), 3);
    assert!(
        received.iter().all(|request| request.boundary_hops == 0),
        "ptp4l の外へ転送させない"
    );
    remove(&path);
}

#[test]
fn a_silent_ptp4l_times_out() {
    let path = socket_path("silent").unwrap();
    // 受け取るだけで答えない
    let _server = UnixDatagram::bind(&path).unwrap();
    let error = client(&path, Duration::from_millis(50))
        .unwrap()
        .discover_ports()
        .unwrap_err();
    assert_eq!(
        error,
        Ptp4lError::Timeout {
            id: ManagementId::DEFAULT_DATA_SET,
        }
    );
    remove(&path);
}

#[test]
fn a_dead_ptp4l_is_reported_at_connect() {
    let path = socket_path("dead").unwrap();
    // ソケットのファイルはあるが、誰も待ち受けていない（ptp4l が落ちた後）
    drop(UnixDatagram::bind(&path).unwrap());
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let error = ptp4l::connect(Some(&path), clock).unwrap_err();
    assert!(
        error.to_string().contains("ptp4l が動いていません"),
        "{error}"
    );
    remove(&path);
}
