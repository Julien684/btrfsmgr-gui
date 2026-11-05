//! btrfsmgr-gui — interface graphique Qt6 (qtrs), binaire unique.
//!
//! Portage Rust de l'interface GTK4. Quatre onglets : Général, Snapshots,
//! Automatisation, Boot ; journal + statut en bas.
//!
//! Modèle qtrs : les widgets « à état » (tableaux, journal) sont gardés en
//! `Rc<RefCell<..>>` et placés dans des `ScrollArea` ; les champs de formulaire
//! sont ajoutés aux layouts et manipulés par leur pointeur C++ (`qtrs::ffi`).
//! Les callbacks capturent un `Weak<Ui>` (jamais un Rc) pour ne pas prolonger
//! la vie des wrappers après la destruction des objets Qt.

use std::cell::RefCell;
use std::rc::Rc;

use cxx::let_cxx_string;
use qtrs::ffi;
use qtrs::prelude::*;
use qtrs::{
    get_int, get_text, Application, CheckBox, ComboBox, GridLayout, GroupBox, HBoxLayout, Label,
    LineEdit, MainWindow, MessageBox, PlainTextEdit, PushButton, ScrollArea, SpinBox, TabWidget,
    TableWidget, VBoxLayout, Widget, INFORMATION, CANCEL, NO, YES,
};

mod core;
mod i18n;

use core::*;
use i18n::{t, tf};

// ---------------------------------------------------------------------------
// Helpers UI
// ---------------------------------------------------------------------------

struct Ui {
    root: String,
    snapdir: String,
    log: Rc<RefCell<PlainTextEdit>>,
    snap_table: Rc<RefCell<TableWidget>>,
    snap_rows: RefCell<Vec<Snapshot>>,
    timer_table: Rc<RefCell<TableWidget>>,
    timer_rows: RefCell<Vec<core::Timer>>,
    boot_table: Rc<RefCell<TableWidget>>,
    // libellés (pointeurs C++ : les widgets vivent dans l'arbre Qt)
    l_version: *mut ffi::QLabel,
    l_boot: *mut ffi::QLabel,
    l_default: *mut ffi::QLabel,
    l_snapcount: *mut ffi::QLabel,
    l_timercount: *mut ffi::QLabel,
    l_disk: *mut ffi::QLabel,
    l_status: *mut ffi::QLabel,
    l_bootpage: *mut ffi::QLabel,
    // champs du formulaire d'automatisation
    p_tag: *mut ffi::QLineEdit,
    p_freq: *mut ffi::QComboBox,
    p_day: *mut ffi::QComboBox,
    p_daynum: *mut ffi::QSpinBox,
    p_hour: *mut ffi::QSpinBox,
    p_boot: *mut ffi::QCheckBox,
    p_delay: *mut ffi::QSpinBox,
    p_once: *mut ffi::QCheckBox,
    p_keep: *mut ffi::QSpinBox,
    p_timeout: *mut ffi::QSpinBox,
}

fn label_set(p: *mut ffi::QLabel, text: &str) {
    if p.is_null() {
        return;
    }
    let_cxx_string!(s = text);
    unsafe { ffi::QLabel_setText(p, &s) };
}

fn line(p: *mut ffi::QLineEdit) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { ffi::QLineEdit_text(p) }
}
fn combo(p: *mut ffi::QComboBox) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { ffi::QComboBox_currentText(p) }
}
fn spin(p: *mut ffi::QSpinBox) -> i32 {
    if p.is_null() {
        return 0;
    }
    unsafe { ffi::QSpinBox_value(p) }
}
fn checked(p: *mut ffi::QCheckBox) -> bool {
    if p.is_null() {
        return false;
    }
    unsafe { ffi::QCheckBox_isChecked(p) }
}

/// Confirmation 3 choix : adapter / ne pas adapter / annuler.
fn boot_action(action: &str, body: &str) -> Option<&'static str> {
    let mut text = format!("Opération : {action}");
    if !body.trim().is_empty() {
        text.push_str("\n\n");
        text.push_str(body.trim_end());
    }
    let msg = MessageBox::new()
        .icon(INFORMATION)
        .window_title("Adapter les entrées de démarrage systemd-boot ?")
        .text(text.clone())
        .informative_text(
            "Oui = adapter systemd-boot · Non = ne pas adapter · Annuler = ne rien faire",
        )
        .standard_buttons(YES | NO | CANCEL)
        .default_button(YES)
        .build();
    match msg.exec() {
        YES => Some("adapt"),
        NO => Some("skip"),
        _ => None,
    }
}

fn info(title: &str, text: &str) {
    qtrs::information(None, title, text);
}

fn question(title: &str, text: &str) -> bool {
    let msg = MessageBox::new()
        .icon(INFORMATION)
        .window_title(title)
        .text(text)
        .standard_buttons(YES | CANCEL)
        .default_button(YES)
        .build();
    msg.exec() == YES
}

impl Ui {
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

    // ------------------------------------------------------------- refresh

    fn refresh_info(&self) {
        let bl = detect_bootloader();
        let dv = self.run_core(|| default_subvol("/"));
        let snaps = self.run_core(|| list_snapshots(&self.root, &self.snapdir));
        let timers = self.run_core(list_timers);
        label_set(self.l_version, &format!("{APP} {VERSION}"));
        label_set(self.l_boot, &bootloader_display(bl));
        label_set(
            self.l_default,
            if dv.full.is_empty() { "?" } else { &dv.full },
        );
        label_set(self.l_snapcount, &snaps.len().to_string());
        label_set(self.l_timercount, &timers.len().to_string());
        let (_rc, df) = runv(&sargs(&["df", "-h", &self.root]), false);
        label_set(self.l_disk, df.trim());
    }

