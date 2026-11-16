#!/bin/sh
# link モードを、veth でつないだ 2 つのネットワーク名前空間で確かめる（Linux）。
#
# root は不要（unshare でユーザ名前空間を作る）。ホストのネットワークには触らない。
# ループバックでは確かめられない次のことを確かめる:
#   1. 実際の MAC アドレスへの送信と、近隣表が空のときの自前の ARP 要求
#   2. 同じサブネットのサーバへの直接の送信（ゲートウェイを通らない）
#   3. 802.1Q の VLAN タグ（VID と PCP）が付いたまま相手に届くこと
#   4. link モードでのカーネルのタイムスタンプ
#
# 使い方: scripts/link_veth_check.sh <ntpterminal のパス>
set -eu
BIN=${1:?"ntpterminal のパスを指定してください"}

exec unshare -rn sh -s "$BIN" <<'INNER'
set -eu
BIN=$1
fail() { echo "NG: $*"; exit 1; }

# 相手側の名前空間（sleep が生きている間だけある）
unshare -n sleep 60 &
PEER=$!
sleep 0.2
peer() { nsenter -t "$PEER" -n "$@"; }

ip link set lo up
ip link add v0 type veth peer name v1
ip link set v1 netns "$PEER"
ip addr add 10.0.0.1/24 dev v0
ip addr add 10.0.10.1/24 dev v0
ip link set v0 up
peer ip link set lo up
peer ip addr add 10.0.0.2/24 dev v1
peer ip link set v1 up
# VLAN 10 のサブインターフェース（相手側だけ。こちらは link モードでタグを付けて送る）
peer ip link add link v1 name v1.10 type vlan id 10
peer ip addr add 10.0.10.2/24 dev v1.10
peer ip link set v1.10 up

# /sys は元の名前空間のものなので、MAC は ip link から読む
PEER_MAC=$(peer ip -o link show v1 | sed -n 's/.*link\/ether \([0-9a-f:]*\).*/\1/p')
[ -n "$PEER_MAC" ] || fail "相手の MAC が取れない"
peer "$BIN" sim-server --bind 10.0.0.2:12300 --offset-ms -0.978 --refid GNSS 2>/dev/null &
peer "$BIN" sim-server --bind 10.0.10.2:12300 --offset-ms -0.978 --refid VLAN 2>/dev/null &
sleep 0.3

echo "== 1〜2: 近隣表が空の状態で link モード（自前の ARP → 実際の MAC へ送信）=="
[ -z "$(ip neigh show dev v0)" ] || fail "近隣表が空ではない"
OUT=$("$BIN" once 10.0.0.2:12300 --transport link --timestamping user --timeout-ms 1000) || fail "問い合わせに失敗: $OUT"
echo "$OUT" | grep -E "オフセット|Ref ID|送信方式"
echo "$OUT" | grep -q "link（v0 → $PEER_MAC）" || fail "宛先 MAC が相手の MAC（$PEER_MAC）ではない"
echo "OK: ARP で $PEER_MAC を調べて送れた"

echo "== 3: VLAN 10 / PCP 5 のタグ付きで送る =="
peer tcpdump -Z root -l -n -e -c 1 -i v1 'vlan 10 and udp dst port 12300' > /tmp/vlan_capture.txt 2>/dev/null &
TCPDUMP=$!
sleep 0.5
OUT=$("$BIN" once 10.0.10.2:12300 --transport link --vlan 10 --pcp 5 --timestamping user --timeout-ms 1000) || fail "VLAN 付きの問い合わせに失敗: $OUT"
echo "$OUT" | grep -E "Ref ID|送信方式"
echo "$OUT" | grep -q "VLAN" || fail "VLAN 側の疑似サーバの応答ではない"
wait "$TCPDUMP" || true
grep -E "vlan 10, p 5" /tmp/vlan_capture.txt || fail "相手側で VLAN 10 / PCP 5 のタグが見えない: $(cat /tmp/vlan_capture.txt)"
echo "OK: VLAN 10 / PCP 5 のタグが付いたまま届き、応答を受け取れた"

echo "== 4: link モード + カーネルのタイムスタンプ =="
OUT=$("$BIN" once 10.0.0.2:12300 --transport link --timestamping kernel --timeout-ms 1000) || fail "失敗: $OUT"
echo "$OUT" | grep -E "タイムスタンプ"
echo "$OUT" | grep -q "タイムスタンプ *カーネル$" || fail "カーネルの時刻が使われていない"
echo "OK: 送受信ともカーネルの時刻"

kill "$PEER" 2>/dev/null || true
echo "すべて OK"
INNER
