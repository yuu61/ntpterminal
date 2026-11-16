use core::net::{IpAddr, SocketAddr};
use std::io;
use std::net::ToSocketAddrs as _;

pub const NTP_PORT: u16 = 123;

/// `192.168.1.123` / `ntp.example.com` / `host:123` / `[::1]:123` などを解決する。
/// ポートを省略したら 123。複数のアドレスがあれば IPv4 を優先する。
///
/// # Errors
///
/// 名前を解決できない、またはアドレスが 1 つもないとき。
pub fn resolve_server(input: &str) -> io::Result<SocketAddr> {
    let trimmed = input.trim();
    if let Ok(addr) = trimmed.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, NTP_PORT));
    }
    let candidates: Vec<SocketAddr> = if has_port(trimmed) {
        trimmed.to_socket_addrs()?.collect()
    } else {
        (trimmed, NTP_PORT).to_socket_addrs()?.collect()
    };
    candidates
        .iter()
        .find(|addr| addr.is_ipv4())
        .or_else(|| candidates.first())
        .copied()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{trimmed} のアドレスが見つかりません"),
            )
        })
}

/// "host:123" のようにポートが付いているか。
fn has_port(input: &str) -> bool {
    input
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_ntp_port() {
        assert_eq!(
            resolve_server("192.168.1.123").unwrap(),
            "192.168.1.123:123".parse().unwrap()
        );
        assert_eq!(resolve_server("::1").unwrap(), "[::1]:123".parse().unwrap());
    }

    #[test]
    fn keeps_explicit_port() {
        assert_eq!(
            resolve_server("127.0.0.1:12300").unwrap(),
            "127.0.0.1:12300".parse().unwrap()
        );
        assert_eq!(resolve_server("localhost:12300").unwrap().port(), 12300);
    }
}