    fn refresh_snapshots(&self) {
        let snaps = self.run_core(|| {
            let mut v = list_snapshots(&self.root, &self.snapdir);
            v.reverse(); // plus récent en premier (comme le TUI / la GUI GTK)
            v
        });
        {
            let table = self.snap_table.borrow_mut();
            table.set_row_count(0);
            table.set_row_count(snaps.len() as i32);
            for (i, s) in snaps.iter().enumerate() {
                table.set_item(i as i32, 0, &s.path);
                table.set_item(i as i32, 1, &s.generation.to_string());
                table.set_item(i as i32, 2, &s.full);
            }
        }
        if snaps.is_empty() {
            self.log_line(&t("Aucun snapshot."));
        }
        *self.snap_rows.borrow_mut() = snaps;
    }

    fn refresh_timers(&self) {
        let timers = self.run_core(list_timers);
        {
            let table = self.timer_table.borrow_mut();
            table.set_row_count(0);
            table.set_row_count(timers.len() as i32);
            for (i, t) in timers.iter().enumerate() {
                table.set_item(i as i32, 0, &t.timer);
                table.set_item(i as i32, 1, &t.next);
                table.set_item(i as i32, 2, &t.last);
                table.set_item(i as i32, 3, &t.activated);
            }
        }
        *self.timer_rows.borrow_mut() = timers;
    }

    fn refresh_boot(&self) {
        // /boot n'est pas monté par défaut sur Solus : on le monte (comme la
        // GUI GTK) avant de détecter et de lister les entrées générées.
        let (bl, entries) = self.run_core(|| {
            ensure_boot_mounted();
            (detect_bootloader(), boot_entries())
        });
        label_set(
            self.l_bootpage,
            &format!(
                "Chargeur détecté : {}\nEntrées systemd-boot gérées : {}",
                bootloader_display(bl),
                if bl == "systemd-boot" {
                    format!("{}", entries.len())
                } else {
                    "non (n/a)".to_string()
                }
            ),
        );
        let table = self.boot_table.borrow_mut();
        table.set_row_count(0);
        table.set_row_count(entries.len() as i32);
        for (i, e) in entries.iter().enumerate() {
            table.set_item(i as i32, 0, &e.file);
            table.set_item(i as i32, 1, &e.title);
            table.set_item(i as i32, 2, &e.subvol);
        }
    }

    fn refresh_all(&self) {
        self.set_status("Actualisation…");
        self.refresh_info();
        self.refresh_snapshots();
        self.refresh_timers();
        self.refresh_boot();
        self.set_status("Prêt.");
    }

    // ---------------------------------------------------------------- actions

    fn on_create(&self) {
        let name = get_text(
            None,
            &t("Créer un instantané"),
            &t("Nom du snapshot"),
            &now_stamp(),
        );
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let action = tf("Créer l'instantané « {name} »", &[("name", &name)]);
        let Some(boot) = boot_action(&action, "") else {
            self.log_line(&t("Annulé."));
            return;
        };
        let (root, sd) = (self.root.clone(), self.snapdir.clone());
        let n = name.clone();
        let target = self.run_core(move || create_snapshot(&root, &sd, Some(&n)));
        if target.is_empty() {
            self.log_line(&t("Échec de la création du snapshot."));
        } else {
            self.log_line(&format!("✓ Snapshot créé: {name}"));
        }
        if boot == "adapt" {
            let (root, sd) = (self.root.clone(), self.snapdir.clone());
            self.run_core(move || sync_bootloader_entries(&root, &sd, false));
        }
        self.refresh_snapshots();
        self.refresh_info();
    }

    fn on_restore(&self) {
        let rows = self.snap_table.borrow().selected_rows();
        if rows.is_empty() {
            info(
                &t("Rien à restaurer"),
                &t("Sélectionnez un snapshot dans la liste."),
            );
            return;
        }
        if rows.len() > 1 {
            info(
                &t("Un seul à la fois"),
                &t("La restauration concerne un seul snapshot à la fois."),
            );
            return;
        }
        let (name, full) = {
            let snaps = self.snap_rows.borrow();
            match snaps.get(rows[0] as usize) {
                Some(s) => (s.path.clone(), s.full.clone()),
                None => return,
            }
        };
        let _ = full;
        let at = boot_subvol(&self.root).unwrap_or_else(|| t("(racine du FS)"));
        let mut body = format!(
            "Le snapshot prendra la place de « {at} » par échange CoW (deux mv).\n\
L'ancien sous-volume sera conservé dans {}/.\n\n\
Redémarrez ensuite pour booter sur la version restaurée.\n\n\
Cette opération est destructive.",
            self.snapdir
        );
        let (root, sd, n) = (self.root.clone(), self.snapdir.clone(), name.clone());
        let kinfo = self.run_core(move || snapshot_kernel_info(&root, &sd, &n));
        if kinfo.mismatch {
            let ks = if kinfo.snapshot_kernels.is_empty() {
                "?".to_string()
            } else {
                kinfo.snapshot_kernels.join(", ")
            };
            body = format!(
                "{}\n\n{body}",
                tf(
                    KERNEL_MISMATCH_WARNING,
                    &[("cur", &kinfo.current), ("snaps", &ks)]
                )
            );
        }
        let action = tf("Restaurer « {name} »", &[("name", &name)]);
        let Some(boot) = boot_action(&action, &body) else {
            self.log_line(&t("Annulé."));
            return;
        };
        let (root, sd, n) = (self.root.clone(), self.snapdir.clone(), name.clone());
        let rc = self.run_core(move || restore_snapshot(&root, &sd, &n));
        if boot == "adapt" {
            let (root, sd) = (self.root.clone(), self.snapdir.clone());
            self.run_core(move || sync_bootloader_entries(&root, &sd, false));
        }
        if rc == 0 {
            self.log_line(&format!(
                "✓ Restauration terminée: {name} est maintenant le sous-volume principal."
            ));
            if question(
                &t("Redémarrer maintenant ?"),
                &t("Voulez-vous redémarrer immédiatement pour booter sur la version restaurée ?"),
            ) {
                std::process::Command::new("systemctl")
                    .arg("reboot")
                    .spawn()
                    .ok();
            }
        } else {
            self.log_line(&format!("✗ Échec de la restauration de {name}."));
        }
        self.refresh_snapshots();
        self.refresh_info();
    }

