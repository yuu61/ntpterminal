//! 開発・テスト用の疑似 SNMP エージェント。GNSS 受信機の MIB の値を返す。
//!
//! - SNMP v2c の GET と GETNEXT に答える（`snmpwalk -v2c -c public 127.0.0.1:16100` でも見られる）。
//!   ほかの PDU には genErr を返す
//! - コミュニティ名は `public` だけを受け付け、違えば答えない（実機と同じく黙って捨てる）
//! - 値は MIB の対応表（`application::acl::mib`）の OID に置き、TUI の表示を試せるよう衛星の数を時間とともに
//!   少しずつ変える。S650 の衛星の一覧は、実機と同じく 128 バイトで切る（衛星が多いと内訳が出なくなる）

use alloc::sync::Arc;
use core::iter;
use core::net::SocketAddr;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::io;
use std::net::UdpSocket;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use application::acl::mib::MibProfile;
use application::acl::mib::meinberg::{self, lt, ltng, os};
use application::acl::mib::microchip::{self, s650};
use domain::snmp::{ObjectId, Pdu, PduKind, SnmpMessage, SnmpValue, VarBind, decode, encode};

/// 受け付けるコミュニティ名。
pub const SIM_COMMUNITY: &[u8] = b"public";
/// 衛星の数を変える間隔（秒）。
const STEP_SECS: u64 = 20;
/// S650 の `gnssSatlist` の宣言上の長さ（実機はここで切れる）。
const SATLIST_MAX_LEN: usize = 128;
/// `sysDescr.0` / `sysObjectID.0` / `sysUpTime.0`
const SYS_DESCR: [u32; 9] = [1, 3, 6, 1, 2, 1, 1, 1, 0];
const SYS_OBJECT_ID: [u32; 9] = [1, 3, 6, 1, 2, 1, 1, 2, 0];
const SYS_UP_TIME: [u32; 9] = [1, 3, 6, 1, 2, 1, 1, 3, 0];

