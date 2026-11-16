//! ポーリングの作法。
//!
//! 1 秒未満の連続ポーリングは、自分で管理するローカルのサーバ専用。
//! 公開サーバに短い間隔で問い合わせると迷惑になり、KoD（RATE / DENY）を受けることもある。

use std::net::IpAddr;
use std::time::Duration;

/// 公開サーバに対して許す最短のポーリング間隔。
pub const PUBLIC_SERVER_MIN_INTERVAL: Duration = Duration::from_secs(16);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "{ip} はローカルネットワーク外のサーバです。公開サーバには {min_secs} 秒未満の間隔で問い合わせません（自分で管理するサーバなら --allow-fast-poll を指定してください）"
)]
pub struct PollingPolicyError {
    pub ip: IpAddr,
    pub min_secs: u64,
}

/// プライベート・ループバック・リンクローカルのアドレスか。
pub fn is_local_network(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

pub fn check_interval(ip: IpAddr, interval: Duration) -> Result<(), PollingPolicyError> {
    if is_local_network(ip) || interval >= PUBLIC_SERVER_MIN_INTERVAL {
        Ok(())
    } else {
        Err(PollingPolicyError {
            ip,
            min_secs: PUBLIC_SERVER_MIN_INTERVAL.as_secs(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn allows_fast_polling_only_on_local_networks() {
        let fast = Duration::from_millis(500);
        for local in [
            "192.168.1.123",
            "10.0.0.1",
            "172.16.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "::1",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(check_interval(ip(local), fast).is_ok(), "{local}");
        }
        assert!(check_interval(ip("133.243.238.163"), fast).is_err());
        assert!(check_interval(ip("2001:db8::1"), fast).is_err());
        assert!(check_interval(ip("133.243.238.163"), PUBLIC_SERVER_MIN_INTERVAL).is_ok());
    }
}
