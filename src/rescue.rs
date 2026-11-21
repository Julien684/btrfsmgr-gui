//! Mode dépannage (session live) : agir sur les instantanés d'un disque BTRFS
//! qui n'est PAS la racine du système en cours.
//!
//! Périmètre volontairement réduit à la création et à la restauration d'un
//! instantané : rien n'est écrit dans `/etc`, `/boot`, systemd ni fstab de la
//! cible. Les fonctions métier sont celles de `core.rs`, réutilisées telles
//! quelles via un `root` qui pointe sur le point de montage temporaire — elles
//! sont déjà entièrement relatives à `root`.
//!
//! Le disque cible est monté sous [`RESCUE_MOUNT`] et démonté par le `Drop` de
//! [`TargetMount`] (fermeture de la fenêtre ou démontage explicite).

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::Path;
use std::rc::Rc;

use cxx::let_cxx_string;
use qtrs::ffi;
use qtrs::prelude::*;
use qtrs::{
    get_text, ComboBox, GridLayout, GroupBox, HBoxLayout, Label, LineEdit, MainWindow, MessageBox,
    PlainTextEdit, PushButton, ScrollArea, Spacer, SpacerExt, TableWidget, VBoxLayout, Widget,
    CANCEL, INFORMATION, YES,
};

use crate::core::*;
use crate::i18n::t;

/// Point de montage de la cible pendant la session de dépannage.
pub const RESCUE_MOUNT: &str = "/run/btrfsmgr/rescue";

/// Dossiers d'instantanés essayés, dans l'ordre, quand la cible n'est pas
/// joignable par la configuration locale (le GUI ne lit pas snapshots.state).
pub const SNAPDIR_CANDIDATES: [&str; 3] = ["snapshots", ".snapshots", "@snapshots"];

// ---------------------------------------------------------------------------
// Backend — fonctions pures (testables sans root ni BTRFS)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Disk {
    /// Libellé affiché dans la liste déroulante.
    pub label: String,
    /// Chemin du périphérique (`/dev/sdb2`).
    pub path: String,
    /// Points de montage existants : non vide = disque en cours d'utilisation.
    pub mountpoints: Vec<String>,
}

impl Disk {
    pub fn is_mounted(&self) -> bool {
        !self.mountpoints.is_empty()
    }
}

/// Taille lisible (base 1024).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["o", "Kio", "Mio", "Gio", "Tio", "Pio"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} o")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Décompose la sortie de `lsblk -J` : tous les systèmes BTRFS sont remontés,
/// avec leurs points de montage. Le filtrage « utilisable maintenant » est fait
/// par [`free_disks`] — un disque monté doit rester visible pour que
/// l'utilisateur comprenne pourquoi il ne peut pas le choisir.
pub fn parse_lsblk(json: &str) -> Vec<Disk> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(list) = v.get("blockdevices").and_then(|x| x.as_array()) {
        for node in list {
            walk_lsblk(node, &mut out);
        }
    }
    out
}

