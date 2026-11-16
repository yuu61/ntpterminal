#!/bin/sh
# raw ソケット / AF_PACKET を使う確認をまとめて実行する（Linux・root 不要・ホストのネットワークに触らない）。
#
# 1. #[ignore] の結合テスト（tests/raw_ip.rs, tests/link.rs）をネットワーク名前空間の中で実行する。
#    lo の route_localnet と accept_local は名前空間の中でだけ 1 にする。AF_PACKET から入れたフレームは
#    受信側の経路判定を通り、127.0.0.1 あて・自分が送信元のものは既定では捨てられるため。
# 2. veth でつないだ 2 つの名前空間で、実際の MAC・自前の ARP・VLAN タグを確かめる（link_veth_check.sh）。
#
# 使い方: sh scripts/privileged_tests.sh
set -eu
cd "$(dirname "$0")/.."

executable() {
    grep -o '"executable":"[^"]*"' | sed 's/"executable":"//; s/"$//' | grep "$1" | head -n 1
}

for test in raw_ip link; do
    bin=$(cargo test -q -p ntpterminal-infrastructure --test "$test" --no-run --message-format=json | executable "/$test-")
    echo "== $test =="
    unshare -rn sh -c "ip link set lo up \
        && echo 1 > /proc/sys/net/ipv4/conf/lo/route_localnet \
        && echo 1 > /proc/sys/net/ipv4/conf/lo/accept_local \
        && '$bin' --ignored --test-threads=1"
done

ntpterminal=$(cargo build -q --message-format=json | executable "/ntpterminal$")
echo "== veth（2 つの名前空間）=="
sh scripts/link_veth_check.sh "$ntpterminal"
