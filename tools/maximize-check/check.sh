#!/usr/bin/env bash
# A maximized window keeps the `maximized_padding` space on every side, inside the work area a
# bar on any edge leaves, and follows a change to the key at once. Off, it meets the work area;
# a fullscreen window ignores the key. An auto-hiding bar on any edge reserves nothing, shows when
# the pointer reaches its edge and hides when it leaves. Runs CC_BIN in tools/headless at
# 1920x1080, scale 1.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
hl=$here/../headless/headless.sh
export CC_SIZE=1920x1080
export CC_STATE=${CC_STATE:-$(mktemp -d "${TMPDIR:-/tmp}/maximize-check.XXXXXX")}
cargo build --quiet --manifest-path "$here/Cargo.toml"
bin=$here/target/debug/maximize-check
cfg=$CC_STATE/home/.config/cosmic/com.system76.CosmicComp/v1
# A client lands in its workspace slice's realm, shown only while a registry calls it active.
ws=$(grep -o 'workspace-[A-Za-z0-9_-]*\.slice' /proc/self/cgroup | tail -n1 | sed 's/^workspace-//; s/\.slice$//' || true)
if [ -n "$ws" ]; then
	reg=${GRANTS_CHECK:-}
	if [ -z "$reg" ]; then
		cargo build --quiet --manifest-path "$here/../grants-check/Cargo.toml"
		reg=$here/../grants-check/target/debug/grants-check
	fi
	export CC_BUS_SERVICE="exec '$reg' registry allow '$ws' none ''" COSMIC_WORKSPACES=1
fi

# padding RON: written whole, as cosmic-config does, so the watcher never reads half a value.
padding() { printf '%s' "$1" >"$cfg/.maximized_padding" && mv "$cfg/.maximized_padding" "$cfg/maximized_padding"; }

"$hl" up >/dev/null
trap '"$hl" down' EXIT
mkdir -p "$cfg"
padding '(enabled: true, size: Some(0))'
client=

start() {
	client=$("$hl" run "$bin" show "$@")
	log=$(ls -t "$CC_STATE"/client-maximize-check-*.log | head -n1)
}

stop() { kill -TERM -- "-$client" 2>/dev/null || kill -TERM "$client"; sleep 0.3; }

# expect NAME "X Y W H": the window is drawn at W x H and shows there, past any animation. An
# inset window's 1px outline covers its outermost pixels, so each edge may sit 1px in.
expect() {
	local name=$1 want=$2 got x y w h
	for _ in $(seq 100); do grep -q "^configure ${want#* * }$" "$log" && break; sleep 0.1; done
	sleep 1
	"$hl" shot "$CC_STATE/$name.ppm"
	got=$("$bin" rect "$CC_STATE/$name.ppm")
	echo "$name: $got"
	read -r x y w h <<<"$want"
	[ "$got" = "$want" ] || [ "$got" = "$((x + 1)) $((y + 1)) $((w - 2)) $((h - 2))" ] || { echo "$name: FAILED, want $want" >&2; tail -n5 "$log" >&2; exit 1; }
}

start bottom
expect bottom-0 "0 0 1920 1028"
padding '(enabled: true, size: Some(24))'
expect bottom-24 "24 24 1872 980"
padding '(enabled: false, size: Some(24))'
expect bottom-off "0 0 1920 1028"
padding '()'
sleep 1.5
"$hl" shot "$CC_STATE/bottom-default.ppm"
read -r x y w h < <("$bin" rect "$CC_STATE/bottom-default.ppm")
echo "bottom-default (the theme's inset): $x $y $w $h"
[ "$x" = "$y" ] && [ "$w" = $((1920 - 2 * x)) ] && [ "$h" = $((1028 - 2 * x)) ] ||
	{ echo "bottom-default: FAILED, not the same inset on every side" >&2; exit 1; }
stop

padding '(enabled: true, size: Some(16))'
start left
expect left-16 "68 16 1836 1048"
padding '(enabled: true, size: Some(40))'
expect left-40 "92 40 1788 1000"
stop

start left --fullscreen
expect fullscreen "0 0 1920 1080"
stop
echo "maximized padding: ok"

# bar NAME WANT: the bar's colour covers WANT ("none" while it is hidden).
bar() {
	local got
	"$hl" shot "$CC_STATE/$1.ppm"
	got=$("$bin" rect "$CC_STATE/$1.ppm" bar)
	echo "$1: bar $got"
	[ "$got" = "$2" ] || { echo "$1: FAILED, want bar $2" >&2; tail -n5 "$log" >&2; exit 1; }
}

padding '(enabled: true, size: Some(16))'
for edge in bottom top left right; do
	case $edge in
	bottom) at="960 1079" shown="0 1028 1920 52" ;;
	top) at="960 0" shown="0 0 1920 52" ;;
	left) at="0 540" shown="0 0 52 1080" ;;
	right) at="1919 540" shown="1868 0 52 1080" ;;
	esac
	"$hl" move 960 540
	start "$edge" --auto-hide
	# Hidden, it leaves the whole output to the maximized window, less the padding.
	expect "auto-$edge" "16 16 1888 1048"
	bar "auto-$edge-hidden" none
	# shellcheck disable=SC2086
	"$hl" move $at
	sleep 1.2
	bar "auto-$edge-shown" "$shown"
	"$hl" move 960 540
	sleep 1.5
	bar "auto-$edge-left" none
	grep -q '^visible 1$' "$log" && grep -q '^visible 0$' "$log" ||
		{ echo "auto-$edge: FAILED, no visibility events" >&2; exit 1; }
	stop
done
echo "auto-hide on every edge: ok"