fn walk_lsblk(node: &serde_json::Value, out: &mut Vec<Disk>) {
    let text = |key: &str| {
        node.get(key)
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if text("fstype").as_deref() == Some("btrfs") {
        if let Some(path) = text("path") {
            if path.starts_with("/dev/") {
                let size = node.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
                let name = text("label")
                    .or_else(|| text("model"))
                    .unwrap_or_else(|| "btrfs".into());
                let mountpoints: Vec<String> = text("mountpoint")
                    .map(|m| {
                        m.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(Disk {
                    label: format!("{} — {} — {}", path, human_size(size), name),
                    path,
                    mountpoints,
                });
            }
        }
    }
    if let Some(children) = node.get("children").and_then(|x| x.as_array()) {
        for child in children {
            walk_lsblk(child, out);
        }
    }
}

/// Disques manipulables immédiatement : BTRFS et non montés. Un périphérique
/// déjà monté est écarté (le démonter casserait un montage en cours) ; c'est
/// aussi ce qui écarte la clé live, en squashfs/overlay.
pub fn free_disks(disks: &[Disk]) -> Vec<Disk> {
    disks.iter().filter(|d| !d.is_mounted()).cloned().collect()
}

/// Un nom de sous-volume utilisable comme option de montage : un seul niveau,
/// pas de `..`, pas d'espace ni de virgule (injection d'options).
pub fn valid_subvol(name: &str) -> bool {
    let s = name.trim();
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.starts_with('/')
        && !s.contains('/')
        && !s.chars().any(|c| c.is_whitespace() || c == ',')
}

/// Un dossier d'instantanés relatif, sous la racine du sous-volume.
pub fn valid_snapdir(name: &str) -> bool {
    let s = name.trim();
    !s.is_empty()
        && !s.starts_with('/')
        && s != "."
        && !s.split('/').any(|p| p.is_empty() || p == "." || p == "..")
        && !s.chars().any(|c| c.is_whitespace())
}

/// Chemin du périphérique : on n'accepte que `/dev/...`.
pub fn valid_device(path: &str) -> bool {
    path.starts_with("/dev/") && !path.chars().any(|c| c.is_whitespace())
}

/// Sous-volumes de premier niveau (les seuls montables par nom).
pub fn top_level_subvols(paths: impl Iterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = paths
        .map(|p| p.trim_matches('/').to_string())
        .filter(|p| !p.is_empty() && !p.contains('/'))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Premier dossier d'instantanés présent, dans l'ordre de [`SNAPDIR_CANDIDATES`].
pub fn choose_snapdir(exists: impl Fn(&str) -> bool) -> String {
    SNAPDIR_CANDIDATES
        .iter()
        .find(|c| exists(c))
        .map(|c| c.to_string())
        .unwrap_or_default()
}

/// Options de montage de la cible (forme testée).
pub fn mount_options(subvol: &str) -> String {
    format!("subvol={},rw", subvol.trim())
}

// ---------------------------------------------------------------------------
// Backend — accès système
// ---------------------------------------------------------------------------

pub fn list_disks() -> Vec<Disk> {
    let (rc, out) = runv(
        &sargs(&[
            "lsblk",
            "-J",
            "-b",
            "-o",
            "NAME,PATH,TYPE,FSTYPE,LABEL,SIZE,MOUNTPOINT,MODEL",
        ]),
        false,
    );
    if rc != 0 {
        return Vec::new();
    }
    parse_lsblk(&out)
}

/// Montage temporaire, démonté au Drop.
struct ScratchMount {
    path: String,
}

impl ScratchMount {
    fn new() -> Result<Self, String> {
        let base = std::env::temp_dir();
        let path = base.join(format!("btrfsmgr-probe.{}", std::process::id()));
        fs::create_dir_all(&path).map_err(|e| format!("création de {path:?} impossible: {e}"))?;
        Ok(ScratchMount {
            path: path.to_string_lossy().into_owned(),
        })
    }
}

impl Drop for ScratchMount {
    fn drop(&mut self) {
        runv(&sargs(&["umount", &self.path]), false);
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Probe {
    pub subvols: Vec<String>,
    pub default: String,
    pub snapdirs: Vec<String>,
}

/// Monte la racine du FS en lecture seule et inventorie sous-volumes, défaut et
/// dossiers d'instantanés existants.
pub fn probe_subvols(dev: &str) -> Result<Probe, String> {
    if !valid_device(dev) {
        return Err(format!("périphérique invalide : {dev}"));
    }
    let scratch = ScratchMount::new()?;
    let (rc, out) = runv(
        &sargs(&["mount", "-o", "subvolid=5,ro", dev, &scratch.path]),
        false,
    );
    if rc != 0 {
        return Err(format!(
            "montage en lecture seule de {dev} impossible :\n{}",
            out.trim()
        ));
    }
    let subvols = top_level_subvols(subvols_by_id(&scratch.path).into_values());
    let default = default_subvol(&scratch.path).path;
    // `snapshots/` est un sous-volume FRÈRE de @ sur Solus (monté par fstab),
    // mais certaines installations le placent dans le sous-volume racine :
    // on accepte les deux emplacements.
    let inside_default = |c: &str| {
        !default.is_empty()
            && Path::new(&join_path(&join_path(&scratch.path, &default), c)).is_dir()
    };
    let snapdirs: Vec<String> = SNAPDIR_CANDIDATES
        .iter()
        .filter(|c| Path::new(&join_path(&scratch.path, c)).is_dir() || inside_default(c))
        .map(|c| c.to_string())
        .collect();
    Ok(Probe {
        subvols,
        default,
        snapdirs,
    })
}

/// Sous-volume cible monté en écriture, démonté au Drop.
pub struct TargetMount {
    pub dev: String,
    pub subvol: String,
    pub path: String,
    /// Montage secondaire du dossier des instantanés (sous-volume frère, comme
    /// le fait fstab sur le système cible) ; démonté avant le principal.
    nested: Option<String>,
    mounted: bool,
}

/// Le dossier des instantanés est-il un sous-volume à la racine du FS ?
fn snapdir_is_fsroot_subvol(dev: &str, snapdir: &str) -> Result<bool, String> {
    let scratch = ScratchMount::new()?;
    let (rc, _out) = runv(
        &sargs(&["mount", "-o", "subvolid=5,ro", dev, &scratch.path]),
        false,
    );
    if rc != 0 {
        return Ok(false);
    }
    Ok(is_btrfs_subvolume(&join_path(&scratch.path, snapdir)))
}

pub fn mount_target(dev: &str, subvol: &str, snapdir: &str) -> Result<TargetMount, String> {
    if !valid_device(dev) {
        return Err(format!("périphérique invalide : {dev}"));
    }
    if !valid_subvol(subvol) {
        return Err(format!("sous-volume invalide : {subvol}"));
    }
    if fs::create_dir_all(RESCUE_MOUNT).is_err() {
        return Err(format!("création de {RESCUE_MOUNT} impossible"));
    }
    // Ne jamais empiler un montage sur un point déjà occupé.
    if runv(
        &sargs(&["findmnt", "-n", "-o", "SOURCE", RESCUE_MOUNT]),
        false,
    )
    .0 == 0
    {
        return Err(format!("{RESCUE_MOUNT} est déjà un point de montage"));
    }
    // Ni monter un périphérique déjà monté ailleurs (démonter la cible
    // casserait un montage existant).
    let (rc, mounted) = runv(
        &sargs(&["findmnt", "-n", "-o", "TARGET", "--source", dev]),
        false,
    );
    if rc == 0 && !mounted.trim().is_empty() {
        return Err(format!(
            "{dev} est déjà monté sur : {}",
            mounted.trim().replace('\n', " ")
        ));
    }
    let (rc, out) = runv(
        &sargs(&["mount", "-o", &mount_options(subvol), dev, RESCUE_MOUNT]),
        false,
    );
    if rc != 0 {
        return Err(format!(
            "montage de {dev}[{subvol}] impossible :\n{}",
            out.trim()
        ));
    }
    // Reproduire la vue du système cible : quand `snapshots/` est un
    // sous-volume frère de @, fstab le monte sur /snapshots. Sans ce montage,
    // lister/créer un instantané écrirait dans le vide ou au mauvais endroit.
    let mut nested = None;
    if !snapdir.trim().is_empty() && snapdir_is_fsroot_subvol(dev, snapdir).unwrap_or(false) {
        let inner = join_path(RESCUE_MOUNT, snapdir);
        let already = runv(&sargs(&["findmnt", "-n", "-o", "SOURCE", &inner]), false).0 == 0;
        let ready = if already {
            true
        } else {
            fs::create_dir_all(&inner).is_ok()
                && runv(
                    &sargs(&[
                        "mount",
                        "-o",
                        &format!("subvol=/{},rw", snapdir.trim_matches('/')),
                        dev,
                        &inner,
                    ]),
                    false,
                )
                .0 == 0
        };
        if !ready {
            runv(&sargs(&["umount", RESCUE_MOUNT]), false);
            return Err(format!(
                "montage du dossier d'instantanés /{snapdir} impossible"
            ));
        }
        nested = Some(inner);
    }
    Ok(TargetMount {
        dev: dev.to_string(),
        subvol: subvol.to_string(),
        path: RESCUE_MOUNT.to_string(),
        nested,
        mounted: true,
    })
}

impl TargetMount {
    /// Démontage explicite (idempotent) ; renvoie la sortie d'erreur éventuelle.
    pub fn unmount(&mut self) -> String {
        if !self.mounted {
            return String::new();
        }
        if let Some(inner) = self.nested.take() {
            runv(&sargs(&["umount", &inner]), false);
        }
        let (rc, out) = runv(&sargs(&["umount", &self.path]), false);
        if rc == 0 || out.contains("not mounted") || out.contains("n'est pas monté") {
            self.mounted = false;
            String::new()
        } else {
            out.trim().to_string()
        }
    }
}

impl Drop for TargetMount {
    fn drop(&mut self) {
        let _ = self.unmount();
    }
}

/// Inventaire des instantanés du sous-volume monté.
pub fn target_snapshots(root: &str, snapdir: &str) -> Vec<Snapshot> {
    if snapdir.trim().is_empty() {
        return Vec::new();
    }
    let mut v = list_snapshots(root, snapdir);
    v.reverse(); // plus récent en premier, comme l'onglet Snapshots
    v
}

// ---------------------------------------------------------------------------
// Mode autonome de vérification (`--rescue --selftest [disque]`)
// ---------------------------------------------------------------------------

/// Vérification sans interface : liste les disques, puis sonde — et si un
/// disque est fourni — monte, liste les instantanés et démonte.
/// Les lignes `CLÉ=valeur` sont le contrat lu par les tests.
pub fn self_test(args: &[String]) -> i32 {
    let mut dev: Option<String> = None;
    let mut subvol = String::new();
    let mut snapdir = String::new();
    let create = args.iter().any(|a| a == "--create");
    let mut restore_name: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--device" => {
                dev = args.get(i + 1).cloned();
                i += 2;
                continue;
            }
            "--subvol" => {
                subvol = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
                continue;
            }
            "--snapdir" => {
                snapdir = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
                continue;
            }
            "--restore" => {
                restore_name = args.get(i + 1).cloned();
                i += 2;
                continue;
            }
            _ if a.starts_with('-') => {}
            _ if dev.is_none() => dev = Some(a.to_string()),
            _ => {}
        }
        i += 1;
    }

    let disks = free_disks(&list_disks());
    println!("DISKS={}", disks.len());
    for d in &disks {
        println!("DISK path={} label={}", d.path, d.label);
    }
    let Some(dev) = dev else {
        return 0;
    };
    println!("TARGET_DEV={dev}");

    let probe = match probe_subvols(&dev) {
        Ok(p) => p,
        Err(e) => {
            println!("PROBE_ERR={}", e.replace('\n', " | "));
            return 1;
        }
    };
    println!("SUBVOLS={}", probe.subvols.join(","));
    println!("DEFAULT={}", probe.default);
    println!("SNAPDIRS={}", probe.snapdirs.join(","));

    if subvol.trim().is_empty() {
        subvol = if probe.default.is_empty() {
            probe.subvols.first().cloned().unwrap_or_default()
        } else {
            probe.default.clone()
        };
    }
    if !valid_subvol(&subvol) {
        println!("MOUNT_ERR=a_gucun_sous-volume_valide");
        return 1;
    }
    if snapdir.trim().is_empty() {
        snapdir = probe
            .snapdirs
            .first()
            .cloned()
            .unwrap_or_else(|| DEFAULT_SNAPDIR.to_string());
    }
    println!("MOUNT_SUBVOL={subvol}");
    println!("SNAPDIR={snapdir}");

    let mut m = match mount_target(&dev, &subvol, &snapdir) {
        Ok(m) => m,
        Err(e) => {
            println!("MOUNT_ERR={}", e.replace('\n', " | "));
            return 1;
        }
    };
    println!("MOUNT_PATH={}", m.path);
    let snaps = target_snapshots(&m.path, &snapdir);
    println!("SNAPSHOTS={}", snaps.len());
    for s in &snaps {
        println!("SNAPSHOT name={} gen={}", s.path, s.generation);
    }
    // Chemins d'écriture : création, puis restauration (destructive mais
    // attendue — utilisée par les tests sur image en boucle).
    if create {
        let name = format!("selftest-{}", now_stamp());
        let target = create_snapshot(&m.path, &snapdir, Some(&name));
        println!("CREATED={name}");
        println!("CREATE_OK={}", if target.is_empty() { 0 } else { 1 });
    }
    if let Some(name) = restore_name.as_deref() {
        let rc = restore_snapshot(&m.path, &snapdir, name);
        println!("RESTORE_RC={rc}");
    }
    if create || restore_name.is_some() {
        println!(
            "SNAPSHOTS_AFTER={}",
            target_snapshots(&m.path, &snapdir).len()
        );
    }
    let err = m.unmount();
    if !err.is_empty() {
        println!("UMOUNT_ERR={}", err.replace('\n', " | "));
        return 1;
    }
    println!("UMOUNT_OK=1");
    0
}

// ---------------------------------------------------------------------------
// Interface
// ---------------------------------------------------------------------------

struct RescueUi {
    log: Rc<RefCell<PlainTextEdit>>,
    table: Rc<RefCell<TableWidget>>,
    rows: RefCell<Vec<Snapshot>>,
    disks: RefCell<Vec<Disk>>,
    mount: RefCell<Option<TargetMount>>,
    snapdir: RefCell<String>,
    p_dev: *mut ffi::QComboBox,
    p_subvol: *mut ffi::QComboBox,
    p_snapdir: *mut ffi::QLineEdit,
    l_state: *mut ffi::QLabel,
    l_status: *mut ffi::QLabel,
    // Renseignés après la création des boutons (les callbacks ont besoin d'un
    // Weak<RescueUi>, donc de l'état complet) ; `Cell` évite un second passage.
    btn_umount: Cell<*mut ffi::QPushButton>,
    btn_create: Cell<*mut ffi::QPushButton>,
    btn_restore: Cell<*mut ffi::QPushButton>,
}

fn label_set(p: *mut ffi::QLabel, text: &str) {
    if p.is_null() {
        return;
    }
    let_cxx_string!(s = text);
    unsafe { ffi::QLabel_setText(p, &s) };
}

fn set_enabled(p: *mut ffi::QPushButton, enabled: bool) {
    if p.is_null() {
        return;
    }
    unsafe { ffi::QWidget_setEnabled(p as *mut ffi::QWidget, enabled) };
}

fn combo_text(p: *mut ffi::QComboBox) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { ffi::QComboBox_currentText(p) }
}

fn line_text(p: *mut ffi::QLineEdit) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { ffi::QLineEdit_text(p) }
}

fn question(title: &str, text: &str) -> bool {
    MessageBox::new()
        .icon(INFORMATION)
        .window_title(title.to_string())
        .text(text.to_string())
        .standard_buttons(YES | CANCEL)
        .default_button(CANCEL)
        .build()
        .exec()
        == YES
}

impl RescueUi {
    fn log_line(&self, text: &str) {
        let log = self.log.borrow_mut();
        for l in text.lines() {
            log.append_plain_text(l);
        }
    }

    fn set_status(&self, text: &str) {
        label_set(self.l_status, text);
    }

    /// Exécute une opération du cœur en capturant sa sortie vers le journal.
    fn run_core<T>(&self, f: impl FnOnce() -> T) -> T {
        let (res, out) = capture(f);
        if !out.trim().is_empty() {
            self.log_line(out.trim_end());
        }
        res
    }

    fn selected_disk(&self) -> Option<Disk> {
        let text = combo_text(self.p_dev);
        self.disks
            .borrow()
            .iter()
            .find(|d| d.label == text)
            .cloned()
    }

    fn set_actions_enabled(&self, mounted: bool) {
        set_enabled(self.btn_umount.get(), mounted);
        set_enabled(self.btn_create.get(), mounted);
        set_enabled(self.btn_restore.get(), mounted);
    }

    /// (Re)remplit la liste des disques BTRFS non montés.
    fn scan_disks(&self) {
        let all = self.run_core(list_disks);
        let disks = free_disks(&all);
        for d in all.iter().filter(|d| d.is_mounted()) {
            self.log_line(&format!(
                "⚠ {} est déjà monté sur {} — démontez-le avant de l'utiliser ici.",
                d.path,
                d.mountpoints.join(", ")
            ));
        }
        {
            let text = combo_text(self.p_dev);
            let combo = self.p_dev;
            if !combo.is_null() {
                let_cxx_string!(empty = "");
                unsafe {
                    ffi::QComboBox_clear(combo);
                    for d in &disks {
                        let_cxx_string!(c = d.label.as_str());
                        ffi::QComboBox_addItem(combo, &c);
                    }
                    // Conserver la sélection si le disque est toujours présent.
                    if let Some(idx) = disks.iter().position(|d| d.label == text) {
                        ffi::QComboBox_setCurrentIndex(combo, idx as i32);
                    }
                    let _ = &empty;
                }
            }
        }
        self.log_line(&format!("{} disque(s) BTRFS non monté(s).", disks.len()));
        if disks.is_empty() {
            self.log_line(&t(
                "Aucun disque BTRFS non monté. Vérifiez le branchement et relancez la recherche.",
            ));
        }
        *self.disks.borrow_mut() = disks;
    }

    /// Sonde le disque sélectionné : sous-volumes, défaut, dossiers d'instantanés.
    fn probe_selected(&self) {
        let Some(disk) = self.selected_disk() else {
            self.log_line(&t("Sélectionnez un disque."));
            return;
        };
        let dev = disk.path.clone();
        let probe = self.run_core(move || probe_subvols(&dev));
        match probe {
            Ok(p) => {
                let combo = self.p_subvol;
                if !combo.is_null() {
                    unsafe {
                        ffi::QComboBox_clear(combo);
                        // Le sous-volume par défaut en premier : il est
                        // présélectionné, et c'est presque toujours celui qui
                        // porte le système.
                        let mut items = p.subvols.clone();
                        if !p.default.is_empty() {
                            items.retain(|s| s != &p.default);
                            items.insert(0, p.default.clone());
                        }
                        for s in &items {
                            let_cxx_string!(c = s.as_str());
                            ffi::QComboBox_addItem(combo, &c);
                        }
                    }
                }
                let sd = choose_snapdir(|c| p.snapdirs.iter().any(|s| s == c));
                let sd = if sd.is_empty() {
                    DEFAULT_SNAPDIR.to_string()
                } else {
                    sd
                };
                if !self.p_snapdir.is_null() {
                    let_cxx_string!(c = sd.as_str());
                    unsafe { ffi::QLineEdit_setText(self.p_snapdir, &c) };
                }
                *self.snapdir.borrow_mut() = sd.clone();
                self.log_line(&format!(
                    "Sous-volumes : {}\nDéfaut : {}\nDossier d'instantanés : {}",
                    if p.subvols.is_empty() {
                        "(aucun)".to_string()
                    } else {
                        p.subvols.join(", ")
                    },
                    if p.default.is_empty() {
                        "?"
                    } else {
                        &p.default
                    },
                    sd
                ));
                label_set(
                    self.l_state,
                    &format!("Analysé : {} — non monté", disk.path),
                );
            }
            Err(e) => {
                self.log_line(&format!("✗ {e}"));
                label_set(self.l_state, &t("Échec de l'analyse."));
            }
        }
    }

    fn mount_selected(&self) {
        if self.mount.borrow().is_some() {
            return;
        }
        let Some(disk) = self.selected_disk() else {
            self.log_line(&t("Sélectionnez un disque."));
            return;
        };
        let subvol = combo_text(self.p_subvol);
        let snapdir = line_text(self.p_snapdir);
        if !valid_subvol(&subvol) {
            self.log_line(&t("Sélectionnez un sous-volume système."));
            return;
        }
        if !valid_snapdir(&snapdir) {
            self.log_line(&t(
                "Dossier d'instantanés invalide (chemin relatif attendu, ex. snapshots).",
            ));
            return;
        }
        let (dev, sv, sd) = (
            disk.path.clone(),
            subvol.clone(),
            snapdir.trim().to_string(),
        );
        let sd_mount = sd.clone();
        let mounted = self.run_core(move || mount_target(&dev, &sv, &sd_mount));
        match mounted {
            Ok(m) => {
                self.log_line(&format!("✓ {}[{}] monté sur {}", m.dev, m.subvol, m.path));
                label_set(
                    self.l_state,
                    &format!("Monté : {}[{}] → {}", m.dev, m.subvol, m.path),
                );
                *self.snapdir.borrow_mut() = sd;
                *self.mount.borrow_mut() = Some(m);
                self.set_actions_enabled(true);
                self.refresh_snapshots();
                self.set_status(&t("Prêt."));
            }
            Err(e) => {
                self.log_line(&format!("✗ {e}"));
                self.set_status(&t("Montage impossible."));
            }
        }
    }

    fn unmount(&self) {
        let mut slot = self.mount.borrow_mut();
        let Some(m) = slot.as_mut() else {
            return;
        };
        let desc = format!("{}[{}]", m.dev, m.subvol);
        let err = m.unmount();
        if err.is_empty() {
            self.log_line(&format!("✓ {desc} démonté."));
        } else {
            self.log_line(&format!("✗ démontage impossible : {err}"));
            return;
        }
        *slot = None;
        drop(slot);
        label_set(self.l_state, &t("Non monté."));
        self.table.borrow_mut().set_row_count(0);
        self.rows.borrow_mut().clear();
        self.set_actions_enabled(false);
    }

    fn refresh_snapshots(&self) {
        let slot = self.mount.borrow();
        let Some(m) = slot.as_ref() else {
            return;
        };
        let (root, sd) = (m.path.clone(), self.snapdir.borrow().clone());
        let sd_list = sd.clone();
        let snaps = self.run_core(move || target_snapshots(&root, &sd_list));
        {
            let table = self.table.borrow_mut();
            table.set_row_count(0);
            table.set_row_count(snaps.len() as i32);
            for (i, s) in snaps.iter().enumerate() {
                table.set_item(i as i32, 0, &s.path);
                table.set_item(i as i32, 1, &s.generation.to_string());
                // Chemin relatif au sous-volume : `s.full` suppose le dossier
                // d'instantanés visible depuis la racine, ce qui n'est vrai
                // qu'après le montage secondaire (ou sur un système démarré).
                table.set_item(i as i32, 2, &join_path(&sd, &s.path));
            }
        }
        if snaps.is_empty() {
            self.log_line(&t("Aucun instantané sur ce disque."));
        }
        *self.rows.borrow_mut() = snaps;
    }

    fn on_create(&self) {
        let mounted = self.mount.borrow().is_some();
        if !mounted {
            return;
        }
        let name = get_text(
            None,
            &t("Créer un instantané"),
            &t("Nom du snapshot"),
            &now_stamp(),
        );
        let name = name.trim().replace(' ', "_");
        if name.is_empty() {
            return;
        }
        let (root, sd) = {
            let slot = self.mount.borrow();
            let m = slot.as_ref().unwrap();
            (m.path.clone(), self.snapdir.borrow().clone())
        };
        if !question(
            &t("Confirmer la création"),
            &format!(
                "{} :\n\n{}\n\n{}",
                t("Disque cible"),
                root,
                format!("{} « {name} »", t("Créer l'instantané"))
            ),
        ) {
            self.log_line(&t("Annulé."));
            return;
        }
        let n = name.clone();
        let created = self.run_core(move || create_snapshot(&root, &sd, Some(&n)));
        if created.is_empty() {
            self.log_line(&t("Échec de la création du snapshot."));
        } else {
            self.log_line(&format!("✓ Snapshot créé : {name}"));
        }
        self.refresh_snapshots();
    }

    fn on_restore(&self) {
        let rows = self.table.borrow().selected_rows();
        if rows.is_empty() {
            self.log_line(&t("Sélectionnez un instantané dans la liste."));
            return;
        }
        let (name, sd, root, at, dev) = {
            let snaps = self.rows.borrow();
            let Some(s) = snaps.get(rows[0] as usize) else {
                return;
            };
            let slot = self.mount.borrow();
            let Some(m) = slot.as_ref() else {
                return;
            };
            (
                s.path.clone(),
                self.snapdir.borrow().clone(),
                m.path.clone(),
                m.subvol.clone(),
                m.dev.clone(),
            )
        };
        let text = format!(
            "{}\n\n{}\n\n{}\n\n{}",
            format!("{} : {dev}", t("Disque cible")),
            format!(
                "{} « {name} » → {at}",
                t("Le snapshot prendra la place du sous-volume")
            ),
            t("L'ancien sous-volume est conservé dans le dossier des instantanés. Rien n'est supprimé."),
            t("Le sous-volume par défaut du disque cible sera repointé. Redémarrez la machine cible — pas la session live — pour booter dessus."),
        );
        if !question(&t("Restaurer cet instantané ?"), &text) {
            self.log_line(&t("Annulé."));
            return;
        }
        let n = name.clone();
        let rc = self.run_core(move || restore_snapshot(&root, &sd, &n));
        if rc == 0 {
            self.log_line(&format!(
                "✓ Restauration terminée : {name} est désormais {at} sur {dev}."
            ));
            self.log_line(&t(
                "Redémarrez la machine cible pour booter sur la version restaurée.",
            ));
            // Le sous-volume monté a été renommé puis remplacé : le point de
            // montage pointe encore sur l'ancien, on le libère plutôt que de
            // risquer une création au mauvais endroit.
            self.unmount();
            self.log_line(&t(
                "Disque démonté. Relancez « Analyser » puis « Monter » pour recharger la nouvelle racine.",
            ));
        } else {
            self.log_line(&format!("✗ Échec de la restauration de {name}."));
        }
        self.refresh_snapshots();
    }
}

struct RescueWindow {
    window: MainWindow,
    ui: Rc<RescueUi>,
    // Les layouts doivent survivre : un QLayout droppé supprime la géométrie
    // de ses enfants (cf. build_ui de main.rs).
    _layouts: Vec<Box<dyn std::any::Any>>,
}

thread_local! {
    /// Fenêtre de dépannage, allouée pour toute la vie du processus.
    ///
    /// Volontairement fuitée (`Box::leak`) : les destructeurs de `thread_local`
    /// sont exécutés par libc à la sortie du processus, APRÈS le début du
    /// démontage Qt ; le `Drop` des wrappers qtrs supprimerait alors des
    /// objets C++ déjà libérés (SIGSEGV à la fermeture). Qt reste propriétaire
    /// des widgets C++ : on ne rend jamais la main au GC Rust.
    static RESCUE_WIN: RefCell<Option<&'static mut RescueWindow>> = const { RefCell::new(None) };
}

fn build_window() -> RescueWindow {
    let window = MainWindow::new()
        .window_title(t("Dépannage — instantanés sur disque externe"))
        .size(960, 640)
        .build();
    let mut layouts: Vec<Box<dyn std::any::Any>> = Vec::new();
    let mut central = Widget::new().build();
    central.set_has_parent();
    let mut root_l = VBoxLayout::with_parent(&central);
    root_l.set_spacing(8);
    root_l.set_contents_margins(10, 8, 10, 8);

    let header = Widget::new().parent(&central).build();

    // --- widgets à état -----------------------------------------------------
    let log = Rc::new(RefCell::new(PlainTextEdit::new().build()));
    log.borrow_mut().set_read_only(true);
    let table = Rc::new(RefCell::new(TableWidget::new(0, 3).build()));
    table.borrow_mut().set_has_parent();
    let hdrs = [t("Nom"), t("Gen"), t("Chemin complet")];
    let hdrs: Vec<&str> = hdrs.iter().map(|s| s.as_str()).collect();
    table.borrow_mut().set_horizontal_header_labels(&hdrs);
    table.borrow_mut().set_selection_mode(1); // SingleSelection
    table.borrow_mut().set_selection_behavior(1); // SelectRows
    table.borrow_mut().set_column_width(0, 240);
    table.borrow_mut().set_column_width(1, 70);
    table.borrow_mut().set_column_width(2, 560);

    // --- groupe « disque cible » --------------------------------------------
    let disk_box = GroupBox::new(t("Disque cible")).parent(&central).build();
    let mut grid = GridLayout::with_parent(&disk_box);
    let disk_lbl = Label::new(t("Disque BTRFS")).parent(&disk_box).build();
    grid.add(disk_lbl, 0, 0, 1, 1);
    let disk_row = Widget::new().parent(&disk_box).build();
    let mut disk_hl = HBoxLayout::with_parent(&disk_row);
    disk_hl.set_spacing(8);
    let dev_cb = ComboBox::new().parent(&disk_box).build();
    let p_dev = dev_cb.widget_ptr() as *mut ffi::QComboBox;
    disk_hl.add(dev_cb);
    disk_hl.add_spacer(Spacer::horizontal_expanding());
    grid.add(disk_row, 0, 1, 1, 1);

    let sv_lbl = Label::new(t("Sous-volume système"))
        .parent(&disk_box)
        .build();
    grid.add(sv_lbl, 1, 0, 1, 1);
    let sv_row = Widget::new().parent(&disk_box).build();
    let mut sv_hl = HBoxLayout::with_parent(&sv_row);
    sv_hl.set_spacing(8);
    let sv_cb = ComboBox::new().parent(&disk_box).build();
    let p_subvol = sv_cb.widget_ptr() as *mut ffi::QComboBox;
    sv_hl.add(sv_cb);
    sv_hl.add_spacer(Spacer::horizontal_expanding());
    grid.add(sv_row, 1, 1, 1, 1);

    let sd_lbl = Label::new(t("Dossier des instantanés (relatif)"))
        .parent(&disk_box)
        .build();
    grid.add(sd_lbl, 2, 0, 1, 1);
    let sd_edit = LineEdit::new(DEFAULT_SNAPDIR).parent(&disk_box).build();
    let p_snapdir = sd_edit.widget_ptr() as *mut ffi::QLineEdit;
    grid.add(sd_edit, 2, 1, 1, 1);

    let state_lbl = Label::new(t("Non monté.")).parent(&disk_box).build();
    let l_state = state_lbl.widget_ptr() as *mut ffi::QLabel;
    grid.add(state_lbl, 3, 0, 1, 2);

    let action_row = Widget::new().parent(&disk_box).build();
    let mut action_hl = HBoxLayout::with_parent(&action_row);
    action_hl.set_spacing(8);
    grid.add(action_row, 4, 0, 1, 2);

    // --- instantanés --------------------------------------------------------
    let snaps_lbl = Label::new(t("Instantanés présents sur le disque cible"))
        .parent(&central)
        .build();
    let table_scroll = ScrollArea::new().parent(&central).build();
    table_scroll.set_widget_resizable(true);
    table_scroll.set_widget(&*table.borrow());
    let snaps_row = Widget::new().parent(&central).build();
    let mut snaps_hl = HBoxLayout::with_parent(&snaps_row);
    snaps_hl.set_spacing(8);

    let status_lbl = Label::new(t("Prêt.")).parent(&central).build();
    let l_status = status_lbl.widget_ptr() as *mut ffi::QLabel;

    // --- état (les boutons viennent après : leurs callbacks capturent un
    //     Weak<RescueUi>, qui n'existe qu'une fois l'état complet) ----------
    let ui = Rc::new(RescueUi {
        log: log.clone(),
        table: table.clone(),
        rows: RefCell::new(Vec::new()),
        disks: RefCell::new(Vec::new()),
        mount: RefCell::new(None),
        snapdir: RefCell::new(DEFAULT_SNAPDIR.to_string()),
        p_dev,
        p_subvol,
        p_snapdir,
        l_state,
        l_status,
        btn_umount: Cell::new(std::ptr::null_mut()),
        btn_create: Cell::new(std::ptr::null_mut()),
        btn_restore: Cell::new(std::ptr::null_mut()),
    });

    // --- en-tête ------------------------------------------------------------
    {
        let mut hl = HBoxLayout::with_parent(&header);
        hl.set_spacing(8);
        hl.add(
            Label::new(format!(
                "Btrfsmgr — mode dépannage (session live) v{VERSION}"
            ))
            .parent(&header)
            .build(),
        );
        hl.add_spacer(Spacer::horizontal_expanding());
        let win_ptr = window.widget_ptr() as *mut ffi::QWidget;
        hl.add(
            PushButton::new(t("Fermer"))
                .on_clicked(move || unsafe {
                    ffi::QWidget_hide(win_ptr);
                })
                .parent(&header)
                .build(),
        );
        layouts.push(Box::new(hl));
    }

    // --- boutons ------------------------------------------------------------
    {
        let w = Rc::downgrade(&ui);
        let scan_btn = PushButton::new(t("Rechercher les disques"))
            .on_clicked(move || {
                if let Some(u) = w.upgrade() {
                    u.scan_disks();
                }
            })
            .parent(&disk_box)
            .build();
        disk_hl.add(scan_btn);
    }
    {
        let w = Rc::downgrade(&ui);
        let probe_btn = PushButton::new(t("Analyser"))
            .on_clicked(move || {
                if let Some(u) = w.upgrade() {
                    u.probe_selected();
                }
            })
            .parent(&disk_box)
            .build();
        sv_hl.add(probe_btn);
    }
    {
        let w = Rc::downgrade(&ui);
        let mount_btn = PushButton::new(t("Monter"))
            .on_clicked(move || {
                if let Some(u) = w.upgrade() {
                    u.mount_selected();
                }
            })
            .parent(&disk_box)
            .build();
        action_hl.add(mount_btn);

        let w = Rc::downgrade(&ui);
        let umount_btn = PushButton::new(t("Démonter"))
            .on_clicked(move || {
                if let Some(u) = w.upgrade() {
                    u.unmount();
                }
            })
            .parent(&disk_box)
            .build();
        ui.btn_umount
            .set(umount_btn.widget_ptr() as *mut ffi::QPushButton);
        action_hl.add(umount_btn);
        action_hl.add_spacer(Spacer::horizontal_expanding());
    }
    {
        let w = Rc::downgrade(&ui);
        let create_btn = PushButton::new(t("Créer un instantané"))
            .on_clicked(move || {
                if let Some(u) = w.upgrade() {
                    u.on_create();
                }
            })
            .parent(&central)
            .build();
        ui.btn_create
            .set(create_btn.widget_ptr() as *mut ffi::QPushButton);
        snaps_hl.add(create_btn);

        let w = Rc::downgrade(&ui);
        let restore_btn = PushButton::new(t("Restaurer la sélection"))
            .on_clicked(move || {
                if let Some(u) = w.upgrade() {
                    u.on_restore();
                }
            })
            .parent(&central)
            .build();
        ui.btn_restore
            .set(restore_btn.widget_ptr() as *mut ffi::QPushButton);
        snaps_hl.add(restore_btn);
        snaps_hl.add_spacer(Spacer::horizontal_expanding());
    }
    ui.set_actions_enabled(false);

    // --- assemblage ---------------------------------------------------------
    root_l.add(header);
    root_l.add(disk_box);
    root_l.add(snaps_lbl);
    root_l.add(table_scroll);
    root_l.add(snaps_row);
    root_l.add(Label::new(t("Journal")).parent(&central).build());
    let log_scroll = ScrollArea::new().parent(&central).build();
    log_scroll.set_widget_resizable(true);
    log_scroll.set_widget(&*ui.log.borrow());
    unsafe {
        ffi::QWidget_setMinimumSize(log_scroll.widget_ptr() as *mut ffi::QWidget, 0, 110);
    }
    root_l.add(log_scroll);
    root_l.add(status_lbl);

    layouts.push(Box::new(root_l));
    layouts.push(Box::new(grid));
    layouts.push(Box::new(disk_hl));
    layouts.push(Box::new(sv_hl));
    layouts.push(Box::new(action_hl));
    layouts.push(Box::new(snaps_hl));

    window.set_central_widget(&central);

    // Premier inventaire : la fenêtre s'ouvre déjà avec la liste des disques.
    ui.scan_disks();

    RescueWindow {
        window,
        ui,
        _layouts: layouts,
    }
}

/// Ouvre la fenêtre de dépannage (créée une seule fois, réaffichée ensuite).
pub fn open_window() {
    RESCUE_WIN.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(w) = slot.as_mut() {
            w.window.show();
            return;
        }
        let w: &'static mut RescueWindow = Box::leak(Box::new(build_window()));
        w.window.show();
        *slot = Some(w);
    });
}

