pub mod once;
pub mod stats;

use crate::format::{display_width, pad_to_width};

/// 「ラベル  値」の行を、ラベルの表示幅をそろえて並べる。
fn render_rows(rows: &[(&str, String)]) -> String {
    let width = rows
        .iter()
        .map(|&(label, _)| display_width(label))
        .max()
        .unwrap_or(0)
        + 2;
    let mut out = String::new();
    for row in rows {
        out.push_str(&pad_to_width(row.0, width));
        out.push_str(&row.1);
        out.push('\n');
    }
    out
}
