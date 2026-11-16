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

## GNSS 受信機の行（SNMP）を確かめる

疑似サーバに `--snmp-mib` を付けると、疑似 SNMP エージェント（既定は 127.0.0.1:16100、コミュニティ名は public）も
起動し、その MIB で GNSS 受信機の値を返す（meinberg-os / meinberg-ltng / meinberg-lt / microchip-s650）。

```sh
cargo run -- sim-server --offset-ms -0.978 --refid GNSS --snmp-mib meinberg-ltng   # 別の端末で ↓
cargo run -- tui 127.0.0.1:12300 --snmp-community public --snmp-port 16100
```

情報欄の「Ref ID」の下に「GNSS 受信機」と「衛星」の 2 行が出る（`--snmp-community` を付けたときだけ）。
MIB は自動で判定する。`tui --snmp-mib` で固定もでき、固定した MIB が応答しなければ、ほかの MIB に切り替えずに
エラーを出す。疑似エージェントの衛星の数は 20 秒ごとに変わる。microchip-s650 では、衛星が多いと一覧が
128 バイトで切れて衛星系の内訳が出なくなる（実機と同じ）。疑似エージェントは net-snmp でも読める:
`snmpwalk -v2c -c public 127.0.0.1:16100 .1.3.6.1`

実機に問い合わせるときは、機器の SNMP の設定（v2c を有効にする・コミュニティ名・問い合わせを許すアドレス）を
確かめておく。対応している MIB と、メーカーごとの衛星の数の意味は `crates/application/src/acl/mib.rs` の先頭にある。

## 推移のグラフ

情報欄の下の「直近 N 回の測定値」（推移のグラフ）は、`tui --trend` を付けたときだけ出す（既定では出さない）。
付けても、出して時計の倍率が変わらない大きさの端末でだけ出る。120×40 なら出るが、80×24 では出ない。
