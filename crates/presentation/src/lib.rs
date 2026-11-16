//! プレゼンテーション層: 読み取り用モデルとイベントを、人が読める形（CLI・TUI）にする。
#![forbid(unsafe_code)]

extern crate alloc;

pub mod cli;
pub mod format;
pub mod tui;