/// 疑似 SNMP エージェント。drop すると止まる。
#[derive(Debug)]
pub struct SimSnmpAgent {
    local_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SimSnmpAgent {
    /// `bind` で待ち受け、`profile` の MIB の値を返す。
    ///
    /// # Errors
    ///
    /// ソケットを用意できない、またはスレッドを起動できないとき。
    pub fn spawn(bind: SocketAddr, profile: MibProfile) -> io::Result<Self> {
        let socket = UdpSocket::bind(bind)?;
        // 停止の合図を確認するため、定期的に受信待ちから戻る
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let local_addr = socket.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let handle = thread::Builder::new()
            .name("sim-snmp-agent".into())
            .spawn({
                let thread_stop = Arc::clone(&stop);
                move || serve(&socket, profile, &thread_stop)
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
}

impl Drop for SimSnmpAgent {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

fn serve(socket: &UdpSocket, profile: MibProfile, stop: &AtomicBool) {
    let started = Instant::now();
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        // タイムアウトや、相手が消えたあとの ConnectionReset（Windows）は無視して続ける
        let Ok((len, peer)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let Ok(request) = decode(&buf[..len]) else {
            continue;
        };
        let objects = objects(profile, started.elapsed().as_secs());
        if let Some(response) = respond(&request, &objects) {
            drop(socket.send_to(&encode(&response), peer));
        }
    }
}

/// 要求への応答。v2c で、コミュニティ名が合うときだけ答える。
fn respond(request: &SnmpMessage, objects: &[VarBind]) -> Option<SnmpMessage> {
    if request.version != SnmpMessage::VERSION_2C || request.community != SIM_COMMUNITY {
        return None;
    }
    let asked = request.pdu.varbinds.iter().map(|varbind| &varbind.name);
    let (error_status, varbinds) = match request.pdu.kind {
        PduKind::GetRequest => (
            Pdu::NO_ERROR,
            asked.map(|name| get(objects, name)).collect(),
        ),
        PduKind::GetNextRequest => (
            Pdu::NO_ERROR,
            asked.map(|name| get_next(objects, name)).collect(),
        ),
        PduKind::Response
        | PduKind::SetRequest
        | PduKind::GetBulkRequest
        | PduKind::InformRequest
        | PduKind::Trap
        | PduKind::Report
        | PduKind::Other(_) => (Pdu::GEN_ERR, request.pdu.varbinds.clone()),
    };
    Some(SnmpMessage {
        version: request.version,
        community: request.community.clone(),
        pdu: Pdu {
            kind: PduKind::Response,
            request_id: request.pdu.request_id,
            error_status,
            // エラーなら、原因として最初の varbind を指す
            error_index: i64::from(error_status != Pdu::NO_ERROR),
            varbinds,
        },
    })
}

fn get(objects: &[VarBind], name: &ObjectId) -> VarBind {
    objects
        .iter()
        .find(|object| object.name == *name)
        .cloned()
        .unwrap_or_else(|| VarBind {
            name: name.clone(),
            value: SnmpValue::NoSuchObject,
        })
}

/// `name` より後ろ（辞書順）で最初のオブジェクト。`objects` は OID の順に並んでいること。
fn get_next(objects: &[VarBind], name: &ObjectId) -> VarBind {
    objects
        .iter()
        .find(|object| object.name > *name)
        .cloned()
        .unwrap_or_else(|| VarBind {
            name: name.clone(),
            value: SnmpValue::EndOfMibView,
        })
}

/// 起動から `elapsed_secs` 秒後の値（OID の順）。
fn objects(profile: MibProfile, elapsed_secs: u64) -> Vec<VarBind> {
    // 20 秒ごとに 0〜3 を繰り返す
    let step = u32::try_from((elapsed_secs / STEP_SECS) % 4).unwrap_or(0);
    let in_view = 9 + step;
    let good = in_view - 3;
    let enterprise = match profile {
        MibProfile::MeinbergOs | MibProfile::MeinbergLtng | MibProfile::MeinbergLt => {
            ObjectId::new(&meinberg::ENTERPRISE)
        }
        MibProfile::MicrochipS650 => ObjectId::new(&microchip::ENTERPRISE),
    };
    let mut values = vec![
        text(
            ObjectId::new(&SYS_DESCR),
            &format!(
                "ntpterminal 疑似 SNMP エージェント（{}）",
                profile.mib_name()
            ),
        ),
        VarBind {
            name: ObjectId::new(&SYS_OBJECT_ID),
            value: SnmpValue::ObjectId(enterprise),
        },
        VarBind {
            name: ObjectId::new(&SYS_UP_TIME),
            value: SnmpValue::TimeTicks(
                u32::try_from(elapsed_secs.saturating_mul(100)).unwrap_or(u32::MAX),
            ),
        },
    ];
    values.extend(match profile {
        MibProfile::MeinbergOs => vec![
            text(os::column(os::COLUMN_SLOT, 1), "CLK1"),
            integer(os::column(os::COLUMN_STATE, 1), 6),
            integer(os::column(os::COLUMN_IS_GNSS, 1), os::YES),
            integer(os::column(os::COLUMN_GOOD_SV, 1), i64::from(good)),
            integer(os::column(os::COLUMN_SV, 1), i64::from(in_view + 20)),
            VarBind {
                // antennaConnected（bit 10）と clockSynchronized（bit 11）
                name: meinberg::oid(&os::MASTER_REF_BITS),
                value: SnmpValue::OctetString(vec![0x00, 0x30]),
            },
            integer(meinberg::oid(&os::SYS_SYNC), 1),
        ],
        MibProfile::MeinbergLtng => vec![
            integer(ltng::column(ltng::COLUMN_TYPE, 1), 30),
            integer(ltng::column(ltng::COLUMN_USAGE, 1), ltng::USAGE_PRIMARY),
            integer(ltng::column(ltng::COLUMN_STATE, 1), 1),
            integer(ltng::column(ltng::COLUMN_SUBSTATE, 1), 1),
            gauge(ltng::column(ltng::COLUMN_STATUS_A, 1), good),
            gauge(ltng::column(ltng::COLUMN_MAX_STATUS_A, 1), in_view),
        ],
        MibProfile::MeinbergLt => vec![
            text(lt::scalar(lt::OBJECT_TYPE), "GPS170"),
            integer(lt::scalar(lt::OBJECT_TYPE_VAL), 0),
            integer(lt::scalar(lt::OBJECT_GPS_STATE_VAL), 1),
            integer(lt::scalar(lt::OBJECT_SATELLITES_GOOD), i64::from(good)),
            integer(
                lt::scalar(lt::OBJECT_SATELLITES_IN_VIEW),
                i64::from(in_view),
            ),
            integer(lt::scalar(lt::OBJECT_GPS_MODE_VAL), 1),
        ],
        MibProfile::MicrochipS650 => vec![
            // 実機と同じく末尾に NUL を付ける
            text(s650::object(&s650::SYSTEM_MODEL), "SyncServer S650\0"),
            text(s650::object(&s650::CLOCK_STATUS), "NormalLock"),
            text(s650::object(&s650::GNSS_RECEIVER_STATUS), "Tracking"),
            text(s650::object(&s650::GNSS_SATLIST), &satlist(in_view - 2)),
        ],
    });
    values.sort_by(|left, right| left.name.cmp(&right.name));
    values
}

/// "N,X<衛星番号>,Y<通し番号>,C<衛星系>,…" を 128 バイトで切ったもの。
fn satlist(count: u32) -> String {
    const SYSTEMS: [&str; 5] = ["GPS", "GLONASS", "Galileo", "BeiDou", "QZSS"];
    let entries = (1..=count)
        .zip(SYSTEMS.iter().cycle())
        .map(|(index, system)| format!("X{},Y{index},C{system}", index * 4 - 1));
    let mut text = iter::once(count.to_string())
        .chain(entries)
        .collect::<Vec<_>>()
        .join(",");
    // ASCII だけなので、どこで切っても文字の途中にならない
    text.truncate(SATLIST_MAX_LEN);
    text
}

const fn integer(name: ObjectId, value: i64) -> VarBind {
    VarBind {
        name,
        value: SnmpValue::Integer(value),
    }
}

const fn gauge(name: ObjectId, value: u32) -> VarBind {
    VarBind {
        name,
        value: SnmpValue::Gauge32(value),
    }
}

fn text(name: ObjectId, value: &str) -> VarBind {
    VarBind {
        name,
        value: SnmpValue::OctetString(value.as_bytes().to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_answers_its_probe_and_reads_back() {
        for profile in MibProfile::DETECTION_ORDER {
            let objects = objects(profile, 0);
            let request = SnmpMessage::get_request(SIM_COMMUNITY, 7, &profile.status_oids());
            let response = respond(&request, &objects).unwrap();
            assert_eq!(response.pdu.kind, PduKind::Response);
            assert_eq!(response.pdu.request_id, 7);
            let status = profile.interpret(&response.pdu.varbinds).unwrap();
            assert!(!status.satellites.is_empty(), "{profile:?}");
            assert!(
                objects
                    .iter()
                    .any(|object| object.name == profile.probe_oid()),
                "{profile:?}"
            );
        }
    }

    #[test]
    fn ignores_other_communities_and_versions() {
        let objects = objects(MibProfile::MeinbergLtng, 0);
        let names = [ObjectId::new(&SYS_DESCR)];
        assert!(respond(&SnmpMessage::get_request(b"private", 1, &names), &objects).is_none());
        let mut v1 = SnmpMessage::get_request(SIM_COMMUNITY, 1, &names);
        v1.version = 0;
        assert!(respond(&v1, &objects).is_none());
    }

    #[test]
    fn get_next_walks_in_oid_order_and_ends() {
        let objects = objects(MibProfile::MicrochipS650, 0);
        let mut name = ObjectId::new(&[1, 3, 6, 1]);
        let mut walked = Vec::new();
        loop {
            let request = SnmpMessage::get_next_request(SIM_COMMUNITY, 1, &[name.clone()]);
            let response = respond(&request, &objects).unwrap();
            let varbind = response.pdu.varbinds[0].clone();
            if varbind.value == SnmpValue::EndOfMibView {
                break;
            }
            assert!(varbind.name > name);
            name = varbind.name;
            walked.push(name.clone());
        }
        assert_eq!(walked.len(), objects.len());
    }

    #[test]
    fn other_pdus_get_gen_err() {
        let objects = objects(MibProfile::MeinbergOs, 0);
        let mut request = SnmpMessage::get_request(SIM_COMMUNITY, 1, &[ObjectId::new(&SYS_DESCR)]);
        request.pdu.kind = PduKind::SetRequest;
        let response = respond(&request, &objects).unwrap();
        assert_eq!(response.pdu.error_status, Pdu::GEN_ERR);
    }

    /// 衛星が多いと、実機と同じく一覧が 128 バイトで切れて内訳が出なくなる。
    #[test]
    fn satlist_is_cut_like_the_real_device() {
        let short = satlist(7);
        assert!(short.len() <= SATLIST_MAX_LEN);
        assert!(short.starts_with("7,X3,Y1,CGPS,X7,Y2,CGLONASS"));
        let long = satlist(10);
        assert_eq!(long.len(), SATLIST_MAX_LEN);
        let status = MibProfile::MicrochipS650
            .interpret(&[text(s650::object(&s650::GNSS_SATLIST), &long)])
            .unwrap();
        assert_eq!(status.satellites.tracking, Some(10));
        assert_eq!(status.constellations, []);
    }
}
