//! このリポジトリの URL の QR コード（`--qr`）。
//!
//! - 中身はリポジトリの URL に固定する（[`REPOSITORY_URL`]）。`https://` を付けて、読み取ったアプリにリンクとして
//!   扱わせる（付けないと文字として扱うアプリがある）。36 バイトなので、誤り訂正 M のバージョン 3（29×29 モジュール）になる
//! - 1 モジュール = 横 1 セル × 縦 半セル（`big_digits` と同じく、縦長のセルでほぼ正方形になる）。上下の 2 モジュールが
//!   同じ色のセルは背景色付きの空白、違うセルは `▀`（前景が上、背景が下）で描く（`▄` は使わない）。`▀` は
//!   East Asian Ambiguous で、QR ではほとんどのセルに使うので、曖昧幅が全角の端末では崩れて読めない
//!   （状態の印 `●` と同じく、半角を前提にする）
//! - 明るい地に暗いモジュールを描き、周りの余白（quiet zone）も明るく塗る。端末の暗い背景は余白にならず、白黒が
//!   反転した QR を読めないアプリもあるため。どのセルも前景と背景の両方を決める（端末の既定の色に任せない）
//! - 余白は [`QUIET_ZONE`] モジュール。規格の 4 より狭くして、画面を取りすぎないようにする。縦のモジュールの数が
//!   奇数なので、下の余白は 1 モジュール多い。余白を含めて 33 桁 × 17 行
//! - 画面の隅にぴったり置き、そこがほかの表示と重ならないときだけ出す（推移と同じく、時計を縮めてまで出さない）。
//!   重なるかは、ほかのものを描いた後のバッファで確かめる。全角文字の右半分のセルは空に見えるので、内側にも
//!   1 列・1 行の空きを求める（全角文字の左半分だけが残らないように。表示との間も空く）
//! - 毎フレーム作らず、最初に使うときに 1 回だけ作る（[`repository_qr`]）

use std::sync::LazyLock;

use qrcodegen::{QrCode, QrCodeEcc};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::Color;

use super::theme;

/// QR コードの中身。
pub(super) const REPOSITORY_URL: &str = "https://github.com/yuu61/ntpterminal";
/// 周りの余白（モジュール）。
const QUIET_ZONE: i32 = 2;

/// 作った QR コード。固定の URL が入りきらないことはない（テストで確かめる）ので、作れなければ出さないだけにする。
static REPOSITORY_QR: LazyLock<Option<QrCode>> =
    LazyLock::new(|| QrCode::encode_text(REPOSITORY_URL, QrCodeEcc::Medium).ok());

/// QR コードを置く画面の隅。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QrCorner {
    /// 左上
    TopLeft,
    /// 右上
    TopRight,
    /// 左下
    BottomLeft,
    /// 右下
    BottomRight,
}

impl QrCorner {
    const fn is_left(self) -> bool {
        matches!(self, Self::TopLeft | Self::BottomLeft)
    }

    const fn is_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::TopRight)
    }
}

/// リポジトリの URL の QR コード（最初に呼んだときに作る）。
pub(super) fn repository_qr() -> Option<&'static QrCode> {
    REPOSITORY_QR.as_ref()
}

/// 余白を含めた大きさ（幅, 高さ。単位はセル）。
pub(super) fn size(code: &QrCode) -> (u16, u16) {
    let modules = u16::try_from(code.size() + 2i32 * QUIET_ZONE).unwrap_or(u16::MAX);
    (modules, modules.div_ceil(2))
}