/// Contrôle de géométrie pour le smoke test (même hook que la fenêtre
/// principale : `BTRFSMGR_GUI_ASSERT_GEOM=1`).
pub fn assert_geometry() {
    RESCUE_WIN.with(|slot| {
        let slot = slot.borrow();
        let Some(w) = slot.as_ref() else {
            eprintln!("GEOM_FAIL rescue: fenêtre absente");
            std::process::exit(1);
        };
        let (tw, th) = {
            let table = w.ui.table.borrow();
            let p = table.widget_ptr() as *mut ffi::QWidget;
            unsafe { (ffi::QWidget_width(p), ffi::QWidget_height(p)) }
        };
        let (ww, wh) = unsafe {
            (
                ffi::QWidget_width(w.window.widget_ptr() as *mut ffi::QWidget),
                ffi::QWidget_height(w.window.widget_ptr() as *mut ffi::QWidget),
            )
        };
        if tw < 200 || th < 80 || ww < 400 || wh < 300 {
            eprintln!("GEOM_FAIL rescue window={ww}x{wh} table={tw}x{th}");
            std::process::exit(1);
        }
        eprintln!("GEOM_OK rescue window={ww}x{wh} table={tw}x{th}");
        std::process::exit(0);
    });
}

// ---------------------------------------------------------------------------
// Tests unitaires (pas de root, pas de BTRFS requis)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const LSBLK: &str = r#"{
      "blockdevices": [
        {"name":"vda","path":"/dev/vda","type":"disk","fstype":null,"label":null,
         "size":53687091200,"mountpoint":null,"model":"QEMU HARDDISK",
         "children":[
           {"name":"vda1","path":"/dev/vda1","type":"part","fstype":"vfat",
            "label":"ESP","size":536870912,"mountpoint":null,"model":null},
           {"name":"vda2","path":"/dev/vda2","type":"part","fstype":"btrfs",
            "label":"Solus","size":42949672960,"mountpoint":null,"model":null}
         ]},
        {"name":"sdb","path":"/dev/sdb","type":"disk","fstype":"btrfs","label":"Backup",
         "size":1073741824,"mountpoint":"/run/media/julien/Backup","model":"USB Disk"},
        {"name":"loop0","path":"/dev/loop0","type":"loop","fstype":"squashfs",
         "label":null,"size":1048576,"mountpoint":"/","model":null}
      ]
    }"#;

    #[test]
    fn lsblk_reports_btrfs_with_mount_state() {
        let d = parse_lsblk(LSBLK);
        assert_eq!(d.len(), 2, "{d:?}");
        let solus = d.iter().find(|x| x.path == "/dev/vda2").unwrap();
        assert!(!solus.is_mounted());
        assert!(solus.label.contains("/dev/vda2"));
        assert!(solus.label.contains("40.0 Gio"));
        assert!(solus.label.contains("Solus"));
        let backup = d.iter().find(|x| x.path == "/dev/sdb").unwrap();
        assert!(backup.is_mounted());
        assert_eq!(
            backup.mountpoints,
            vec!["/run/media/julien/Backup".to_string()]
        );
    }

    #[test]
    fn free_disks_excludes_mounted_and_live_medium() {
        let all = parse_lsblk(LSBLK);
        // Ni la clé live (squashfs montée sur /), ni le disque déjà monté.
        assert!(all.iter().all(|x| x.path != "/dev/loop0"));
        let free = free_disks(&all);
        assert_eq!(free.len(), 1);
        assert_eq!(free[0].path, "/dev/vda2");
    }

    #[test]
    fn multiple_mountpoints_are_split() {
        let json = r#"{"blockdevices":[{"name":"sdc1","path":"/dev/sdc1",
            "fstype":"btrfs","size":1024,"mountpoint":"/mnt/a,/mnt/b",
            "label":"Two"}]}"#;
        let d = parse_lsblk(json);
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].mountpoints,
            vec!["/mnt/a".to_string(), "/mnt/b".to_string()]
        );
        assert!(free_disks(&d).is_empty());
    }

    #[test]
    fn human_size_reads_naturally() {
        assert_eq!(human_size(0), "0 o");
        assert_eq!(human_size(1023), "1023 o");
        assert_eq!(human_size(1024), "1.0 Kio");
        assert_eq!(human_size(42949672960), "40.0 Gio");
    }

    #[test]
    fn subvol_names_are_restricted() {
        assert!(valid_subvol("@"));
        assert!(valid_subvol("@rootfs"));
        assert!(valid_subvol("  @home  "));
        assert!(!valid_subvol(""));
        assert!(!valid_subvol("."));
        assert!(!valid_subvol(".."));
        assert!(!valid_subvol("/@"));
        assert!(!valid_subvol("@/home"));
        assert!(!valid_subvol("@ home"));
        assert!(!valid_subvol("@,ro"));
    }

    #[test]
    fn snapdir_must_stay_relative() {
        assert!(valid_snapdir("snapshots"));
        assert!(valid_snapdir(".snapshots"));
        assert!(!valid_snapdir(""));
        assert!(!valid_snapdir("/snapshots"));
        assert!(!valid_snapdir("../snapshots"));
        assert!(!valid_snapdir("snap shots"));
        assert!(!valid_snapdir("snap//shots"));
    }

    #[test]
    fn device_must_be_a_dev_path() {
        assert!(valid_device("/dev/sdb2"));
        assert!(!valid_device("sdb2"));
        assert!(!valid_device("/dev/ sdb2"));
        assert!(!valid_device(""));
    }

    #[test]
    fn mount_option_is_stable() {
        assert_eq!(mount_options("@"), "subvol=@,rw");
        assert_eq!(mount_options("  @home "), "subvol=@home,rw");
    }

    #[test]
    fn only_top_level_subvols_are_offered() {
        let p = vec![
            "/@".to_string(),
            "snapshots/20260101".to_string(),
            "@home".to_string(),
            "@".to_string(),
            "snapshots".to_string(),
        ];
        assert_eq!(
            top_level_subvols(p.into_iter()),
            vec![
                "@".to_string(),
                "@home".to_string(),
                "snapshots".to_string()
            ]
        );
    }

    #[test]
    fn snapdir_detection_prefers_conventional_names() {
        assert_eq!(choose_snapdir(|c| c == "snapshots"), "snapshots");
        assert_eq!(choose_snapdir(|c| c == "@snapshots"), "@snapshots");
        assert_eq!(
            choose_snapdir(|c| c == "snapshots" || c == ".snapshots"),
            "snapshots"
        );
        assert_eq!(choose_snapdir(|_| false), "");
    }
}
