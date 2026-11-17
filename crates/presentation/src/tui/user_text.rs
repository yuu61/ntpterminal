//! 利用者が書く文（見出し `--title`・最下部の一文 `--footer`）の書き方と、画面での並べ方。
//!
//! - `\n` で改行し、`\t` でタブを入れる。`\` そのものは `\\` と書く。`\` の後にほかの文字が続く値・`\` で終わる
//!   値はエラーにする（`C:\Users` の `\U` のような書き間違いで、文字を黙って落とさない）
//! - コマンドラインと設定ファイルで同じ書き方にする（[`parse_user_text`] を両方で使う）。設定ファイルの `"..."` は
//!   TOML のエスケープを解いた後の値を読むので、`"A\nB"` も改行になり、`\` そのものは `"\\\\"` と書く
//!   （`'...'` ならコマンドラインと同じ）
//! - 改行・タブの文字そのもの（`"""` で書いた複数行など）も使える。CR LF は LF にする（CR LF で保存した設定ファイル・
//!   PowerShell の `` `r`n ``）。ほかの制御文字はエラーにする（ratatui は制御文字を描かずに黙って捨てる）
//! - 末尾の改行 1 つは行に数えない（`"""` の閉じを次の行に書くと付く）。空の値は空の 1 行（行の数は変えない）
//! - 行ごとに左右中央に置く。タブは行の先頭から [`TAB_WIDTH`] 桁ごとの位置まで空白で埋める（行ごとに中央へ置くので、
//!   行をまたいで桁はそろわない）。幅は、中央に置く計算と同じ ratatui の幅で数える。見出しは字間を空けてから埋める
//! - 利用者の文なので ASCII に限らない（英語・ASCII だけという決まり（`text` の先頭）は、プログラムが出す文言のもの）

use ratatui::text::Span;

/// タブで埋める桁の単位（端末の既定のタブ幅）。
const TAB_WIDTH: usize = 8;

/// `--title`・`--footer` の値の `\n`・`\t`・`\\` を、改行・タブ・`\` にする。CR LF は LF にする。
/// コマンドラインと設定ファイルの値の両方に使う（モジュールの説明を参照）。
///
/// # Errors
///
/// `\` の後に `n`・`t`・`\` 以外が続くとき、`\` で終わるとき、改行とタブのほかの制御文字があるとき。
pub fn parse_user_text(text: &str) -> Result<String, String> {
    let mut parsed = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some('n') => parsed.push('\n'),
                Some('t') => parsed.push('\t'),
                Some('\\') => parsed.push('\\'),
                Some(other) => {
                    return Err(format!(
                        "\\ の後に書けるのは n（改行）・t（タブ）・\\ だけです（\\ そのものは \\\\ と書く）: \\{}",
                        other.escape_debug()
                    ));
                }
                None => {
                    return Err("\\ で終わっています（\\ そのものは \\\\ と書く）".to_owned());
                }
            },
            // CR LF の CR は捨てる（LF だけが残る）
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\t' => parsed.push(ch),
            _ if ch.is_control() => {
                return Err(format!(
                    "改行とタブのほかの制御文字は使えません（改行は \\n、タブは \\t と書く）: {}",
                    ch.escape_unicode()
                ));
            }
            _ => parsed.push(ch),
        }
    }
    Ok(parsed)
}

/// 行に分ける。末尾の改行 1 つは行に数えず、空の値は空の 1 行にする。
pub(super) fn lines(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() { vec![""] } else { lines }
}

/// タブを、行の先頭から [`TAB_WIDTH`] 桁ごとの位置まで空白で埋める。
pub(super) fn expand_tabs(line: &str) -> String {
    let mut expanded = String::with_capacity(line.len());
    for ch in line.chars() {
        if ch == '\t' {
            let column = Span::raw(expanded.as_str()).width();
            let stop = column.saturating_add(1).next_multiple_of(TAB_WIDTH);
            expanded.push_str(&" ".repeat(stop.saturating_sub(column)));
        } else {
            expanded.push(ch);
        }
    }
    expanded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_become_newlines_tabs_and_backslashes() {
        assert_eq!(
            parse_user_text(r"Sponsored by\nExample\tInc. \\o/"),
            Ok("Sponsored by\nExample\tInc. \\o/".to_owned())
        );
        // 文字そのものも使える。利用者の文は ASCII に限らない
        assert_eq!(
            parse_user_text("協賛\n\tExample"),
            Ok("協賛\n\tExample".to_owned())
        );
        assert_eq!(parse_user_text(""), Ok(String::new()));
    }

    #[test]
    fn crlf_becomes_lf_and_other_controls_are_errors() {
        assert_eq!(parse_user_text("A\r\nB\r\n"), Ok("A\nB\n".to_owned()));
        for (text, expected) in [
            ("A\rB", r"\u{d}"),
            ("A\u{7}", r"\u{7}"),
            ("\u{1b}[31mred", r"\u{1b}"),
        ] {
            let error = parse_user_text(text).unwrap_err();
            assert!(error.contains(expected), "{text:?}: {error}");
        }
    }

    /// 使えないエスケープは文字を落とさずにエラーにする（Windows のパスの書き間違いなど）。
    #[test]
    fn unknown_escapes_and_trailing_backslash_are_errors() {
        for (text, expected) in [
            (r"C:\Users", r": \U"),
            (r"\r\n", r": \r"),
            // \ の後に改行そのもの（エラーでは改行を \n と書いて示す）
            ("A\\\nB", r": \\n"),
            (r"tail\", "\\ で終わって"),
        ] {
            let error = parse_user_text(text).unwrap_err();
            assert!(error.contains(expected), "{text:?}: {error}");
            assert!(error.contains(r"\\ と書く"), "{text:?}: {error}");
        }
    }

    #[test]
    fn last_newline_does_not_add_a_line() {
        assert_eq!(lines("A\nB"), ["A", "B"]);
        assert_eq!(lines("A\nB\n"), ["A", "B"]);
        assert_eq!(lines("A\n\nB\n\n"), ["A", "", "B", ""]);
        assert_eq!(lines("\nA"), ["", "A"]);
        assert_eq!(lines(""), [""]);
        assert_eq!(lines("\n"), [""]);
    }

    #[test]
    fn tabs_fill_up_to_the_next_stop() {
        assert_eq!(expand_tabs("\tA"), "        A");
        assert_eq!(expand_tabs("ABC\tD"), "ABC     D");
        // ちょうどタブ位置にあれば、次のタブ位置まで進める
        assert_eq!(expand_tabs("ABCDEFGH\tI"), "ABCDEFGH        I");
        assert_eq!(expand_tabs("A\t\tB"), "A               B");
        // 全角の文字は 2 桁に数える
        assert_eq!(expand_tabs("協賛\tA"), "協賛    A");
        assert_eq!(expand_tabs("no tab"), "no tab");
    }
}
