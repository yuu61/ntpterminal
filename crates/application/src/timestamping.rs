//! タイムスタンプの取得状況（読み取り用）。
//!
//! 送受信（infrastructure）が書き、表示（TUI・stats・once）が読む。スレッドをまたいで共有するので atomic で持つ。
//! 「カーネルの時刻を使う設定にしたのに、実際は取れていない」状態を表示で隠さないためのもの。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// タイムスタンプの取得状況。`Arc` で共有する。
#[derive(Debug, Default)]
pub struct TimestampingStatus {
    kernel_enabled: AtomicBool,
    kernel_samples: AtomicU64,
    user_samples: AtomicU64,
    tx_misses: AtomicU64,
    last_was_kernel: AtomicBool,
}

/// ある瞬間の取得状況。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimestampingCounts {
    /// カーネルのタイムスタンプを有効にしたか
    pub kernel_enabled: bool,
    /// T1 と T4 の両方をカーネルの時刻で取れた受信の数
    pub kernel_samples: u64,
    /// ユーザ空間の時刻で取った受信の数（カーネルの時刻が取れなかったとき）
    pub user_samples: u64,
    /// 送信時刻（T1）をカーネルから取れなかった送信の数
    pub tx_misses: u64,
    /// 直近の受信がカーネルの時刻だったか
    pub last_was_kernel: bool,
}

impl TimestampingStatus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_kernel_enabled(&self, enabled: bool) {
        self.kernel_enabled.store(enabled, Ordering::Relaxed);
    }

    /// 受け入れた受信 1 件の時刻の出どころを記録する。
    pub fn record_receive(&self, kernel: bool) {
        let counter = if kernel {
            &self.kernel_samples
        } else {
            &self.user_samples
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.last_was_kernel.store(kernel, Ordering::Relaxed);
    }

    pub fn record_tx_miss(&self) {
        self.tx_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn counts(&self) -> TimestampingCounts {
        TimestampingCounts {
            kernel_enabled: self.kernel_enabled.load(Ordering::Relaxed),
            kernel_samples: self.kernel_samples.load(Ordering::Relaxed),
            user_samples: self.user_samples.load(Ordering::Relaxed),
            tx_misses: self.tx_misses.load(Ordering::Relaxed),
            last_was_kernel: self.last_was_kernel.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_sources_of_received_samples() {
        let status = TimestampingStatus::new();
        status.set_kernel_enabled(true);
        status.record_receive(true);
        status.record_receive(true);
        status.record_tx_miss();
        status.record_receive(false);
        assert_eq!(
            status.counts(),
            TimestampingCounts {
                kernel_enabled: true,
                kernel_samples: 2,
                user_samples: 1,
                tx_misses: 1,
                last_was_kernel: false,
            }
        );
    }
}
