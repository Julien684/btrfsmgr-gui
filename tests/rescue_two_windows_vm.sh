#!/usr/bin/env bash
# Vérifie la coexistence des deux fenêtres dans UN processus : la fenêtre
# de dépannage est ouverte par le même chemin que le menu Fichier (crochet
# BTRFSMGR_GUI_OPEN_RESCUE_MS), puis capture + code de sortie.
set -uo pipefail
LIB=/tmp/bg
fail=0
ok() { echo "  [OK ] $1"; }
ko() { echo "  [KO ] $1 — $2"; fail=$((fail + 1)); }

rm -f /tmp/target.img
truncate -s 1G /tmp/target.img
mkfs.btrfs -f -q /tmp/target.img || exit 1
LOOP=$(losetup -f --show /tmp/target.img)
mkdir -p /tmp/tmnt
mount "$LOOP" /tmp/tmnt
btrfs subvolume create /tmp/tmnt/@ >/dev/null
btrfs subvolume create /tmp/tmnt/snapshots >/dev/null
echo x >/tmp/tmnt/@/f
btrfs subvolume snapshot -r /tmp/tmnt/@ /tmp/tmnt/snapshots/snap-1 >/dev/null
umount /tmp/tmnt

setsid Xvfb :78 -screen 0 1100x760x24 </dev/null >/dev/null 2>&1 &
XPID=$!
sleep 1

BTRFSMGR_GUI_OPEN_RESCUE_MS=1500 BTRFSMGR_GUI_SMOKE_MS=6000 \
    DISPLAY=:78 QT_QPA_PLATFORM=xcb "$LIB" </dev/null >/tmp/two.log 2>&1 &
APID=$!
sleep 4
DISPLAY=:78 ffmpeg -loglevel error -y -f x11grab -video_size 1100x760 -i :78 \
    -frames:v 1 /tmp/two_windows.png
wait "$APID"
RC=$?
echo "APP_RC=$RC"
[ "$RC" = "0" ] && ok "sortie propre avec les deux fenêtres" || ko "code de sortie $RC" "$(tail -5 /tmp/two.log)"
[ -s /tmp/two_windows.png ] && ok "capture produite" || ko "capture absente" ""

kill "$XPID" 2>/dev/null || true
losetup -d "$LOOP" 2>/dev/null || true
rm -f /tmp/target.img
echo "ECHECS=$fail"
exit "$fail"
