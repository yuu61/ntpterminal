#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SyncState {
    /// まだ一度も有効な応答を受け取っていない。
    Unsynchronized,
    /// 直近に有効な応答を受け取っている。
    Synchronized,
    /// 応答が途絶えている。最後の基準点から時刻を進め続けている（自走）。
    Holdover,
    /// サーバから拒否された（KoD の DENY / RSTR）。問い合わせを止めている。
    Denied,
}
