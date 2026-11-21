#!/usr/bin/env bash
# Test d'intégration du mode dépannage sur une image BTRFS en boucle.
# Crée une cible @ + @home + snapshots, y lance btrfsmgr en --selftest,
# puis vérifie création et restauration d'un instantané.
set -uo pipefail
IMG=/tmp/target.img
M=/tmp/tmnt
fail=0
ok() { echo "  [OK ] $1"; }
ko() { echo "  [KO ] $1 — $2"; fail=$((fail + 1)); }

cleanup_loop() {
    losetup -j "$IMG" 2>/dev/null | cut -d: -f1 | while read -r l; do
        umount "$l" 2>/dev/null || true
        losetup -d "$l" 2>/dev/null || true
    done
}

cleanup_loop
rm -f "$IMG"
truncate -s 1G "$IMG"
mkfs.btrfs -f -q "$IMG" || exit 1
LOOP=$(losetup -f --show "$IMG")
rmdir "$M" 2>/dev/null || true
mkdir -p "$M"
mount "$LOOP" "$M"
btrfs subvolume create "$M/@" >/dev/null
btrfs subvolume create "$M/@home" >/dev/null
btrfs subvolume create "$M/snapshots" >/dev/null
ID=$(btrfs subvolume list "$M" | awk '/ path @$/ {print $2}')
btrfs subvolume set-default "$ID" "$M"
echo "v1" >"$M/@/etat"
echo "home" >"$M/@home/f"
btrfs subvolume snapshot -r "$M/@" "$M/snapshots/snap-1" >/dev/null
echo "v2" >"$M/@/etat"
umount "$M"
echo "LOOP=$LOOP"

echo "=== création d'un instantané sur la cible ==="
OUT=$(/tmp/bg --rescue --selftest "$LOOP" --create 2>&1)
echo "$OUT" | sed 's/^/    /'
echo "$OUT" | grep -q "^SNAPSHOTS=1" && ok "instantané existant détecté" || ko "instantané existant non détecté" "$OUT"
echo "$OUT" | grep -q "^CREATE_OK=1" && ok "création d'un instantané" || ko "création échouée" "$OUT"
echo "$OUT" | grep -q "^SNAPSHOTS_AFTER=2" && ok "nouvel instantané visible (2 au total)" || ko "comptage après création" "$OUT"

echo "=== restauration de snap-1 ==="
OUT=$(/tmp/bg --rescue --selftest "$LOOP" --restore snap-1 2>&1)
echo "$OUT" | sed 's/^/    /'
echo "$OUT" | grep -q "^RESTORE_RC=0" && ok "restauration rc=0" || ko "restauration en échec" "$OUT"

echo "=== vérifications BTRFS après restauration ==="
# `mount` simple monterait le sous-volume PAR DÉFAUT (= @) : il faut la racine
# du FS pour voir @, @home et snapshots comme des frères.
mount -o subvolid=5 "$LOOP" "$M"
CUR=$(cat "$M/@/etat" 2>/dev/null)
echo "    contenu de @/etat = $CUR"
[ "$CUR" = "v1" ] && ok "@ contient le contenu de snap-1" || ko "contenu non restauré" "$CUR"
ls -d "$M"/snapshots/@_old-* >/dev/null 2>&1 && ok "ancien @ conservé dans snapshots/" || ko "ancien @ absent" ""
btrfs subvolume show "$M/snapshots" >/dev/null 2>&1 && ok "snapshots reste un sous-volume" || ko "snapshots cassé" ""
[ -d "$M/@home" ] && ok "@home intact" || ko "@home perdu" ""
DEF=$(btrfs subvolume get-default "$M" | sed 's/.*path //')
[ "$DEF" = "@" ] && ok "sous-volume par défaut repointé sur @" || ko "défaut=$DEF" ""
umount "$M"
cleanup_loop
rm -f "$IMG"
echo "ECHECS=$fail"
exit "$fail"
