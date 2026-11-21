#!/usr/bin/env bash
# Vérification GUI du mode dépannage sur la VM (Xvfb, root).
# Contrôle aussi la non-régression de la fenêtre principale (4 onglets).
set -uo pipefail
LIB=/tmp/bg
IMG=/tmp/target.img
M=/tmp/tmnt
fail=0
ok() { echo "  [OK ] $1"; }
ko() { echo "  [KO ] $1 — $2"; fail=$((fail + 1)); }

rm -f "$IMG"
truncate -s 1G "$IMG"
mkfs.btrfs -f -q "$IMG" || exit 1
LOOP=$(losetup -f --show "$IMG")
mkdir -p "$M"
mount "$LOOP" "$M"
btrfs subvolume create "$M/@" >/dev/null
btrfs subvolume create "$M/snapshots" >/dev/null
echo x >"$M/@/f"
btrfs subvolume snapshot -r "$M/@" "$M/snapshots/snap-1" >/dev/null
umount "$M"
echo "LOOP=$LOOP (disque cible visible, non monté)"

setsid Xvfb :77 -screen 0 1100x760x24 </dev/null >/dev/null 2>&1 &
XPID=$!
sleep 1

echo "=== fenêtre principale : non-régression ==="
OUT=$(BTRFSMGR_GUI_ASSERT_GEOM=1 DISPLAY=:77 QT_QPA_PLATFORM=xcb timeout 20 "$LIB" 2>&1)
echo "$OUT" | sed 's/^/    /'
echo "$OUT" | grep -q "GEOM_OK tabs" && ok "4 onglets rendus" || ko "rendu des onglets" "$OUT"

echo "=== fenêtre dépannage ==="
OUT=$(BTRFSMGR_GUI_ASSERT_GEOM=1 DISPLAY=:77 QT_QPA_PLATFORM=xcb timeout 20 "$LIB" --rescue 2>&1)
echo "$OUT" | sed 's/^/    /'
echo "$OUT" | grep -q "GEOM_OK rescue" && ok "fenêtre de dépannage rendue" || ko "rendu dépannage" "$OUT"

echo "=== capture d'écran ==="
BTRFSMGR_GUI_SMOKE_MS=5000 DISPLAY=:77 QT_QPA_PLATFORM=xcb "$LIB" --rescue \
    </dev/null >/tmp/rescue.log 2>&1 &
APID=$!
sleep 3
DISPLAY=:77 ffmpeg -loglevel error -y -f x11grab -video_size 1100x760 -i :77 \
    -frames:v 1 /tmp/rescue_vm.png
wait "$APID" || true
if [ -s /tmp/rescue_vm.png ]; then
    ok "capture produite"
else
    ko "capture absente" ""
fi
grep -v '^$' /tmp/rescue.log | head -5 | sed 's/^/    log: /' || true

kill "$XPID" 2>/dev/null || true
losetup -d "$LOOP" 2>/dev/null || true
rm -f "$IMG"
echo "ECHECS=$fail"
exit "$fail"
