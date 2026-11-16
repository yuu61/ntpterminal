//! 表示する PHC を決める: ptp4l のポートの一覧から、ptp4l が実際に合わせている PHC を選ぶ。
//!
//! - PHC の番号は ptp4l が知らせる値（`PORT_HWCLOCK_NP`）を使う。仮想 PHC（vclock）を使う設定では、
//!   インターフェースの PHC（`ethtool -T` の値）と ptp4l が合わせている PHC が違うため
//! - `PORT_HWCLOCK_NP` に答えない古い linuxptp（4.0 より前。vclock もない）でだけ、インターフェースの PHC を使う
//! - ソフトウェアのタイムスタンプのポート（ptp4l の `-S`）は PHC を使わない（ptp4l はシステムの時計を合わせる）
//! - 使える PHC が 1 つもない・2 つ以上あって指定がない・指定した PHC を ptp4l が使っていないときはエラーにする
//!   （黙って別の時計を表示しない）

use alloc::collections::BTreeMap;

use super::Ptp4lPort;

/// 表示する PHC の指定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhcRequest {
    /// /dev/ptpN の N
    Index(u32),
    /// ptp4l のポートのインターフェース名（そのポートの PHC）
    Interface(String),
}

/// 選んだ PHC。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedPhc {
    /// /dev/ptpN の N
    pub index: u32,
    /// この PHC を使うポートの番号（番号の順）
    pub ports: Vec<u16>,
    /// この PHC を使うポートのインターフェース名（ポート番号の順）
    pub interfaces: Vec<String>,
    /// 仮想 PHC（vclock）か
    pub vclock: bool,
    /// ptp4l が PHC の番号を知らせず、インターフェースの PHC を使ったか（古い linuxptp）
    pub from_interface: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PhcSelectionError {
    #[error("ptp4l にポートがありません")]
    NoPorts,
    #[error(
        "ptp4l のポートはどれも PHC を使っていません（ソフトウェアのタイムスタンプで動いています: {interfaces}）"
    )]
    NoHardwareClock { interfaces: String },
    #[error("ptp4l は複数の PHC を使っています（{used}）。表示する PHC を指定してください")]
    Ambiguous { used: String },
    #[error("ptp4l は {requested} を使っていません（使っているもの: {used}）")]
    NotUsed { requested: String, used: String },
    #[error(
        "{interface} の PHC が分かりません（ptp4l が PHC の番号を知らせず、インターフェースからも分かりません）"
    )]
    Unknown { interface: String },
}

/// `ports` から、表示する PHC を選ぶ。`interface_phc` はインターフェースの PHC の番号
/// （`PORT_HWCLOCK_NP` に答えない古い linuxptp のときだけ使う）。
///
/// # Errors
///
/// 使える PHC が 1 つもない・2 つ以上あって `requested` がない・`requested` を ptp4l が使っていないとき。
pub fn select_phc<F>(
    ports: &[Ptp4lPort],
    requested: Option<&PhcRequest>,
    interface_phc: F,
) -> Result<SelectedPhc, PhcSelectionError>
where
    F: Fn(&str) -> Option<u32>,
{
    if ports.is_empty() {
        return Err(PhcSelectionError::NoPorts);
    }
    // PHC ごとに、使うポートをまとめる（番号の順）
    let mut by_index: BTreeMap<u32, Vec<(&Ptp4lPort, bool)>> = BTreeMap::new();
    for port in ports.iter().filter(|port| port.timestamping.uses_phc()) {
        let (index, from_interface) = match port.hwclock {
            Some(hwclock) => match u32::try_from(hwclock.index) {
                Ok(index) => (index, false),
                // PHC を使わないポート
                Err(_) => continue,
            },
            None => (
                interface_phc(&port.interface).ok_or_else(|| PhcSelectionError::Unknown {
                    interface: port.interface.clone(),
                })?,
                true,
            ),
        };
        by_index
            .entry(index)
            .or_default()
            .push((port, from_interface));
    }
    if by_index.is_empty() {
        return Err(PhcSelectionError::NoHardwareClock {
            interfaces: join(ports.iter().map(|port| port.interface.clone())),
        });
    }
    let used = || {
        join(by_index.iter().map(|(index, users)| {
            let names = join(users.iter().map(|&(port, _)| port.interface.clone()));
            format!("/dev/ptp{index}: {names}")
        }))
    };
    let chosen = match requested.cloned() {
        Some(PhcRequest::Index(index)) => {
            if !by_index.contains_key(&index) {
                return Err(PhcSelectionError::NotUsed {
                    requested: format!("/dev/ptp{index}"),
                    used: used(),
                });
            }
            index
        }
        Some(PhcRequest::Interface(name)) => {
            let found = by_index.iter().find_map(|(&index, users)| {
                users
                    .iter()
                    .any(|&(port, _)| port.interface == name)
                    .then_some(index)
            });
            found.ok_or_else(|| PhcSelectionError::NotUsed {
                requested: name,
                used: used(),
            })?
        }
        None if by_index.len() == 1 => *by_index.keys().next().unwrap_or(&0),
        None => return Err(PhcSelectionError::Ambiguous { used: used() }),
    };
    let users = by_index.remove(&chosen).unwrap_or_default();
    Ok(SelectedPhc {
        index: chosen,
        ports: users.iter().map(|&(port, _)| port.number).collect(),
        interfaces: users
            .iter()
            .map(|&(port, _)| port.interface.clone())
            .collect(),
        vclock: users
            .iter()
            .any(|&(port, _)| port.hwclock.is_some_and(|hwclock| hwclock.vclock)),
        from_interface: users.iter().any(|&(_, from_interface)| from_interface),
    })
}

