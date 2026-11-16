//! 経路表（/proc/net/route）と近隣表（/proc/net/arp）を読む。Linux のみ。
//!
//! 文字列を受け取る純粋な関数にして、実機なしでテストできるようにしている。
//! ポリシールーティング（ip rule）には対応しない。合わないときは `--dst-mac` で宛先 MAC を直接指定する。

use std::net::Ipv4Addr;

use domain::packet_crafting::MacAddr;

/// 経路が有効
const RTF_UP: u16 = 0x0001;
/// ゲートウェイ経由
const RTF_GATEWAY: u16 = 0x0002;
/// 近隣表のエントリが完成している（MAC が分かっている）
const ATF_COM: u32 = 0x02;

/// 経路表の 1 行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Route {
    pub interface: String,
    pub destination: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub flags: u16,
    pub metric: u32,
    pub mask: Ipv4Addr,
}

/// /proc/net/route を読む。読めない行は飛ばす。
///
/// アドレスはネットワークバイト順の値を「その CPU の整数として」16 進で出力したもの。
/// 例: リトルエンディアンでは 192.168.2.1 が `0102A8C0` になる。
pub(crate) fn parse_routes(text: &str) -> Vec<Route> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            Some(Route {
                interface: (*fields.first()?).to_string(),
                destination: hex_address(fields.get(1)?)?,
                gateway: hex_address(fields.get(2)?)?,
                flags: u16::from_str_radix(fields.get(3)?, 16).ok()?,
                metric: fields.get(6)?.parse().ok()?,
                mask: hex_address(fields.get(7)?)?,
            })
        })
        .collect()
}

fn hex_address(text: &str) -> Option<Ipv4Addr> {
    let value = u32::from_str_radix(text, 16).ok()?;
    Some(Ipv4Addr::from(value.to_ne_bytes()))
}

/// `interface` から `target` へ送るときの次の転送先（同じネットワークなら `target` 自身、
/// それ以外はゲートウェイ）。カーネルと同じく、最長一致・次にメトリックが小さい経路を選ぶ。
pub(crate) fn next_hop(routes: &[Route], interface: &str, target: Ipv4Addr) -> Option<Ipv4Addr> {
    let target_bits = u32::from(target);
    routes
        .iter()
        .filter(|route| route.interface == interface && route.flags & RTF_UP != 0)
        .filter(|route| target_bits & u32::from(route.mask) == u32::from(route.destination))
        .max_by(|a, b| {
            let prefix = |route: &Route| u32::from(route.mask).count_ones();
            prefix(a)
                .cmp(&prefix(b))
                .then_with(|| b.metric.cmp(&a.metric))
        })
        .map(|route| {
            if route.flags & RTF_GATEWAY != 0 {
                route.gateway
            } else {
                target
            }
        })
}

/// /proc/net/arp から、`interface` 上の `ip` の MAC アドレスを探す。完成していないエントリは使わない。
pub(crate) fn find_neighbor(text: &str, interface: &str, ip: Ipv4Addr) -> Option<MacAddr> {
    text.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let entry_ip: Ipv4Addr = fields.first()?.parse().ok()?;
        let flags = u32::from_str_radix(fields.get(2)?.trim_start_matches("0x"), 16).ok()?;
        let mac: MacAddr = fields.get(3)?.parse().ok()?;
        let device = fields.get(5)?;
        (entry_ip == ip && *device == interface && flags & ATF_COM != 0 && mac != MacAddr::ZERO)
            .then_some(mac)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTES: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eth0\t00000000\t0102A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
eth0\t0000A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
eth0\t005CA8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
eth0\t0000000A\tFE02A8C0\t0003\t0\t0\t50\t000000FF\t0\t0\t0
eth0\t0000000A\t0102A8C0\t0003\t0\t0\t10\t000000FF\t0\t0\t0
wlan0\t00000000\t01BEA8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
";

    #[test]
    fn decodes_addresses_in_host_byte_order() {
        let routes = parse_routes(ROUTES);
        assert_eq!(routes.len(), 6);
        assert_eq!(routes[0].gateway, Ipv4Addr::new(192, 168, 2, 1));
        assert_eq!(routes[1].destination, Ipv4Addr::new(192, 168, 0, 0));
        assert_eq!(routes[1].mask, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(routes[0].metric, 100);
    }

    #[test]
    fn same_subnet_goes_direct_and_others_via_gateway() {
        let routes = parse_routes(ROUTES);
        // 192.168.92.0/24 は直結
        assert_eq!(
            next_hop(&routes, "eth0", Ipv4Addr::new(192, 168, 92, 234)),
            Some(Ipv4Addr::new(192, 168, 92, 234))
        );
        // それ以外はデフォルトゲートウェイ
        assert_eq!(
            next_hop(&routes, "eth0", Ipv4Addr::new(133, 243, 238, 164)),
            Some(Ipv4Addr::new(192, 168, 2, 1))
        );
        // 10.0.0.0/8 は 2 本あり、メトリックが小さいほう
        assert_eq!(
            next_hop(&routes, "eth0", Ipv4Addr::new(10, 1, 2, 3)),
            Some(Ipv4Addr::new(192, 168, 2, 1))
        );
        // ほかのインターフェースの経路は使わない
        assert_eq!(
            next_hop(&routes, "wlan0", Ipv4Addr::new(192, 168, 92, 234)),
            Some(Ipv4Addr::new(192, 168, 190, 1))
        );
        assert_eq!(next_hop(&routes, "eth1", Ipv4Addr::new(10, 1, 2, 3)), None);
    }

    const ARP: &str = "\
IP address       HW type     Flags       HW address            Mask     Device
192.168.2.1      0x1         0x2         00:11:22:33:44:55     *        eth0
192.168.2.9      0x1         0x0         00:00:00:00:00:00     *        eth0
192.168.2.1      0x1         0x2         66:77:88:99:aa:bb     *        wlan0
";

    #[test]
    fn finds_only_complete_neighbor_entries() {
        assert_eq!(
            find_neighbor(ARP, "eth0", Ipv4Addr::new(192, 168, 2, 1)),
            Some(MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]))
        );
        assert_eq!(
            find_neighbor(ARP, "wlan0", Ipv4Addr::new(192, 168, 2, 1)),
            Some(MacAddr([0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB]))
        );
        // 未完成（Flags 0x0）は使わない
        assert_eq!(
            find_neighbor(ARP, "eth0", Ipv4Addr::new(192, 168, 2, 9)),
            None
        );
        assert_eq!(
            find_neighbor(ARP, "eth0", Ipv4Addr::new(192, 168, 2, 77)),
            None
        );
    }
}
