#!/usr/bin/env bash
# Capture de la fenêtre btrfsmgr dans la VRAIE session KDE (thème sombre)
# pour juger le contraste réel de la barre de menus.
# Usage (root) : sudo bash tests/dark_capture.sh /tmp/bg2
set -uo pipefail
BIN=${1:-/tmp/bg2}
PS=$(pgrep -x plasmashell | head -1)
if [ -z "${PS:-}" ]; then
    echo "plasmashell introuvable"
    exit 1
fi
getenv() {
    cat /proc/"$PS"/environ 2>/dev/null | tr '\0' '\n' |
        awk -F= -v k="$1" '$1 == k { sub($1 "=", ""); print; exit }'
}
DISP=$(getenv DISPLAY)
XAUTH=$(getenv XAUTHORITY)
RUNTIME=$(getenv XDG_RUNTIME_DIR)
DESKTOP=$(getenv XDG_CURRENT_DESKTOP)
echo "DISPLAY=$DISP XAUTHORITY=$XAUTH RUNTIME=$RUNTIME DESKTOP=$DESKTOP"
[ -n "$DISP" ] || { echo "DISPLAY introuvable"; exit 1; }

env DISPLAY="$DISP" XAUTHORITY="$XAUTH" XDG_RUNTIME_DIR="$RUNTIME" \
    XDG_CURRENT_DESKTOP="$DESKTOP" QT_QPA_PLATFORM=xcb "$BIN" \
    </dev/null >/tmp/sess.log 2>&1 &
sleep 6

export DISPLAY="$DISP" XAUTHORITY="$XAUTH"
WID=$(xdotool search --name "Btrfsmgr" 2>/dev/null | tail -1)
echo "WID=$WID"
if [ -z "$WID" ]; then
    echo "fenêtre introuvable"; tail -5 /tmp/sess.log; exit 1
fi
xdotool getwindowgeometry "$WID"
import -window "$WID" /tmp/dark.png 2>&1 | head -2
pkill -x btrfsmgr-gui 2>/dev/null || true
sleep 1
ls -l /tmp/dark.png 2>/dev/null
