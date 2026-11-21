#!/usr/bin/env bash
# Capture du menu + de la boîte À propos avec le binaire installé (release 18).
# À lancer en root : `sudo bash tests/menu_about_vm.sh`.
set -uo pipefail
BIN=/usr/bin/btrfsmgr-gui
setsid Xvfb :77 -screen 0 1100x760x24 </dev/null >/dev/null 2>&1 &
XPID=$!
sleep 1

BTRFSMGR_GUI_ASSERT_GEOM=1 DISPLAY=:77 QT_QPA_PLATFORM=xcb timeout 20 "$BIN"
echo "MAIN_RC=$?"
BTRFSMGR_GUI_ASSERT_GEOM=1 DISPLAY=:77 QT_QPA_PLATFORM=xcb timeout 20 "$BIN" --rescue
echo "RESCUE_RC=$?"

BTRFSMGR_GUI_OPEN_ABOUT_MS=1500 DISPLAY=:77 QT_QPA_PLATFORM=xcb "$BIN" \
    </dev/null >/tmp/about_vm.log 2>&1 &
sleep 4
DISPLAY=:77 ffmpeg -loglevel error -y -f x11grab -video_size 1100x760 -i :77 \
    -frames:v 1 /tmp/about_vm.png
pkill -x btrfsmgr-gui 2>/dev/null || true
sleep 1
pkill -x Xvfb 2>/dev/null || true
ls -l /tmp/about_vm.png
