//! プレゼンテーション層: 読み取り用モデルとイベントを、人が読める形にする。
//!
//! M1 は CLI の出力のみ。TUI（ratatui）は M2 でここに追加する。
#![forbid(unsafe_code)]

pub mod cli;
pub mod format;
