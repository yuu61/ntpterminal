//! 大きい数字: ビットマップフォントを、半ブロックと背景色で描く。
//!
//! 1 ピクセル = 横 1 セル × 縦 半セル（を倍率ぶん）。端末のセルは縦長（約 1:2）なので、
//! これでほぼ正方形のピクセルになる。
//!
//! 全部点灯しているセルは「背景色付きの空白」で描く。`█` などのブロック文字は
//! East Asian Ambiguous（端末の設定で幅が 1 にも 2 にもなる）なので、なるべく使わない。
//! 半分だけ点灯するセル（倍率が奇数のとき）だけ `▀` / `▄` を使う。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

/// フォントの高さ（ピクセル）。
const GLYPH_HEIGHT: usize = 5;
/// 文字と文字の間隔（ピクセル）。
const GLYPH_GAP: usize = 1;
const MAX_SCALE: u16 = 12;

/// 文字のビットマップ（`#` が点灯）。数字はすべて同じ幅（4 ピクセル）にして、
/// 数字が変わっても時計が左右に揺れないようにする。
fn glyph(c: char) -> Option<[&'static str; GLYPH_HEIGHT]> {
    Some(match c {
        '0' => ["####", "#..#", "#..#", "#..#", "####"],
        '1' => ["..#.", ".##.", "..#.", "..#.", ".###"],
        '2' => ["####", "...#", "####", "#...", "####"],
        '3' => ["####", "...#", ".###", "...#", "####"],
        '4' => ["#..#", "#..#", "####", "...#", "...#"],
        '5' => ["####", "#...", "####", "...#", "####"],
        '6' => ["####", "#...", "####", "#..#", "####"],
        '7' => ["####", "...#", "...#", "...#", "...#"],
        '8' => ["####", "#..#", "####", "#..#", "####"],
        '9' => ["####", "#..#", "####", "...#", "####"],
        '-' => ["....", "....", "####", "....", "...."],
        ':' => [".", "#", ".", "#", "."],
        '.' => [".", ".", ".", ".", "#"],
        _ => return None,
    })
}

/// 文字列のピクセル幅（知らない文字は数えない）。
fn pixel_width(text: &str) -> usize {
    let widths: Vec<usize> = text.chars().filter_map(glyph).map(|g| g[0].len()).collect();
    widths.iter().sum::<usize>() + widths.len().saturating_sub(1) * GLYPH_GAP
}

/// 文字列を 5 行のピクセル列にする。
fn rasterize(text: &str) -> Vec<Vec<bool>> {
    let mut rows = vec![Vec::new(); GLYPH_HEIGHT];
    for (index, pattern) in text.chars().filter_map(glyph).enumerate() {
        for (row, line) in rows.iter_mut().zip(pattern) {
            if index > 0 {
                row.extend(std::iter::repeat_n(false, GLYPH_GAP));
            }
            row.extend(line.bytes().map(|b| b == b'#'));
        }
    }
    rows
}

/// 倍率 `scale` で描いたときの大きさ（幅, 高さ。単位はセル）。
pub fn size(text: &str, scale: u16) -> (u16, u16) {
    let width = u16::try_from(pixel_width(text)).unwrap_or(u16::MAX);
    let height = (GLYPH_HEIGHT as u16).saturating_mul(scale).div_ceil(2);
    (width.saturating_mul(scale), height)
}

/// 幅と高さに収まる最大の倍率。倍率 1 でも収まらなければ `None`。
pub fn fit_scale(text: &str, max_width: u16, max_height: u16) -> Option<u16> {
    (1..=MAX_SCALE).rev().find(|&scale| {
        let (width, height) = size(text, scale);
        width <= max_width && height <= max_height
    })
}

/// 大きい数字のウィジェット。`area` の左上から描く（はみ出す部分は描かない）。
pub struct BigText<'a> {
    text: &'a str,
    scale: u16,
    color: Color,
}

impl<'a> BigText<'a> {
    pub fn new(text: &'a str, scale: u16, color: Color) -> Self {
        Self {
            text,
            scale: scale.max(1),
            color,
        }
    }
}

impl Widget for BigText<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let bitmap = rasterize(self.text);
        let scale = usize::from(self.scale);
        let lit = |x: usize, sub_y: usize| {
            let y = sub_y / scale;
            y < GLYPH_HEIGHT && bitmap[y].get(x / scale).copied().unwrap_or(false)
        };
        let (width, height) = size(self.text, self.scale);
        for cy in 0..height.min(area.height) {
            for cx in 0..width.min(area.width) {
                let x = usize::from(cx);
                let top = lit(x, usize::from(cy) * 2);
                let bottom = lit(x, usize::from(cy) * 2 + 1);
                let position = (area.x.saturating_add(cx), area.y.saturating_add(cy));
                let Some(cell) = buf.cell_mut(position) else {
                    continue;
                };
                match (top, bottom) {
                    (true, true) => {
                        cell.set_symbol(" ").set_bg(self.color);
                    }
                    (true, false) => {
                        cell.set_symbol("▀").set_fg(self.color);
                    }
                    (false, true) => {
                        cell.set_symbol("▄").set_fg(self.color);
                    }
                    (false, false) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 描画結果を文字にする（背景色付きの空白は `█` として表す）。
    fn draw(text: &str, scale: u16) -> Vec<String> {
        let (width, height) = size(text, scale);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        BigText::new(text, scale, Color::Cyan).render(area, &mut buf);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| {
                        let cell = &buf[(x, y)];
                        if cell.symbol() == " " && cell.bg == Color::Cyan {
                            "█".to_string()
                        } else {
                            cell.symbol().to_string()
                        }
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn renders_zero_at_scale_one_with_half_blocks() {
        assert_eq!(draw("0", 1), ["█▀▀█", "█  █", "▀▀▀▀"]);
    }

    #[test]
    fn even_scale_uses_only_full_cells() {
        let lines = draw("10:2.", 2);
        assert!(
            lines
                .iter()
                .all(|line| !line.contains('▀') && !line.contains('▄'))
        );
        assert_eq!(lines.len(), 5);
        // ピクセル 1 個 = 2 セル。"1" の頭、"0" の上辺、"2" の上辺
        assert_eq!(lines[0], "    ██    ████████      ████████    ");
        // "1" のセリフ、"0" の両辺、":" の上の点、"2" の右辺
        assert_eq!(lines[1], "  ████    ██    ██  ██        ██    ");
        // "." は最下段だけ
        assert_eq!(lines[4], "  ██████  ████████      ████████  ██");
    }

    #[test]
    fn every_digit_has_the_same_width() {
        for digit in '0'..='9' {
            assert_eq!(pixel_width(&digit.to_string()), 4, "{digit}");
        }
    }

    #[test]
    fn computes_size_for_clock_text() {
        // 数字 9 個 × 4 + ":" 2 個 + "." 1 個 + 間隔 11 = 50 ピクセル
        assert_eq!(size("12:34:56.789", 1), (50, 3));
        assert_eq!(size("12:34:56.789", 3), (150, 8));
    }

    #[test]
    fn picks_largest_scale_that_fits() {
        let text = "12:34:56.789";
        assert_eq!(fit_scale(text, 80, 10), Some(1));
        assert_eq!(fit_scale(text, 49, 10), None);
        assert_eq!(fit_scale(text, 200, 10), Some(4));
        assert_eq!(fit_scale(text, 200, 9), Some(3));
    }

    #[test]
    fn clips_to_small_area_without_panicking() {
        let area = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);
        BigText::new("12:34:56.789", 4, Color::Cyan).render(area, &mut buf);
    }
}
