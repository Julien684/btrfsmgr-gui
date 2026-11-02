#!/usr/bin/env bash
# Synchronise le cœur Rust depuis le dépôt btrfsmgr puis applique les
# constantes propres à la GUI (nom de paquet, .desktop, version).
#
# Le dépôt GUI est autonome (pas de dépendance git/path vers btrfsmgr) : ce
# script est la seule source de vérité pour éviter une divergence silencieuse.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
SRC="${1:-$HERE/../btrfsmgr}"

[[ -f "$SRC/src/core.rs" ]] || { echo "ERREUR: $SRC/src/core.rs introuvable" >&2; exit 1; }

cp "$SRC/src/core.rs" "$HERE/src/core.rs"
cp "$SRC/src/i18n.rs" "$HERE/src/i18n.rs"
mkdir -p "$HERE/assets"
cp "$SRC/assets/i18n_en.json" "$HERE/assets/i18n_en.json"
cp "$SRC/assets/btrfsmgr-snap.sh" "$HERE/assets/btrfsmgr-snap.sh"

python3 - "$HERE/src/core.rs" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
s = s.replace(
    'pub const AUTO_NOTIF_DESKTOP: &str = "btrfsmgr.desktop";',
    'pub const AUTO_NOTIF_DESKTOP: &str = "btrfsmgr-gui.desktop";\n'
    'pub const GUI_DESKTOP_FILE: &str = "/usr/share/applications/btrfsmgr-gui.desktop";',
)
s = s.replace('pub const VERSION: &str = "1.0.15";', 'pub const VERSION: &str = "1.1.6";')
pos = s.find("\nuse ")
s = s[:pos] + "\n#![allow(dead_code)] // cœur partagé : la GUI n'utilise pas tout\n" + s[pos:]
# La GUI ne doit pas hériter du fallback .desktop du TUI (Terminal=true,
# Exec=sudo btrfsmgr) : elle a son propre lanceur graphique.
s = s.replace('Name=BTRFS Manager\\nComment=', 'Name=BTRFS Manager (GUI)\\nComment=')
s = s.replace('Exec=/usr/bin/sudo /usr/bin/btrfsmgr\\n', 'Exec=btrfsmgr-gui\\n')
s = s.replace('Terminal=true\\n', 'Terminal=false\\n')
# Le garde « ne pas écraser un .desktop déjà installé » doit viser le fichier
# de la GUI, pas celui du TUI.
s = s.replace(
    'if Path::new(DESKTOP_FILE).exists() {\n        // Déjà fourni',
    'if Path::new(GUI_DESKTOP_FILE).exists() {\n        // Déjà fourni',
)
s = s.replace('v.push(d.join("btrfsmgr.desktop")', 'v.push(d.join("btrfsmgr-gui.desktop")')
s = s.replace('d.join("../share/btrfsmgr/btrfsmgr.desktop")', 'd.join("../share/btrfsmgr/btrfsmgr-gui.desktop")')
s = s.replace('v.push(format!("{}/btrfsmgr.desktop", env!("CARGO_MANIFEST_DIR")));',
              'v.push(format!("{}/btrfsmgr-gui.desktop", env!("CARGO_MANIFEST_DIR")));')
s = s.replace('if let Some(parent) = Path::new(DESKTOP_FILE).parent() {',
              'if let Some(parent) = Path::new(GUI_DESKTOP_FILE).parent() {')
s = s.replace('match fs::write(DESKTOP_FILE, content) {\n        Ok(_) => sayln!("  - icône menu installée: {DESKTOP_FILE}"),',
              'match fs::write(GUI_DESKTOP_FILE, content) {\n        Ok(_) => sayln!("  - icône menu installée: {GUI_DESKTOP_FILE}"),')
s = s.replace('for path in ["/bin/btrfsmgr", "/usr/local/bin/btrfsmgr", SNAP_SCRIPT] {',
              'for path in ["/bin/btrfsmgr-gui", "/usr/local/bin/btrfsmgr-gui", SNAP_SCRIPT] {')
s = s.replace(
    'if Path::new(DESKTOP_FILE).exists() {\n        let _ = fs::remove_file(DESKTOP_FILE);\n        println!("  - icône menu retirée: {DESKTOP_FILE}");\n    }',
    'for df in [DESKTOP_FILE, GUI_DESKTOP_FILE] {\n        if Path::new(df).exists() {\n            let _ = fs::remove_file(df);\n            println!("  - icône menu retirée: {df}");\n        }\n    }',
)
open(p, "w").write(s)
PY
echo "cœur synchronisé depuis $SRC"
