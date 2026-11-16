# 確認の手順

## 権限が必要なテスト（Linux）

```sh
sh scripts/privileged_tests.sh
```

raw ソケット / AF_PACKET を使う `#[ignore]` のテストと、veth でつないだ 2 つの名前空間での link モードの確認
（実際の MAC・自前の ARP・VLAN タグ）をまとめて実行する。root 不要で、ホストのネットワークには触らない。
Windows では WSL の中で動かす。CI でも `privileged` ジョブ（ubuntu-24.04）で毎回実行する。

## TUI を端末で確かめる

実機の NTP サーバがなくても、疑似サーバで確かめられる。

```sh
cargo run -- sim-server --offset-ms -0.978 --refid GNSS   # 別の端末で ↓
cargo run -- tui 127.0.0.1:12300                          # q / Esc / Ctrl+C で終了
```

カレントディレクトリに `config.toml` があると、その設定も使われる（引数の指定が優先）。
手元の設定を使わずに確かめるときは `--config /dev/null`（Windows では `--config NUL`）を付ける。

自動で確かめるときは tmux の中で動かす（Windows には tmux がないので WSL で）。

```sh
tmux new-session -d -s ntp -x 120 -y 40 -n sim "$BIN sim-server --offset-ms -0.978 --refid GNSS"
tmux new-window -t ntp -n tui "$BIN tui 127.0.0.1:12300"
tmux capture-pane -t ntp:tui -p     # 「同期済み」が出たら準備完了
tmux send-keys -t ntp:tui q; tmux kill-session -t ntp
```

大きい数字は背景色付きの空白で描いているので、`capture-pane -p` では見えない。
`capture-pane -p -e` で色の情報を含めて取り出し、背景色の付いた空白を数字の点として読む。
色は 256 色の番号で出る（大きい数字は `48;5;14`、推移の棒は `48;5;6`）。

情報欄の下の「直近 N 回の測定値」（推移のグラフ）は、`tui --trend` を付けたときだけ出す（既定では出さない）。
付けても、出して時計の倍率が変わらない大きさの端末でだけ出る。120×40 なら出るが、80×24 では出ない。