fn join<I>(names: I) -> String
where
    I: Iterator<Item = String>,
{
    names.collect::<Vec<_>>().join("、")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecase::{HwClock, PortTimestamping};
    use domain::ptp_clock::PortState;

    fn port(number: u16, interface: &str, hwclock: Option<i32>) -> Ptp4lPort {
        Ptp4lPort {
            number,
            interface: interface.to_owned(),
            timestamping: PortTimestamping::Hardware,
            hwclock: hwclock.map(|index| HwClock {
                index,
                vclock: false,
            }),
            state: PortState::Slave,
        }
    }

    fn no_lookup(_: &str) -> Option<u32> {
        None
    }

    #[test]
    fn uses_the_phc_that_ptp4l_reports() {
        let ports = [port(1, "eno1", Some(0i32))];
        let selected = select_phc(&ports, None, no_lookup).unwrap();
        assert_eq!(selected.index, 0);
        assert_eq!(selected.ports, [1]);
        assert_eq!(selected.interfaces, ["eno1"]);
        assert!(!selected.from_interface);
    }

    /// vclock: インターフェースの PHC（0）ではなく、ptp4l が知らせる仮想 PHC（2）を使う。
    #[test]
    fn prefers_the_virtual_clock_over_the_interface() {
        let mut virtual_port = port(1, "eno1", Some(2i32));
        virtual_port.hwclock = Some(HwClock {
            index: 2i32,
            vclock: true,
        });
        let selected = select_phc(&[virtual_port], None, |_| Some(0)).unwrap();
        assert_eq!(selected.index, 2);
        assert!(selected.vclock);
    }

    #[test]
    fn falls_back_to_the_interface_on_old_linuxptp() {
        let ports = [port(1, "eno1", None)];
        let selected = select_phc(&ports, None, |name| (name == "eno1").then_some(3)).unwrap();
        assert_eq!(selected.index, 3);
        assert!(selected.from_interface);
        assert_eq!(
            select_phc(&ports, None, no_lookup),
            Err(PhcSelectionError::Unknown {
                interface: "eno1".to_owned(),
            })
        );
    }

    #[test]
    fn a_boundary_clock_on_one_nic_is_one_phc() {
        let ports = [port(1, "eth0", Some(0i32)), port(2, "eth1", Some(0i32))];
        let selected = select_phc(&ports, None, no_lookup).unwrap();
        assert_eq!(selected.ports, [1, 2]);
        assert_eq!(selected.interfaces, ["eth0", "eth1"]);
    }

    #[test]
    fn several_phcs_need_a_choice() {
        let ports = [port(1, "eth0", Some(0i32)), port(2, "eth1", Some(1i32))];
        let Err(PhcSelectionError::Ambiguous { used }) = select_phc(&ports, None, no_lookup) else {
            panic!("選べないはず");
        };
        assert_eq!(used, "/dev/ptp0: eth0、/dev/ptp1: eth1");
        let by_index = select_phc(&ports, Some(&PhcRequest::Index(1)), no_lookup).unwrap();
        assert_eq!(by_index.ports, [2]);
        let by_name = select_phc(
            &ports,
            Some(&PhcRequest::Interface("eth0".to_owned())),
            no_lookup,
        )
        .unwrap();
        assert_eq!(by_name.index, 0);
        assert!(matches!(
            select_phc(&ports, Some(&PhcRequest::Index(5)), no_lookup),
            Err(PhcSelectionError::NotUsed { .. })
        ));
        assert!(matches!(
            select_phc(
                &ports,
                Some(&PhcRequest::Interface("eth9".to_owned())),
                no_lookup
            ),
            Err(PhcSelectionError::NotUsed { .. })
        ));
    }

    #[test]
    fn software_timestamping_has_no_phc() {
        let mut software = port(1, "veth0", Some(-1i32));
        software.timestamping = PortTimestamping::Software;
        assert_eq!(
            select_phc(&[software], None, |_| Some(0)),
            Err(PhcSelectionError::NoHardwareClock {
                interfaces: "veth0".to_owned(),
            })
        );
        assert_eq!(
            select_phc(&[], None, no_lookup),
            Err(PhcSelectionError::NoPorts)
        );
    }
}
