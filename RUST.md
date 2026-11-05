# btrfsmgr-gui — version Rust/Qt6

Portage de l'interface GTK4 (`btrfsmgr-gui.py`, 1245 lignes) vers Qt6 via
`qtrs`, dans la continuité des autres outils Rust de l'auteur
(`ollama-manager-qt`, `solus-external-kernel-manager`).

## Construction

```bash
cargo build --release      # target/release/btrfsmgr-gui
cargo test --release
./tools/sync-core.sh       # resynchronise le cœur depuis ../btrfsmgr
./tools/smoke.sh           # rendu sous Xvfb + contrôle de géométrie des onglets
```

Le dépôt est autonome : `src/core.rs` et `src/i18n.rs` sont une **copie**
du cœur du dépôt `btrfsmgr` (copie synchronisée par `tools/sync-core.sh`), avec
les constantes propres à la GUI appliquées par `tools/sync-core.sh`.

## Interface

Quatre onglets, comme la version GTK4 :

* **Général** : version, chargeur de boot, sous-volume par défaut, nombre
  d'instantanés, timers actifs, espace disque.
* **Snapshots** : tableau (nom, génération, chemin complet), création,
  restauration, suppression multi-sélection, rétention.
* **Automatisation** : formulaire (nom du plan, fréquence, jour, heure,
  démarrage, rétention), liste des timers, suppression.
* **Boot** : chargeur détecté, synchronisation des entrées systemd-boot,
  réglage du timeout `clr-boot-manager`.

Journal + barre de statut en bas.

## Lancement

L'application exige root : lancement normal relance via `pkexec` en
préservant `DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY`, `XDG_RUNTIME_DIR`…
Crochets de test (sans effet en usage normal) :

* `BTRFSMGR_GUI_ALLOW_NONROOT=1` : ne pas relancer (smoke test) ;
* `BTRFSMGR_GUI_SMOKE_MS=<ms>` : quitte après N ms ;
* `BTRFSMGR_GUI_TAB=<0..3>` : ouvre directement un onglet.

## Vérifications effectuées

* rendu des 4 onglets sous Xvfb (captures) ;
* exécution sur la VM Solus (Qt 6.11.3), données réelles affichées
  (systemd-boot, instantanés, disque) ;
* création d'un instantané par interaction réelle (clic « Créer un
  instantané », saisie du nom, validation) vérifiée côté BTRFS.

## État

* L'`.eopkg` Rust/Qt6 est construit : `btrfsmgr-gui-1.1.6-9-1-x86_64.eopkg`
  (binaire + `.desktop` + policy polkit ; ne fournit plus `/usr/bin/btrfsmgr`,
  qui appartient au paquet TUI).
* `install.sh` a été retiré : installation depuis les sources via
  `cargo build --release` puis copie du binaire.
* Tests d'interaction automatisés des autres boutons (restauration,
  suppression, timer) — le cœur est déjà couvert par les tests du dépôt
  `btrfsmgr`.

