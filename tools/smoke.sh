#!/usr/bin/env bash
# Smoke test de la GUI : lance le binaire sous Xvfb et vérifie que le
# QTabWidget a une géométrie réelle. Détecte la régression « layouts droppés »
# (onglets 0×0, widgets empilés à 0,0) qui ne produit aucune erreur au build.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$HERE/target/release/btrfsmgr-gui}"
DISP="${DISPLAY_SMOKE:-:91}"

[[ -x "$BIN" ]] || { echo "ERREUR: binaire introuvable: $BIN" >&2; exit 1; }

Xvfb "$DISP" -screen 0 1200x900x24 >/dev/null 2>&1 &
XVFB_PID=$!
trap 'kill "$XVFB_PID" 2>/dev/null || true' EXIT
sleep 1

BTRFSMGR_GUI_ALLOW_NONROOT=1 \
BTRFSMGR_GUI_ASSERT_GEOM=1 \
DISPLAY="$DISP" \
QT_QPA_PLATFORM=xcb \
timeout 20 "$BIN" /
