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

Tests du mode dépannage (nécessitent root et btrfs-progs ; conçus pour la VM) :

```bash
sudo tests/rescue_e2e.sh             # image BTRFS en boucle : analyse, création, restauration
sudo tests/rescue_gui_vm.sh          # rendu Xvfb des deux fenêtres
sudo tests/rescue_two_windows_vm.sh  # coexistence des deux fenêtres, code de sortie
sudo tests/menu_about_vm.sh          # capture menu + À propos avec le binaire installé
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

La barre d'en-tête porte quatre boutons : « Dépannage (live)… », « À propos »
(version, résumé des onglets, mode dépannage, licence), « Actualiser » et
« Quitter ». Une barre de menus `QMenuBar` a été essayée puis abandonnée : sous
un thème sombre Qt dessinait ses intitulés avec une couleur quasi identique au
fond (illisibles), et `qtrs` n'expose ni API de palette ni `setNativeMenuBar`
pour corriger. Les boutons, eux, suivent la palette et restent lisibles.

« Dépannage (live)… » ouvre une seconde fenêtre indépendante dans le même
processus (voir `src/rescue.rs`) : sélection d'un autre disque BTRFS non
monté, analyse de ses sous-volumes, montage sous `/run/btrfsmgr/rescue`, puis
création ou restauration d'un instantané. Aucune écriture dans `/etc`,
`/boot`, systemd ni fstab de la cible.

## Lancement

L'application exige root : lancement normal relance via `pkexec` en
préservant `DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY`, `XDG_RUNTIME_DIR`…
Crochets de test (sans effet en usage normal) :

* `BTRFSMGR_GUI_ALLOW_NONROOT=1` : ne pas relancer (smoke test) ;
* `BTRFSMGR_GUI_SMOKE_MS=<ms>` : quitte après N ms ;
* `BTRFSMGR_GUI_TAB=<0..3>` : ouvre directement un onglet ;
* `BTRFSMGR_GUI_ASSERT_GEOM=1` : vérifie la géométrie (onglets, ou tableau de
  la fenêtre de dépannage) puis quitte avec 0/1 ;
* `BTRFSMGR_GUI_OPEN_RESCUE_MS=<ms>` : ouvre la fenêtre de dépannage depuis le
  processus principal, exactement comme le menu (test de coexistence) ;
* `BTRFSMGR_GUI_OPEN_ABOUT_MS=<ms>` : ouvre « Aide → À propos » (dialogue
  modal, destiné à une capture suivie d'un arrêt du processus).

Modes en ligne de commande :

* `--rescue` : démarre directement sur la fenêtre de dépannage ;
* `--rescue --selftest [/dev/loopX] [--device D] [--subvol S] [--snapdir D]
  [--create] [--restore NOM]` : vérification sans interface (liste des disques,
  analyse, montage, instantanés) ; `--create` et `--restore` exercent les
  chemins d'écriture. Les lignes `CLÉ=valeur` sont le contrat des tests.

## Vérifications effectuées

* rendu des 4 onglets sous Xvfb (captures) ;
* exécution sur la VM Solus (Qt 6.11.3), données réelles affichées
  (systemd-boot, instantanés, disque) ;
* création d'un instantané par interaction réelle (clic « Créer un
  instantané », saisie du nom, validation) vérifiée côté BTRFS.

Mode dépannage :

* `cargo test` : 22 tests (cœur, i18n, découverte des disques, validation des
  noms de sous-volume et de dossier, options de montage) ;
* `tests/rescue_e2e.sh` sur la VM : image BTRFS en boucle avec `@`, `@home`,
  `snapshots` et un instantané — analyse (`SUBVOLS`, `DEFAULT=@`), création
  (`CREATE_OK=1`), restauration (`RESTORE_RC=0`), puis vérification du contenu
  restauré, de l'ancien `@` conservé en `snapshots/@_old-*`, de `@home` intact
  et du sous-volume par défaut repointé (0 échec) ;
* rendu de la fenêtre sous Xvfb (hôte et VM), disque cible listé, et disque
  réel de la VM correctement écarté avec son point de montage dans le journal ;
* coexistence des deux fenêtres dans un seul processus, code de sortie 0 ;
* barre de menus (séparateur + gras) et « Aide → À propos » capturés sur la
  VM avec le binaire installé ; test unitaire sur le contenu du résumé.

## État

* L'`.eopkg` Rust/Qt6 est construit : `btrfsmgr-gui-1.1.8-21-1-x86_64.eopkg`
  (binaire + `.desktop` + policy polkit ; ne fournit plus `/usr/bin/btrfsmgr`,
  qui appartient au paquet TUI). Installé et vérifié sur la VM jusqu'à la
  release 20 ; le binaire embarqué dans la 21 est celui qui a passé les tests
  (`sha256 bf3436c9…`).
* `install.sh` a été retiré : installation depuis les sources via
  `cargo build --release` puis copie du binaire.
* Tests d'interaction automatisés des autres boutons (restauration,
  suppression, timer) — le cœur est déjà couvert par les tests du dépôt
  `btrfsmgr`.