    fn on_delete(&self) {
        let rows = self.snap_table.borrow().selected_rows();
        if rows.is_empty() {
            info(
                &t("Rien à supprimer"),
                &t("Sélectionnez un ou plusieurs snapshots dans la liste."),
            );
            return;
        }
        let targets: Vec<Snapshot> = {
            let snaps = self.snap_rows.borrow();
            rows.iter()
                .filter_map(|&r| snaps.get(r as usize).cloned())
                .collect()
        };
        let names = targets
            .iter()
            .map(|s| s.path.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let body = format!("{} :\n\n{names}", t("Suppression DÉFINITIVE (sous-volumes)"));
        let action = tf(
            "Supprimer {n} snapshot(s)",
            &[("n", &targets.len().to_string())],
        );
        let Some(boot) = boot_action(&action, &body) else {
            self.log_line(&t("Annulé."));
            return;
        };
        let mut ok = 0;
        for s in &targets {
            let full = s.full.clone();
            let (rc, out) = self.run_core(move || delete_subvol(&full));
            if rc == 0 {
                ok += 1;
            } else {
                self.log_line(&format!("✗ {} — {}", s.full, out.trim()));
            }
        }
        self.log_line(&format!("{ok}/{} supprimé(s).", targets.len()));
        if boot == "adapt" {
            let (root, sd) = (self.root.clone(), self.snapdir.clone());
            self.run_core(move || sync_bootloader_entries(&root, &sd, false));
        }
        self.refresh_snapshots();
        self.refresh_info();
    }

    fn on_retention(&self) {
        let keep = get_int(
            None,
            &t("Rétention manuelle"),
            &t("Conserver les N plus récents (0 = tous)"),
            7,
            0,
            999,
            1,
        );
        let (root, sd) = (self.root.clone(), self.snapdir.clone());
        let snaps = self.run_core(move || list_snapshots(&root, &sd));
        let to_del = if keep <= 0 || snaps.len() as i64 <= keep as i64 {
            0
        } else {
            snaps.len() as i64 - keep as i64
        };
        if to_del <= 0 {
            info(
                &t("Rien à supprimer"),
                &t("Aucun snapshot à supprimer (déjà ≤ N)."),
            );
            return;
        }
        let action = tf("Rétention : garder {keep}", &[("keep", &keep.to_string())]);
        let body = format!(
            "Garder les {keep} plus récents, supprimer les autres.\n{}",
            tf("Supprimer {n} snapshot(s)", &[("n", &to_del.to_string())])
        );
        let Some(boot) = boot_action(&action, &body) else {
            self.log_line(&t("Annulé."));
            return;
        };
        let (root, sd) = (self.root.clone(), self.snapdir.clone());
        self.run_core(move || prune_snapshots(&root, &sd, keep as i64));
        if boot == "adapt" {
            let (root, sd) = (self.root.clone(), self.snapdir.clone());
            self.run_core(move || sync_bootloader_entries(&root, &sd, false));
        }
        self.refresh_snapshots();
        self.refresh_info();
    }

    fn on_schedule_create(&self) {
        let tag = {
            let v = line(self.p_tag);
            if v.trim().is_empty() {
                "daily".to_string()
            } else {
                v.trim().to_string()
            }
        };
        let freq = combo(self.p_freq);
        let boot = checked(self.p_boot);
        let boot_delay = spin(self.p_delay) as i64;
        let once_per_day = if boot { checked(self.p_once) } else { false };
        let keep = spin(self.p_keep) as i64;
        let hour = spin(self.p_hour);

        let schedule = if !boot {
            if freq.starts_with("Chaque jour") || freq == "Every day" {
                format!("*-*-* {hour:02}:00:00")
            } else if freq.starts_with("Chaque semaine") || freq == "Every week" {
                let day = combo(self.p_day);
                let day = if day.trim().is_empty() {
                    "Mon".to_string()
                } else {
                    day.trim().to_string()
                };
                format!("{day} *-*-* {hour:02}:00:00")
            } else {
                let daynum = spin(self.p_daynum);
                format!("*-*-{daynum} {hour:02}:00:00")
            }
        } else {
            String::new()
        };

        let plan = if boot {
            format!("Au démarrage ({boot_delay} min après le boot)")
        } else {
            format!("OnCalendar={schedule}")
        };
        let mut plan = plan;
        if boot && once_per_day {
            plan.push_str(" (un seul par jour)");
        }
        let text = format!(
            "Planning : {plan}\nRétention: {keep} snapshot(s)\nCible    : {}\n\nCréer le timer systemd '{tag}' ?",
            join_path(&self.root, &self.snapdir)
        );
        if !question(&tf("Créer le timer systemd '{tag}' ?", &[("tag", &tag)]), &text) {
            return;
        }
        let (root, sd, tag2) = (self.root.clone(), self.snapdir.clone(), tag.clone());
        self.run_core(move || {
            write_timer_files(
                &root,
                &sd,
                &tag2,
                &schedule,
                keep,
                boot,
                boot_delay,
                once_per_day,
            )
        });
        // Une automatisation créée doit aussi ajouter les entrées de boot des
        // instantanés déjà présents (sinon la liste reste vide jusqu'au
        // premier déclenchement du timer).
        let (root, sd) = (self.root.clone(), self.snapdir.clone());
        self.run_core(move || sync_bootloader_entries(&root, &sd, true));
        self.log_line(&format!("✓ Timer '{tag}' créé et activé."));
        self.refresh_timers();
        self.refresh_boot();
        self.refresh_info();
    }

    fn on_timer_delete(&self) {
        let rows = self.timer_table.borrow().selected_rows();
        let Some(&row) = rows.first() else {
            info(
                &t("Rien à supprimer"),
                &t("Sélectionnez une automatisation dans la liste."),
            );
            return;
        };
        let timer = match self.timer_rows.borrow().get(row as usize) {
            Some(t) => t.timer.clone(),
            None => return,
        };
        let tag = timer_tag(&timer, &self.root, &self.snapdir);
        if !question(
            &tf("Supprimer l'automatisation '{tag}' ?", &[("tag", &tag)]),
            &t("Le timer sera désactivé et ses fichiers supprimés."),
        ) {
            return;
        }
        let (root, sd) = (self.root.clone(), self.snapdir.clone());
        self.run_core(move || delete_timer_files(&root, &sd, &tag));
        self.refresh_timers();
        self.refresh_info();
    }

    fn on_boot_sync(&self) {
        let (root, sd) = (self.root.clone(), self.snapdir.clone());
        self.run_core(move || sync_bootloader_entries(&root, &sd, true));
        self.refresh_boot();
    }

    fn on_boot_timeout(&self) {
        let secs = spin(self.p_timeout);
        if secs < 0 {
            return;
        }
        if !question(
            &tf(
                "Appliquer: clr-boot-manager set-timeout {secs}",
                &[("secs", &secs.to_string())],
            ),
            "0 = masquer le menu au démarrage, N > 0 = l'afficher N secondes.",
        ) {
            return;
        }
        let (rc, out) = runv(
            &sargs(&["clr-boot-manager", "set-timeout", &secs.to_string()]),
            false,
        );
        if rc == 0 {
            self.log_line(&format!("✓ Done (timeout={secs}s)."));
        } else {
            self.log_line(&format!("✗ Échec: {}", out.trim()));
        }
    }
}

// ---------------------------------------------------------------------------
// Construction de l'interface
// ---------------------------------------------------------------------------

fn build_ui(root: String, snapdir: String) -> (MainWindow, Rc<Ui>, Vec<Box<dyn std::any::Any>>) {
    let window = MainWindow::new()
        .window_title("Btrfsmgr — Gestionnaire BTRFS")
        .size(980, 660)
        .build();
    // Les layouts doivent survivre à build_ui : un VBoxLayout droppé supprime
    // le QLayout C++ et laisse ses enfants sans géométrie (widgets empilés à
    // 0,0 avec leur sizeHint).
    let mut layouts: Vec<Box<dyn std::any::Any>> = Vec::new();
    let mut central = Widget::new().build();
    // `set_central_widget` ne transfère PAS l'ownership côté Rust : sans ce
    // drapeau, le wrapper détruit le QWidget central à sa sortie de portée et
    // la fenêtre garde un pointeur mort (SIGSEGV au premier rendu).
    central.set_has_parent();
    let mut root_l = VBoxLayout::with_parent(&central);
    root_l.set_spacing(8);
    root_l.set_contents_margins(10, 8, 10, 8);

    // --- en-tête (contenu ajouté après la création de `Ui`) ----------------
    let header = Widget::new().parent(&central).build();

    // --- widgets à état -----------------------------------------------------
    let log = Rc::new(RefCell::new(PlainTextEdit::new().build()));
    log.borrow_mut().set_read_only(true);
    let status_lbl = Label::new(t("Prêt.")).parent(&central).build();
    let status_ptr = status_lbl.widget_ptr() as *mut ffi::QLabel;
    let snap_table = Rc::new(RefCell::new(TableWidget::new(0, 3).build()));
    snap_table.borrow_mut().set_has_parent();
    let snap_hdrs = [t("Nom"), t("Gen"), t("Chemin complet")];
    let snap_hdrs: Vec<&str> = snap_hdrs.iter().map(|s| s.as_str()).collect();
    snap_table
        .borrow_mut()
        .set_horizontal_header_labels(&snap_hdrs);
    snap_table.borrow_mut().set_selection_mode(3); // ExtendedSelection
    snap_table.borrow_mut().set_selection_behavior(1); // SelectRows
    snap_table.borrow_mut().set_column_width(0, 260);
    snap_table.borrow_mut().set_column_width(1, 70);
    snap_table.borrow_mut().set_column_width(2, 520);

    let timer_table = Rc::new(RefCell::new(TableWidget::new(0, 4).build()));
    timer_table.borrow_mut().set_has_parent();
    let timer_hdrs = [
        t("Unité"),
        t("Prochaine exécution"),
        t("Dernière"),
        t("Activée"),
    ];
    let timer_hdrs: Vec<&str> = timer_hdrs.iter().map(|s| s.as_str()).collect();
    timer_table
        .borrow_mut()
        .set_horizontal_header_labels(&timer_hdrs);
    timer_table.borrow_mut().set_selection_mode(1); // SingleSelection
    timer_table.borrow_mut().set_selection_behavior(1);
    timer_table.borrow_mut().set_column_width(0, 380);
    timer_table.borrow_mut().set_column_width(1, 170);
    timer_table.borrow_mut().set_column_width(2, 140);
    timer_table.borrow_mut().set_column_width(3, 140);

    let boot_table = Rc::new(RefCell::new(TableWidget::new(0, 3).build()));
    boot_table.borrow_mut().set_has_parent();
    let boot_hdrs = [t("Fichier"), t("Titre"), t("Sous-volume")];
    let boot_hdrs: Vec<&str> = boot_hdrs.iter().map(|s| s.as_str()).collect();
    boot_table
        .borrow_mut()
        .set_horizontal_header_labels(&boot_hdrs);
    boot_table.borrow_mut().set_selection_mode(1);
    boot_table.borrow_mut().set_selection_behavior(1);
    boot_table.borrow_mut().set_column_width(0, 320);
    boot_table.borrow_mut().set_column_width(1, 340);
    boot_table.borrow_mut().set_column_width(2, 220);

    // --- onglets -----------------------------------------------------------
    let mut tabs = TabWidget::new().parent(&central).build();
    let tabs_ptr = tabs.widget_ptr();

    // ===== Onglet Général ==================================================
    let page_info = Widget::new().parent(&central).build();
    let mut v_info = VBoxLayout::with_parent(&page_info);
    v_info.set_spacing(8);
    let info_box = GroupBox::new(t("Informations")).parent(&page_info).build();
    let mut g = GridLayout::with_parent(&info_box);
    let rows = [
        t("Version"),
        t("Chargeur de boot"),
        t("Subvolume par défaut"),
        t("Snapshots existants"),
        t("Timers actifs"),
        t("Espace disque"),
    ];
    let mut ptrs: Vec<*mut ffi::QLabel> = Vec::new();
    for (i, name) in rows.iter().enumerate() {
        let key = Label::new(name.clone()).parent(&info_box).build();
        g.add(key, i as i32, 0, 1, 1);
        let val = Label::new("").parent(&info_box).build();
        let p = val.widget_ptr() as *mut ffi::QLabel;
        ptrs.push(p);
        g.add(val, i as i32, 1, 1, 1);
    }
    v_info.add(info_box);
    v_info.add(
        Label::new(t(
            "Cliquez sur « Actualiser » ou changez d'onglet pour recharger ces informations.",
        ))
        .parent(&page_info)
        .build(),
    );

    // ===== Onglet Snapshots ================================================
    let page_snap = Widget::new().parent(&central).build();
    let mut v_snap = VBoxLayout::with_parent(&page_snap);
    v_snap.set_spacing(8);
    let snap_box = GroupBox::new(t("Snapshots")).parent(&page_snap).build();
    let mut v_snap_box = VBoxLayout::with_parent(&snap_box);
    v_snap_box.set_spacing(8);
    let snap_actions = Widget::new().parent(&snap_box).build();
    let mut snap_hl = HBoxLayout::with_parent(&snap_actions);
    snap_hl.set_spacing(8);
    v_snap_box.add(snap_actions);
    let snap_scroll = ScrollArea::new().parent(&snap_box).build();
    snap_scroll.set_widget_resizable(true);
    snap_scroll.set_widget(&*snap_table.borrow());
    v_snap_box.add(snap_scroll);
    v_snap.add(snap_box);
    v_snap.add(
        Label::new(t(
            "Astuce : sélectionnez plusieurs lignes (Ctrl/Shift) pour supprimer en une fois.",
        ))
        .parent(&page_snap)
        .build(),
    );

    // ===== Onglet Automatisation ===========================================
    let page_sched = Widget::new().parent(&central).build();
    let mut v_sched = VBoxLayout::with_parent(&page_sched);
    v_sched.set_spacing(8);
    let form_box = GroupBox::new(t("Créer une automatisation"))
        .parent(&page_sched)
        .build();
    let mut fg = GridLayout::with_parent(&form_box);
    let tag_lbl = Label::new(t("Nom du plan (ex. daily, weekly)"))
        .parent(&form_box)
        .build();
    fg.add(tag_lbl, 0, 0, 1, 1);
    let tag_edit = LineEdit::new("daily").parent(&form_box).build();
    let p_tag = tag_edit.widget_ptr() as *mut ffi::QLineEdit;
    fg.add(tag_edit, 0, 1, 1, 1);

    let freq_lbl = Label::new(t("Fréquence")).parent(&form_box).build();
    fg.add(freq_lbl, 1, 0, 1, 1);
    let freq_items = [
        t("Chaque jour"),
        t("Chaque semaine"),
        t("Chaque mois"),
    ];
    let freq_cb = ComboBox::new()
        .items(&[&freq_items[0], &freq_items[1], &freq_items[2]])
        .parent(&form_box)
        .build();
    let p_freq = freq_cb.widget_ptr() as *mut ffi::QComboBox;
    fg.add(freq_cb, 1, 1, 1, 1);

    let day_lbl = Label::new(t("Jour de la semaine (Mon..Sun)"))
        .parent(&form_box)
        .build();
    fg.add(day_lbl, 2, 0, 1, 1);
    let day_cb = ComboBox::new()
        .items(&["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"])
        .parent(&form_box)
        .build();
    let p_day = day_cb.widget_ptr() as *mut ffi::QComboBox;
    fg.add(day_cb, 2, 1, 1, 1);

    let daynum_lbl = Label::new(t("Jour du mois (1-31)"))
        .parent(&form_box)
        .build();
    fg.add(daynum_lbl, 3, 0, 1, 1);
    let daynum_sp = SpinBox::new()
        .range(1, 31)
        .value(1)
        .parent(&form_box)
        .build();
    let p_daynum = daynum_sp.widget_ptr() as *mut ffi::QSpinBox;
    fg.add(daynum_sp, 3, 1, 1, 1);

    let hour_lbl = Label::new(t("Heure (0-23)")).parent(&form_box).build();
    fg.add(hour_lbl, 4, 0, 1, 1);
    let hour_sp = SpinBox::new()
        .range(0, 23)
        .value(3)
        .parent(&form_box)
        .build();
    let p_hour = hour_sp.widget_ptr() as *mut ffi::QSpinBox;
    fg.add(hour_sp, 4, 1, 1, 1);

    let boot_chk = CheckBox::new(t(
        "Au démarrage de la machine (au lieu d'un horaire)",
    ))
    .parent(&form_box)
    .build();
    let p_boot = boot_chk.widget_ptr() as *mut ffi::QCheckBox;
    fg.add(boot_chk, 5, 0, 1, 2);

    let delay_lbl = Label::new(t("Minutes après le démarrage"))
        .parent(&form_box)
        .build();
    fg.add(delay_lbl, 6, 0, 1, 1);
    let delay_sp = SpinBox::new()
        .range(0, 1440)
        .value(5)
        .parent(&form_box)
        .build();
    let p_delay = delay_sp.widget_ptr() as *mut ffi::QSpinBox;
    fg.add(delay_sp, 6, 1, 1, 1);

    let once_chk = CheckBox::new(t(
        "Un seul instantané par jour (sinon un par démarrage)",
    ))
    .parent(&form_box)
    .build();
    let p_once = once_chk.widget_ptr() as *mut ffi::QCheckBox;
    fg.add(once_chk, 7, 0, 1, 2);

    let keep_lbl = Label::new(t("Snapshots à conserver"))
        .parent(&form_box)
        .build();
    fg.add(keep_lbl, 8, 0, 1, 1);
    let keep_sp = SpinBox::new()
        .range(0, 999)
        .value(7)
        .parent(&form_box)
        .build();
    let p_keep = keep_sp.widget_ptr() as *mut ffi::QSpinBox;
    fg.add(keep_sp, 8, 1, 1, 1);

    v_sched.add(form_box);
    v_sched.add(
        Label::new(t("Automatisations existantes"))
            .parent(&page_sched)
            .build(),
    );
    let timer_scroll = ScrollArea::new().parent(&page_sched).build();
    timer_scroll.set_widget_resizable(true);
    timer_scroll.set_widget(&*timer_table.borrow());
    v_sched.add(timer_scroll);

    // ===== Onglet Boot =====================================================
    let page_boot = Widget::new().parent(&central).build();
    let mut v_boot = VBoxLayout::with_parent(&page_boot);
    v_boot.set_spacing(8);
    let boot_box = GroupBox::new(t("Menu de démarrage")).parent(&page_boot).build();
    let mut bg = GridLayout::with_parent(&boot_box);
    let boot_info = Label::new("").parent(&boot_box).build();
    let p_bootpage = boot_info.widget_ptr() as *mut ffi::QLabel;
    bg.add(boot_info, 0, 0, 1, 2);
    let timeout_lbl = Label::new(t("Secondes d'affichage du menu (0 = masqué)"))
        .parent(&boot_box)
        .build();
    bg.add(timeout_lbl, 1, 0, 1, 1);
    let timeout_sp = SpinBox::new()
        .range(0, 600)
        .value(5)
        .parent(&boot_box)
        .build();
    let p_timeout = timeout_sp.widget_ptr() as *mut ffi::QSpinBox;
    bg.add(timeout_sp, 1, 1, 1, 1);
    v_boot.add(boot_box);
    v_boot.add(
        Label::new(t("Entrées de boot générées"))
            .parent(&page_boot)
            .build(),
    );
    let boot_scroll = ScrollArea::new().parent(&page_boot).build();
    boot_scroll.set_widget_resizable(true);
    boot_scroll.set_widget(&*boot_table.borrow());
    v_boot.add(boot_scroll);

    // --- Ui (avant les boutons pour les callbacks) --------------------------
    let ui = Rc::new(Ui {
        root,
        snapdir,
        log,
        snap_table,
        snap_rows: RefCell::new(Vec::new()),
        timer_table,
        timer_rows: RefCell::new(Vec::new()),
        boot_table,
        l_version: ptrs[0],
        l_boot: ptrs[1],
        l_default: ptrs[2],
        l_snapcount: ptrs[3],
        l_timercount: ptrs[4],
        l_disk: ptrs[5],
        l_status: status_ptr,
        l_bootpage: p_bootpage,
        p_tag,
        p_freq,
        p_day,
        p_daynum,
        p_hour,
        p_boot,
        p_delay,
        p_once,
        p_keep,
        p_timeout,
    });

    // --- boutons (après Ui : capturent un Weak<Ui>) -------------------------
    // En-tête : titre + Actualiser + Quitter, dans UN SEUL layout.
    {
        let mut hl = HBoxLayout::with_parent(&header);
        hl.set_spacing(8);
        hl.add(
            Label::new(format!(
                "Btrfsmgr — Gestionnaire de snapshots BTRFS (v{VERSION} — racine: {})",
                ui.root
            ))
            .parent(&header)
            .build(),
        );
        let w = Rc::downgrade(&ui);
        hl.add(
            PushButton::new(t("Actualiser"))
                .on_clicked(move || {
                    if let Some(u) = w.upgrade() {
                        u.refresh_all();
                    }
                })
                .parent(&header)
                .build(),
        );
        hl.add(
            PushButton::new(t("Quitter"))
                .on_clicked(|| std::process::exit(0))
                .parent(&header)
                .build(),
        );
        layouts.push(Box::new(hl));
        root_l.add(header);
    }


    // Snapshots
    {
        let w = Rc::downgrade(&ui);
        snap_hl.add(
            PushButton::new(t("Créer un instantané"))
                .on_clicked(move || {
                    if let Some(u) = w.upgrade() {
                        u.on_create();
                    }
                })
                .parent(&page_snap)
                .build(),
        );
        let w = Rc::downgrade(&ui);
        snap_hl.add(
            PushButton::new(t("Restaurer"))
                .on_clicked(move || {
                    if let Some(u) = w.upgrade() {
                        u.on_restore();
                    }
                })
                .parent(&page_snap)
                .build(),
        );
        let w = Rc::downgrade(&ui);
        snap_hl.add(
            PushButton::new(t("Supprimer"))
                .on_clicked(move || {
                    if let Some(u) = w.upgrade() {
                        u.on_delete();
                    }
                })
                .parent(&page_snap)
                .build(),
        );
        let w = Rc::downgrade(&ui);
        snap_hl.add(
            PushButton::new(t("Rétention (garder N)"))
                .on_clicked(move || {
                    if let Some(u) = w.upgrade() {
                        u.on_retention();
                    }
                })
                .parent(&page_snap)
                .build(),
        );
    }

    // Automatisation
    {
        let w = Rc::downgrade(&ui);
        let create_timer = PushButton::new(t("Créer le timer systemd"))
            .on_clicked(move || {
                if let Some(u) = w.upgrade() {
                    u.on_schedule_create();
                }
            })
            .parent(&page_sched)
            .build();
        v_sched.add(create_timer);
        let w = Rc::downgrade(&ui);
        let del_timer = PushButton::new(t("Supprimer l'automatisation sélectionnée"))
            .on_clicked(move || {
                if let Some(u) = w.upgrade() {
                    u.on_timer_delete();
                }
            })
            .parent(&page_sched)
            .build();
        v_sched.add(del_timer);
    }

    // Boot
    {
        let w = Rc::downgrade(&ui);
        let sync_btn = PushButton::new(t("Adapter les entrées systemd-boot"))
            .on_clicked(move || {
                if let Some(u) = w.upgrade() {
                    u.on_boot_sync();
                }
            })
            .parent(&page_boot)
            .build();
        v_boot.add(sync_btn);
        let w = Rc::downgrade(&ui);
        let timeout_btn = PushButton::new(t("Appliquer le timeout"))
            .on_clicked(move || {
                if let Some(u) = w.upgrade() {
                    u.on_boot_timeout();
                }
            })
            .parent(&page_boot)
            .build();
        v_boot.add(timeout_btn);
    }

    tabs.add_tab(Box::new(page_info), &t("Général"));
    tabs.add_tab(Box::new(page_snap), &t("Snapshots"));
    tabs.add_tab(Box::new(page_sched), &t("Automatisation"));
    tabs.add_tab(Box::new(page_boot), &t("Boot"));
    if let Ok(idx) = std::env::var("BTRFSMGR_GUI_TAB") {
        if let Ok(idx) = idx.parse::<i32>() {
            tabs.set_current_index(idx);
        }
    }
    root_l.add(tabs);

    // --- journal + statut (en bas, APRÈS les onglets) -----------------------
    root_l.add(Label::new(t("Journal")).parent(&central).build());
    let log_scroll = ScrollArea::new().parent(&central).build();
    log_scroll.set_widget_resizable(true);
    log_scroll.set_widget(&*ui.log.borrow());
    unsafe {
        ffi::QWidget_setMinimumSize(log_scroll.widget_ptr() as *mut ffi::QWidget, 0, 150);
    }
    root_l.add(log_scroll);
    root_l.add(status_lbl);

    // Les layouts créés ci-dessus doivent rester vivants après le retour de
    // build_ui : un VBoxLayout/HBoxLayout droppé détruit le QLayout C++ et
    // laisse tous ses enfants sans géométrie (widgets empilés à 0,0).
    layouts.push(Box::new(root_l));
    layouts.push(Box::new(v_info));
    layouts.push(Box::new(g));
    layouts.push(Box::new(v_snap));
    layouts.push(Box::new(v_snap_box));
    layouts.push(Box::new(snap_hl));
    layouts.push(Box::new(v_sched));
    layouts.push(Box::new(fg));
    layouts.push(Box::new(v_boot));
    layouts.push(Box::new(bg));

    // Le widget central est rattaché APRÈS que l'arbre soit complet : le faire
    // avant laisse le QTabWidget sans géométrie (hauteur nulle) au premier
    // rendu sous xcb.
    window.set_central_widget(&central);

    // Contrôle de géométrie : détecte la régression « layouts droppés »
    // (onglets à 0×0, widgets empilés à 0,0). Utilisé par tools/smoke.sh.
    if std::env::var("BTRFSMGR_GUI_ASSERT_GEOM").is_ok() {
        qtrs::Timer::single_shot(1200, move || {
            let p = tabs_ptr as *mut ffi::QWidget;
            let (w, h) = unsafe { (ffi::QWidget_width(p), ffi::QWidget_height(p)) };
            if w < 200 || h < 100 {
                eprintln!("GEOM_FAIL tabs w={w} h={h}");
                std::process::exit(1);
            }
            eprintln!("GEOM_OK tabs w={w} h={h}");
            std::process::exit(0);
        });
    }

    (window, ui, layouts)
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn parse_args(args: &[String]) -> (String, String) {
    let mut root = "/".to_string();
    let mut snapdir = DEFAULT_SNAPDIR.to_string();
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--snapdir" {
            if let Some(v) = args.get(i + 1) {
                snapdir = v.clone();
            }
            i += 2;
            continue;
        }
        if let Some(v) = a.strip_prefix("--snapdir=") {
            snapdir = v.to_string();
            i += 1;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        positional.push(a.to_string());
        i += 1;
    }
    if let Some(p) = positional.first() {
        root = std::path::absolute(p)
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_else(|_| p.clone());
    }
    (root, snapdir)
}

fn relaunch_root(args: &[String]) -> ! {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("btrfsmgr-gui"));
    let mut cmd = std::process::Command::new("pkexec");
    cmd.arg("env");
    for k in [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "XDG_RUNTIME_DIR",
        "XDG_SESSION_TYPE",
        "XDG_SESSION_DESKTOP",
        "XDG_CURRENT_DESKTOP",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        if let Ok(v) = std::env::var(k) {
            cmd.arg(format!("{k}={v}"));
        }
    }
    cmd.arg(exe);
    for a in args {
        cmd.arg(a);
    }
    eprintln!("btrfsmgr-gui : relance en root via pkexec…");
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    eprintln!("btrfsmgr-gui : relance impossible ({err}).");
    eprintln!("  Essayez : sudo btrfsmgr-gui");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Mode headless utilisé par le script d'instantané quand SEUL le paquet
    // GUI est installé (il ne fournit plus /usr/bin/btrfsmgr) :
    //   btrfsmgr-gui sync <root> --snapdir <dir>
    if args.iter().any(|a| a == "sync") {
        let rest: Vec<String> = args.iter().filter(|a| a.as_str() != "sync").cloned().collect();
        let (root, snapdir) = parse_args(&rest);
        sync_bootloader_entries(&root, &snapdir, true);
        std::process::exit(0);
    }
    let (root, snapdir) = parse_args(&args);

