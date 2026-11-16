//! 送信に使うインターフェースの情報と、AF_PACKET のアドレス（sockaddr_ll）。Linux のみ。

use std::io;
use std::mem::size_of;
use std::net::Ipv4Addr;

use domain::packet_crafting::MacAddr;
use nix::ifaddrs::getifaddrs;
use nix::libc;
use nix::net::if_::if_nametoindex;
use nix::sys::socket::{LinkAddr, SockaddrLike};

const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkKind {
    Ethernet,
    /// ループバック（lo）。MAC はすべて 0 で、ARP は要らない
    Loopback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Interface {
    pub name: String,
    pub index: u32,
    pub mac: MacAddr,
    pub kind: LinkKind,
}

/// IPv4 アドレス `local_ip` を持つインターフェースを探す。
pub(crate) fn find_interface(local_ip: Ipv4Addr) -> io::Result<Interface> {
    let addresses: Vec<_> = getifaddrs()?.collect();
    let name = addresses
        .iter()
        .find(|entry| {
            entry
                .address
                .as_ref()
                .and_then(|address| address.as_sockaddr_in())
                .is_some_and(|address| address.ip() == local_ip)
        })
        .map(|entry| entry.interface_name.clone())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{local_ip} を持つインターフェースが見つかりません"),
            )
        })?;
    let link = addresses
        .iter()
        .filter(|entry| entry.interface_name == name)
        .find_map(|entry| entry.address.as_ref()?.as_link_addr().copied())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{name} のリンク層の情報が取れません"),
            )
        })?;
    let kind = match link.hatype() {
        ARPHRD_ETHER => LinkKind::Ethernet,
        ARPHRD_LOOPBACK => LinkKind::Loopback,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{name} は Ethernet ではありません（種別 {other}）。link モードは Ethernet とループバックだけに対応しています"
                ),
            ));
        }
    };
    Ok(Interface {
        index: if_nametoindex(name.as_str())?,
        mac: MacAddr(link.addr().unwrap_or_default()),
        kind,
        name,
    })
}

/// AF_PACKET の bind 先・送信先のアドレス（sockaddr_ll）を作る。
///
/// `protocol` は EtherType（ホストのバイト順で渡す。中でネットワークバイト順にする）。
pub(crate) fn link_addr(index: u32, protocol: u16, destination: MacAddr) -> io::Result<LinkAddr> {
    let mut address = [0u8; 8];
    address[..6].copy_from_slice(&destination.0);
    let raw = libc::sockaddr_ll {
        sll_family: libc::AF_PACKET as u16,
        sll_protocol: protocol.to_be(),
        sll_ifindex: i32::try_from(index)
            .map_err(|_| io::Error::other("インターフェース番号が大きすぎます"))?,
        sll_hatype: 0,
        sll_pkttype: 0,
        sll_halen: 6,
        sll_addr: address,
    };
    let len = libc::socklen_t::try_from(size_of::<libc::sockaddr_ll>())
        .map_err(|_| io::Error::other("sockaddr_ll の大きさが不正です"))?;
    // SAFETY: `raw` はすべての項目を初期化した sockaddr_ll で、この関数が終わるまで有効。
    // 渡す長さはちょうど sockaddr_ll の大きさなので、from_raw は範囲内だけを読む。
    unsafe { LinkAddr::from_raw((&raw as *const libc::sockaddr_ll).cast(), Some(len)) }
        .ok_or_else(|| io::Error::other("AF_PACKET のアドレスを作れません"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_addr_round_trips_through_safe_accessors() {
        let mac = MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        let address = link_addr(7, 0x0800, mac).unwrap();
        assert_eq!(address.ifindex(), 7);
        // protocol() はネットワークバイト順のまま返す
        assert_eq!(u16::from_be(address.protocol()), 0x0800);
        assert_eq!(address.halen(), 6);
        assert_eq!(address.addr(), Some(mac.0));
    }

    #[test]
    fn loopback_is_found_with_zero_mac() {
        let lo = find_interface(Ipv4Addr::LOCALHOST).unwrap();
        assert_eq!(lo.kind, LinkKind::Loopback);
        assert_eq!(lo.mac, MacAddr::ZERO);
        assert!(lo.index > 0);
    }
}
