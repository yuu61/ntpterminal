//! 配色。
//!
//! 16 色の名前付き ANSI カラーだけを使う。RGB は X のない Linux コンソール（キオスク端末）で
//! 正しく出ないことがあるため。

use ratatui::style::Color;

/// 見出しと大きい時計
pub const ACCENT: Color = Color::LightCyan;
/// ラベル・補足
pub const LABEL: Color = Color::Gray;
/// 通常の値
pub const TEXT: Color = Color::White;
/// 良好（同期済み・小さいずれ）
pub const GOOD: Color = Color::LightGreen;
/// 注意（自走中・やや大きいずれ）
pub const WARN: Color = Color::LightYellow;
/// 異常（拒否・大きいずれ）
pub const BAD: Color = Color::LightRed;
/// 最下部の一文
pub const FOOTER: Color = Color::Yellow;