/// `area` の `corner` の隅に描く。入らないとき・ほかの表示と重なるとき（内側の 1 列・1 行の空きを含む）は描かない。
pub(super) fn render(code: &QrCode, corner: QrCorner, area: Rect, buf: &mut Buffer) {
    let (width, height) = size(code);
    if width > area.width || height > area.height {
        return;
    }
    let x = if corner.is_left() {
        area.x
    } else {
        area.right() - width
    };
    let y = if corner.is_top() {
        area.y
    } else {
        area.bottom() - height
    };
    // 内側（隅の反対側）へ 1 列・1 行広げた範囲。画面の外は除く
    let clearance = Rect::new(
        if corner.is_left() {
            x
        } else {
            x.saturating_sub(1)
        },
        if corner.is_top() {
            y
        } else {
            y.saturating_sub(1)
        },
        width.saturating_add(1),
        height.saturating_add(1),
    )
    .intersection(area);
    let is_blank = |position| buf.cell(position).is_some_and(|cell| *cell == Cell::EMPTY);
    if !clearance.positions().all(is_blank) {
        return;
    }

    let color = |dark: bool| -> Color {
        if dark {
            theme::QR_DARK
        } else {
            theme::QR_LIGHT
        }
    };
    for (row, cell_y) in (0..height).zip(y..) {
        let upper_y = i32::from(row) * 2i32 - QUIET_ZONE;
        for (column, cell_x) in (0..width).zip(x..) {
            let module_x = i32::from(column) - QUIET_ZONE;
            // 範囲の外（余白）は明るいモジュールになる
            let upper = code.get_module(module_x, upper_y);
            let lower = code.get_module(module_x, upper_y + 1);
            if let Some(cell) = buf.cell_mut((cell_x, cell_y)) {
                let mut drawn = Cell::new(if upper == lower { " " } else { "▀" });
                drawn.fg = color(upper);
                drawn.bg = color(lower);
                *cell = drawn;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    const ALL_CORNERS: [QrCorner; 4] = [
        QrCorner::TopLeft,
        QrCorner::TopRight,
        QrCorner::BottomLeft,
        QrCorner::BottomRight,
    ];

    fn code() -> &'static QrCode {
        repository_qr().unwrap()
    }

    /// 隅にぴったり置いたときの場所。
    fn placed(corner: QrCorner, area: Rect) -> Rect {
        let (width, height) = size(code());
        let x = if corner.is_left() {
            area.x
        } else {
            area.right() - width
        };
        let y = if corner.is_top() {
            area.y
        } else {
            area.bottom() - height
        };
        Rect::new(x, y, width, height)
    }

    fn is_dark(color: Color) -> bool {
        if color == theme::QR_DARK {
            true
        } else {
            assert_eq!(color, theme::QR_LIGHT);
            false
        }
    }

    /// 描いた結果からモジュールを読み戻す（余白を含む。`▀` は前景が上・背景が下、空白は上下とも背景）。
    /// QR の 2 色でない色・ほかの文字があればテストを落とす。
    fn read_back(buf: &Buffer, rect: Rect) -> Vec<Vec<bool>> {
        let mut modules = Vec::new();
        for y in rect.top()..rect.bottom() {
            let mut upper = Vec::new();
            let mut lower = Vec::new();
            for x in rect.left()..rect.right() {
                let cell = &buf[(x, y)];
                match cell.symbol() {
                    "▀" => {
                        upper.push(is_dark(cell.fg));
                        lower.push(is_dark(cell.bg));
                    }
                    " " => {
                        // 前景も決めてある（端末の既定の色に任せない）
                        assert_eq!(cell.fg, cell.bg, "({x}, {y})");
                        upper.push(is_dark(cell.bg));
                        lower.push(is_dark(cell.bg));
                    }
                    other => panic!("({x}, {y}) に {other:?}"),
                }
            }
            modules.push(upper);
            modules.push(lower);
        }
        modules
    }

    /// 余白を含めたモジュール（縦はセルの行の 2 倍。下の余白が 1 モジュール多い）。
    fn expected_modules() -> Vec<Vec<bool>> {
        let (width, height) = size(code());
        (0i32..i32::from(height) * 2i32)
            .map(|y| {
                (0i32..i32::from(width))
                    .map(|x| code().get_module(x - QUIET_ZONE, y - QUIET_ZONE))
                    .collect()
            })
            .collect()
    }

    /// `rect` の外のセルがすべて空か。
    fn blank_outside(buf: &Buffer, rect: Rect) -> bool {
        buf.area
            .positions()
            .filter(|&position| !rect.contains(position))
            .all(|position| buf[position] == Cell::EMPTY)
    }

    #[test]
    fn encodes_the_repository_url_as_version_3() {
        assert!(REPOSITORY_URL.starts_with("https://"));
        assert_eq!(REPOSITORY_URL.len(), 36);
        let code = code();
        assert_eq!(code.version().value(), 3);
        assert_eq!(code.error_correction_level(), QrCodeEcc::Medium);
        assert_eq!(code.size(), 29i32);
        // 余白 2 モジュールを含めて 33 × 33 モジュール = 33 桁 × 17 行
        assert_eq!(size(code), (33, 17));
    }

    /// どの隅でも、隅にぴったり置き、余白を含めて全モジュールを正しい色で描く。ほかのセルには触らない。
    #[test]
    fn draws_every_module_flush_in_each_corner() {
        let area = Rect::new(0, 0, 50, 30);
        for corner in ALL_CORNERS {
            let mut buf = Buffer::empty(area);
            render(code(), corner, area, &mut buf);
            let rect = placed(corner, area);
            assert_eq!(read_back(&buf, rect), expected_modules(), "{corner:?}");
            assert!(blank_outside(&buf, rect), "{corner:?}");
        }
    }

    /// 余白は明るく塗る（左上の隅のセルと、下の余白の 1 行）。
    #[test]
    fn paints_the_quiet_zone_light() {
        let area = Rect::new(0, 0, 33, 17);
        let mut buf = Buffer::empty(area);
        render(code(), QrCorner::TopLeft, area, &mut buf);
        for x in 0..33 {
            for y in [0, 16] {
                let cell = &buf[(x, y)];
                assert_eq!(
                    (cell.symbol(), cell.fg, cell.bg),
                    (" ", theme::QR_LIGHT, theme::QR_LIGHT)
                );
            }
        }
    }

    /// 画面がちょうど QR の大きさなら描く（内側の空きは画面の外なので求めない）。1 桁でも足りなければ描かない。
    #[test]
    fn needs_the_whole_code_to_fit() {
        for (width, height, drawn) in [(33, 17, true), (32, 17, false), (33, 16, false)] {
            let area = Rect::new(0, 0, width, height);
            let mut buf = Buffer::empty(area);
            render(code(), QrCorner::BottomRight, area, &mut buf);
            assert_eq!(buf[(0, 0)] != Cell::EMPTY, drawn, "{width}x{height}");
        }
    }

    /// 置く場所か内側の 1 列・1 行に何かあれば描かない。その外側なら描く。
    #[test]
    fn skips_when_the_corner_or_its_inner_gap_is_taken() {
        let area = Rect::new(0, 0, 50, 30);
        // 右下の QR は x 17..50・y 13..30。内側の空きは x = 16 と y = 12
        for (x, y, drawn) in [
            (49, 29, false),
            (17, 13, false),
            (16, 20, false),
            (30, 12, false),
            (16, 12, false),
            (15, 20, true),
            (30, 11, true),
            (49, 11, true),
        ] {
            let mut buf = Buffer::empty(area);
            buf.set_string(x, y, "x", Style::new());
            render(code(), QrCorner::BottomRight, area, &mut buf);
            // (20, 14) は QR の中で、どの "x" とも重ならない
            assert_eq!(buf[(20, 14)] != Cell::EMPTY, drawn, "({x}, {y})");
        }
    }

    /// 全角文字の右半分のセルは空に見えるが、左半分が内側の空きに入るので、全角文字を半分だけ消さない。
    #[test]
    fn does_not_cut_a_wide_character() {
        let area = Rect::new(0, 0, 50, 30);
        // 右下の QR の左端は x = 17。x = 16 の全角文字は x = 17 まで使う
        let mut in_gap = Buffer::empty(area);
        in_gap.set_string(16, 20, "あ", Style::new());
        assert_eq!(in_gap[(17, 20)], Cell::EMPTY);
        render(code(), QrCorner::BottomRight, area, &mut in_gap);
        assert_eq!(in_gap[(49, 29)], Cell::EMPTY);
        assert_eq!(in_gap[(16, 20)].symbol(), "あ");

        // x = 15 の全角文字（x = 16 まで）なら重ならないので描く
        let mut outside = Buffer::empty(area);
        outside.set_string(15, 20, "あ", Style::new());
        render(code(), QrCorner::BottomRight, area, &mut outside);
        assert_eq!(
            read_back(&outside, placed(QrCorner::BottomRight, area)),
            expected_modules()
        );
        assert_eq!(outside[(15, 20)].symbol(), "あ");
    }
}