    // Crochet de test : permet un smoke test hors root (le vrai usage relance
    // via pkexec). Ne change rien pour un lancement normal en root.
    let allow_nonroot = std::env::var("BTRFSMGR_GUI_ALLOW_NONROOT").is_ok();
    if !is_root() && !allow_nonroot {
        relaunch_root(&args);
    }
    if !btrfs_available() {
        eprintln!("ERREUR: 'btrfs' introuvable. Installez btrfs-progs.");
        std::process::exit(1);
    }
    // Réécrire le script d'instantané au démarrage (root) : correctifs
    // appliqués aux automatisations existantes, sans les recréer.
    if is_root() {
        install_snap_script();
    }
    if std::env::var("QT_QPA_PLATFORM").is_err()
        && std::env::var("DISPLAY").is_err()
        && std::env::var("WAYLAND_DISPLAY").is_err()
    {
        eprintln!("ERREUR: impossible d'initialiser l'interface — aucun affichage accessible.");
        std::process::exit(1);
    }

    let app = Application::new();
    app.set_desktop_file_name("btrfsmgr-gui");
    let (window, ui, _layouts) = build_ui(root.clone(), snapdir.clone());
    ui.log_line(&format!(
        "{} v{} — sous-volume racine: {}",
        APP, VERSION, root
    ));
    window.show();
    ui.refresh_all();
    // Smoke test automatisé : quitte après N ms.
    if let Ok(ms) = std::env::var("BTRFSMGR_GUI_SMOKE_MS") {
        if let Ok(ms) = ms.parse::<i32>() {
            // Single-shot sur le thread Qt : évite l'arrêt d'un timer Qt
            // depuis un autre thread (« Timers cannot be stopped… »).
            qtrs::Timer::single_shot(ms, || std::process::exit(0));
        }
    }
    app.exec();
}
