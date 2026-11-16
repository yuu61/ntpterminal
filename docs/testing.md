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

自動で確かめるときは tmux の中で動かす（Windows には tmux がないので WSL で）。

```sh
tmux new-session -d -s ntp -x 120 -y 40 -n sim "$BIN sim-server --offset-ms -0.978 --refid GNSS"
tmux new-window -t ntp -n tui "$BIN tui 127.0.0.1:12300"
tmux capture-pane -t ntp:tui -p     # 「同期済み」が出たら準備完了
tmux send-keys -t ntp:tui q; tmux kill-session -t ntp
```

大きい数字は背景色付きの空白で描いているので、`capture-pane -p` では見えない。
`capture-pane -p -e` で色の情報を含めて取り出し、背景色の付いた空白を数字の点として読む。
