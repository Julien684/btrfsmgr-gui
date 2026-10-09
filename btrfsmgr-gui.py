#!/usr/bin/env python3
"""
btrfsmgr-gui — Interface graphique (GTK3) pour gérer les sous-volumes BTRFS.

Réutilise la logique métier du TUI (btrfsmgr.py) sans la dupliquer :
  * Créer / lister / restaurer / supprimer des instantanés
  * Rétention (conserver les N plus récents)
  * Automatisation via timers systemd (création + suppression)
  * Menu systemd-boot (affichage au démarrage)
  * Infos système (chargeur de boot, timers, espace disque)

La sortie console des fonctions réutilisées est capturée et affichée dans
un panneau de journal, afin qu'aucune information ne soit perdue par rapport
au mode TUI.

Dépendances graphiques : python3-gobject (PyGObject), gtk3.
Dépendances métier    : btrfs-progs, systemd, python3 >= 3.8.

Usage:
    sudo btrfsmgr-gui          # gère le sous-volume /
    sudo btrfsmgr-gui /chemin  # gère un autre sous-volume BTRFS
"""
from __future__ import annotations

import contextlib
import io
import os
import shutil
import sys
import threading

# ---------------------------------------------------------------------------
# Charger la logique métier (btrfsmgr.py) — depuis le dossier source ou depuis
# l'installation (/bin/btrfsmgr).
# ---------------------------------------------------------------------------
def _load_core():
    candidates = [
        os.path.join(os.path.dirname(os.path.abspath(__file__)), "btrfsmgr.py"),
        os.path.join(os.getcwd(), "btrfsmgr.py"),
        "/usr/bin/btrfsmgr",
        "/bin/btrfsmgr",
        "/usr/local/bin/btrfsmgr",
    ]
    # Déjà importable (module installé sur le PYTHONPATH) ?
    try:
        import btrfsmgr  # noqa: F401
        return sys.modules["btrfsmgr"]
    except ImportError:
        pass
    import importlib.util
    import importlib.machinery
    for p in candidates:
        if os.path.isfile(p):
            spec = importlib.util.spec_from_file_location("_btrfsmgr_core", p)
            if spec is None or spec.loader is None:
                loader = importlib.machinery.SourceFileLoader(
                    "_btrfsmgr_core", p)
                spec = importlib.util.spec_from_loader(
                    "_btrfsmgr_core", loader)
            mod = importlib.util.module_from_spec(spec)
            sys.modules["_btrfsmgr_core"] = mod
            spec.loader.exec_module(mod)  # type: ignore[union-attr]
            return mod
    raise ImportError(
        "Impossible de localiser la logique métier (btrfsmgr.py).\n"
        "Placez btrfsmgr-gui.py à côté de btrfsmgr.py, ou installez btrfsmgr.")


CORE = _load_core()
APP = CORE.APP
VERSION = CORE.VERSION
LANGUAGE = CORE.LANGUAGE
_ = CORE._
_f = CORE._f
DEFAULT_SNAPDIR = CORE.DEFAULT_SNAPDIR


# ---------------------------------------------------------------------------
# GTK / Gtk3 via PyGObject
# ---------------------------------------------------------------------------
try:
    import gi
    gi.require_version("Gtk", "3.0")
    from gi.repository import Gtk, GLib
except Exception as e:  # pragma: no cover
    sys.stderr.write(
        "ERREUR: interface graphique indisponible — installez "
        "python3-gobject et gtk3.\n  Détail: %s\n" % e)
    sys.exit(1)


# ---------------------------------------------------------------------------
# Exécution des opérations métier hors du thread UI (pas de blocage) et
# capture de leur sortie console.
# ---------------------------------------------------------------------------
def _call_core(fn, *args, **kwargs):
    """Exécute une fonction métier, capture stdout/stderr et renvoie
    (resultat, sortie_console).

    On capture aussi SystemExit : la logique métier (partagée avec le TUI)
    appelle sys.exit() en cas d'erreur fatale — ce qui tuerait le GUI.  On
    la transforme en échec signalé dans le journal, sans fermer l'application.
    """
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(buf):
        try:
            result = fn(*args, **kwargs)
        except SystemExit as e:
            buf.write(f"ERREUR: opération interrompue (code {e.code}).\n")
            result = None
        except Exception as e:  # noqa: BLE001
            buf.write(f"EXCEPTION: {e!r}\n")
            import traceback
            traceback.print_exc(file=buf)
            result = None
    return result, buf.getvalue()


def run_async(fn, on_done, *args, **kwargs):
    """Lance fn() dans un thread d'arrière-plan puis appelle on_done(result,
    output) sur le thread principal GTK via GLib.idle_add."""
    def _worker():
        res, out = _call_core(fn, *args, **kwargs)
        GLib.idle_add(on_done, res, out)
    threading.Thread(target=_worker, daemon=True).start()


# ---------------------------------------------------------------------------
# Widgets utilitaires
# ---------------------------------------------------------------------------
def _vbox(spacing=8, margin=12):
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=spacing)
    box.set_margin_start(margin)
    box.set_margin_end(margin)
    box.set_margin_top(margin)
    box.set_margin_bottom(margin)
    return box


def _label(text=None, markup=None, selectable=False):
    lb = Gtk.Label(label=text or "")
    if markup:
        lb.set_markup(markup)
    lb.set_selectable(selectable)
    lb.set_xalign(0)
    lb.set_line_wrap(True)
    return lb


def _button(text, handler=None, sensitive=True, style=None):
    btn = Gtk.Button(label=text, sensitive=sensitive)
    if handler:
        btn.connect("clicked", handler)
    if style == "destructive":
        btn.get_style_context().add_class("destructive-action")
    elif style == "suggested":
        btn.get_style_context().add_class("suggested-action")
    return btn


def _spinner() -> Gtk.Spinner:
    sp = Gtk.Spinner()
    sp.set_visible(False)
    return sp


# ---------------------------------------------------------------------------
# Dialogues
# ---------------------------------------------------------------------------
def message(parent, kind, title, text):
    dlg = Gtk.MessageDialog(
        transient_for=parent, modal=True,
        message_type=kind, buttons=Gtk.ButtonsType.OK,
        text=title, secondary_text=text)
    dlg.run()
    dlg.destroy()


def ask_yes_no(parent, title, text) -> bool:
    dlg = Gtk.MessageDialog(
        transient_for=parent, modal=True,
        message_type=Gtk.MessageType.QUESTION,
        buttons=Gtk.ButtonsType.YES_NO, text=title,
        secondary_text=text)
    res = dlg.run()
    dlg.destroy()
    return res == Gtk.ResponseType.YES


def _entry_row(label_text, default="") -> tuple[Gtk.Box, Gtk.Entry]:
    box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
    lb = _label(label_text)
    entry = Gtk.Entry()
    entry.set_text(default)
    box.pack_start(lb, False, False, 0)
    box.pack_start(entry, True, True, 0)
    return box, entry


def _spin_row(label_text, lo, hi, default) -> tuple[Gtk.Box, Gtk.SpinButton]:
    box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
    lb = _label(label_text)
    adj = Gtk.Adjustment(value=default, lower=lo, upper=hi, step_increment=1)
    spin = Gtk.SpinButton(adjustment=adj, climb_rate=1.0, digits=0)
    box.pack_start(lb, False, False, 0)
    box.pack_start(spin, False, False, 0)
    return box, spin


# ---------------------------------------------------------------------------
# Application principale
# ---------------------------------------------------------------------------
class BtrfsMgrApp:
    def __init__(self, root: str, snapdir: str = DEFAULT_SNAPDIR):
        self.root = self._validate_root(root)
        self.snapdir = snapdir
        self._busy = False

        self.builder = Gtk.Builder()
        win = Gtk.Window()
        win.set_title(_f(" {app} — Gestionnaire BTRFS", app=APP.capitalize()))
        win.set_default_size(940, 620)
        win.set_border_width(0)
        win.connect("delete-event", lambda *_: Gtk.main_quit())

        # ---- barre d'en-tête --------------------------------------------
        header = Gtk.HeaderBar()
        header.set_show_close_button(True)
        header.props.title = _f("{app} — Gestionnaire de snapshots BTRFS",
                                app=APP.capitalize())
        header.props.subtitle = _f("v{ver} — racine: {root}",
                                   ver=VERSION, root=self.root)
        win.set_titlebar(header)

        refresh_btn = Gtk.Button(label=_("Actualiser"))
        refresh_btn.connect("clicked", lambda *_: self.refresh_all())
        header.pack_end(refresh_btn)

        quit_btn = Gtk.Button(label=_("Quitter"))
        quit_btn.connect("clicked", lambda *_: Gtk.main_quit())
        header.pack_end(quit_btn)

        # ---- layout principal : sidebar (gauche) + pile (droite) ----------
        self.main = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        win.add(self.main)

        paned = Gtk.Paned(orientation=Gtk.Orientation.HORIZONTAL)
        self.main.pack_start(paned, True, True, 0)

        # sidebar des boutons de navigation
        sidebar = _vbox(spacing=4, margin=10)
        sidebar.set_size_request(210, -1)
        sidebar_label = _label(markup="<b>" + _("Actions") + "</b>")
        sidebar.pack_start(sidebar_label, False, False, 0)
        paned.pack1(sidebar, False, False)

        self.stack = Gtk.Stack()
        self.stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        paned.pack2(self.stack, True, True)

        # --- panneau de journal (bas de la fenêtre) ----------------------
        log_label = _label(markup="<b>" + _("Journal") + "</b>")
        self.main.pack_start(log_label, False, False, 0)
        log_sw = Gtk.ScrolledWindow()
        log_sw.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
        log_sw.set_min_content_height(120)
        self.textlog = Gtk.TextView()
        self.textlog.set_editable(False)
        self.textlog.set_monospace(True)
        self.textlog.set_wrap_mode(Gtk.WrapMode.NONE)
        log_sw.add(self.textlog)
        self.main.pack_start(log_sw, False, False, 0)
        self.logbuf = self.textlog.get_buffer()

        # barre de statut avec spinner
        statusbar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        statusbar.set_margin_start(10)
        statusbar.set_margin_end(10)
        statusbar.set_margin_bottom(6)
        self.spinner = _spinner()
        self.status = _label("")
        statusbar.pack_start(self.spinner, False, False, 0)
        statusbar.pack_start(self.status, True, True, 0)
        self.main.pack_start(statusbar, False, False, 0)

        # ---- construction des pages ---------------------------------------
        self._build_info_page()
        self._build_snapshots_page()
        self._build_schedule_page()
        self._build_boot_page()

        # boutons de navigation
        for name, label_txt in (("info", _("Général")),
                                ("snapshots", _("Snapshots")),
                                ("schedule", _("Automatisation")),
                                ("boot", _("Boot"))):
            nav = _button(label_txt,
                          (lambda _btn, n=name:
                           self.stack.set_visible_child_name(n)))
            sidebar.pack_start(nav, False, False, 0)

        self.win = win
        win.show_all()
        self.log(_f("{app} v{ver} — sous-volume racine: {root}",
                    app=APP.capitalize(), ver=VERSION, root=self.root))
        # synchronisation des entrées de boot + montage snapdir au démarrage
        self.refresh_all(boot_sync=True)

    # ------------------------------------------------------------------ root
    @staticmethod
    def _validate_root(raw_root: str) -> str:
        r = raw_root.rstrip("/")
        if r == "" or r == "/":
            return "/"
        if r in ("/snapshots", "snapshots"):
            return "/"
        return "/"

    # ------------------------------------------------------------------ log
    def log(self, text: str):
        self.logbuf.insert(self.logbuf.get_end_iter(), text + "\n")
        # auto-scroll
        try:
            self.textlog.scroll_mark_onscreen(
                self.logbuf.create_mark(None, self.logbuf.get_end_iter(), False))
        except Exception:
            pass

    def set_busy(self, busy: bool):
        self._busy = busy
        if busy:
            self.spinner.start()
            self.status.set_text(_("Opération en cours…"))
        else:
            self.spinner.stop()
            self.status.set_text(_("Prêt."))

    def _on_done(self, result, output, success_message=""):
        self.set_busy(False)
        if output.strip():
            self.log(output.rstrip())
        if success_message:
            self.log("✓ " + success_message)

    # ----------------------------------------------------------------- info
    def _build_info_page(self):
        page = _vbox()
        page.set_name("info")
        self.stack.add_named(page, "info")

        card = Gtk.Frame(label=_("Informations"))
        grid = Gtk.Grid(column_spacing=16, row_spacing=8)
        card.add(grid)
        card.set_margin_top(6)
        page.pack_start(card, False, False, 0)

        self.lbl_version = _label(selectable=True)
        self.lbl_bootloader = _label(selectable=True)
        self.lbl_default_subvol = _label(selectable=True)
        self.lbl_snap_count = _label()
        self.lbl_timer_count = _label()
        self.lbl_disk = _label(selectable=True)

        rows = [
            (_("Version"), self.lbl_version),
            (_("Chargeur de boot"), self.lbl_bootloader),
            (_("Subvolume par défaut"), self.lbl_default_subvol),
            (_("Snapshots existants"), self.lbl_snap_count),
            (_("Timers actifs"), self.lbl_timer_count),
        ]
        for i, (t, w) in enumerate(rows):
            grid.attach(_label(markup=f"<b>{t}</b>"), 0, i, 1, 1)
            grid.attach(w, 1, i, 1, 1)
        grid.attach(_label(markup="<b>" + _("Espace disque") + "</b>"), 0,
                    len(rows), 1, 1)
        grid.attach(self.lbl_disk, 1, len(rows), 1, 1)

        hint = _label(_("Cliquez sur « Actualiser » ou changez de page pour "
                        "recharger ces informations."))
        hint.set_margin_top(10)
        page.pack_start(hint, False, False, 0)

    # ------------------------------------------------------------ snapshots
    def _build_snapshots_page(self):
        page = _vbox()
        page.set_name("snapshots")
        self.stack.add_named(page, "snapshots")

        toolbar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        page.pack_start(toolbar, False, False, 0)

        btn_create = _button(_("Créer un instantané"), self.on_create,
                             style="suggested")
        btn_restore = _button(_("Restaurer"), self.on_restore)
        btn_del = _button(_("Supprimer"), self.on_delete,
                          style="destructive")
        btn_retention = _button(_("Rétention (garder N)"), self.on_retention)
        for b in (btn_create, btn_restore, btn_del, btn_retention):
            toolbar.pack_start(b, False, False, 0)

        # colonnes : nom | génération | chemin
        self.store = Gtk.ListStore(str, str, str)
        self.tree = Gtk.TreeView(model=self.store)
        self.tree.get_selection().set_mode(Gtk.SelectionMode.MULTIPLE)
        for i, title in ((0, _("Nom")), (1, _("Gen")), (2, _("Chemin complet"))):
            col = Gtk.TreeViewColumn(title, Gtk.CellRendererText(), text=i)
            col.set_expand(i == 2)
            self.tree.append_column(col)

        sw = Gtk.ScrolledWindow()
        sw.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
        sw.add(self.tree)
        page.pack_start(sw, True, True, 0)

        hint = _label(_("Astuce : sélectionnez plusieurs lignes (Ctrl/Shift) "
                        "pour supprimer en une fois."))
        page.pack_start(hint, False, False, 0)

    def _snap_rows(self) -> list[tuple[str, str, str]]:
        snaps = CORE.list_snapshots(self.root, self.snapdir)
        # plus récent en premier (comme le TUI)
        rows = [(s["path"], str(s["gen"]), s["full"])
                for s in reversed(snaps)]
        return rows

    def _selected_paths(self) -> list[str]:
        paths = []
        model, treeiters = self.tree.get_selection().get_selected_rows()
        for it in treeiters:
            paths.append(model.get_value(it, 2))
        return paths

    def refresh_snapshots(self):
        self.store.clear()
        run_async(lambda: CORE.list_snapshots(self.root, self.snapdir),
                  self._load_snaps_done)

    def _load_snaps_done(self, result, output):
        if output.strip():
            self.log(output.rstrip())
        if result is None:
            return
        self.store.clear()
        rows = [(s["path"], str(s["gen"]), s["full"])
                for s in reversed(result)]
        if not rows:
            self.log(_("Aucun snapshot."))
        for r in rows:
            self.store.append(list(r))

    # -------------------------------------------------------------- create
    def on_create(self, *a):
        dlg = Gtk.Dialog(title=_("Créer un instantané"),
                         transient_for=self.win, modal=True)
        dlg.add_buttons(Gtk.STOCK_CANCEL, Gtk.ResponseType.CANCEL,
                        Gtk.STOCK_OK, Gtk.ResponseType.OK)
        box = dlg.get_content_area()
        box.set_spacing(8)
        box.set_margin_top(12)
        box.set_margin_bottom(12)
        box.set_margin_start(12)
        box.set_margin_end(12)
        r, entry = _entry_row(_("Nom du snapshot"), CORE.now_stamp())
        box.pack_start(r, False, False, 0)
        dlg.show_all()
        if dlg.run() == Gtk.ResponseType.OK:
            name = entry.get_text().strip()
            dlg.destroy()
            if not name:
                message(self.win, Gtk.MessageType.ERROR, _("Nom vide"),
                        _("Veuillez saisir un nom de snapshot."))
                return
            self.set_busy(True)
            run_async(
                lambda: (CORE.create_snapshot(self.root, self.snapdir, name),
                         CORE.sync_bootloader_entries(self.root, self.snapdir,
                                                      verbose=False)),
                lambda r, o: self._create_done(r, o, name))
        else:
            dlg.destroy()

    def _create_done(self, result, output, name):
        self.set_busy(False)
        if output.strip():
            self.log(output.rstrip())
        self.log(_f("✓ Snapshot créé: {name}", name=name))
        self.refresh_snapshots()
        self.refresh_info()

    # ------------------------------------------------------------- restore
    def on_restore(self, *a):
        selected = self._selected_paths()
        if not selected:
            message(self.win, Gtk.MessageType.INFO, _("Rien à restaurer"),
                    _("Sélectionnez un snapshot dans la liste."))
            return
        if len(selected) > 1:
            message(self.win, Gtk.MessageType.WARNING, _("Un seul à la fois"),
                    _("La restauration concerne un seul snapshot à la fois."))
            return
        full = selected[0]
        name = os.path.basename(full)
        at = CORE.boot_subvol(self.root)
        if ask_yes_no(
                self.win, _f("Restaurer {name} ?", name=name),
                _f("Le snapshot prendra la place de « {at} » par échange CoW "
                   "(deux mv).\nL'ancien sous-volume sera conservé dans "
                   "{snapdir}/.\n\nRedémarrez ensuite pour booter sur la "
                   "version restaurée.\n\nCette opération est destructive — "
                   "poursuivre ?", at=at, snapdir=self.snapdir)):
            self.set_busy(True)
            run_async(
                lambda: (CORE.restore_snapshot(self.root, self.snapdir, name),
                         CORE.sync_bootloader_entries(self.root, self.snapdir,
                                                      verbose=False)),
                lambda r, o: self._restore_done(r, o, name))

    def _restore_done(self, result, output, name):
        rc, _out2 = (result or (1, "")) if isinstance(result, tuple) else (0, "")
        self.set_busy(False)
        if output.strip():
            self.log(output.rstrip())
        if rc == 0:
            self.log(_f("✓ Restauration terminée: {name} est maintenant "
                        "le sous-volume principal.", name=name))
            if ask_yes_no(self.win, _("Redémarrer maintenant ?"),
                          _("Voulez-vous redémarrer immédiatement pour booter "
                            "sur la version restaurée ?")):
                os.system("systemctl reboot &")
        else:
            self.log(_f("✗ Échec de la restauration de {name}.", name=name))
        self.refresh_snapshots()
        self.refresh_info()

    # ------------------------------------------------------------- delete
    def on_delete(self, *a):
        selected = self._selected_paths()
        if not selected:
            message(self.win, Gtk.MessageType.INFO, _("Rien à supprimer"),
                    _("Sélectionnez un ou plusieurs snapshots dans la liste."))
            return
        confirm = "\n".join(os.path.basename(p) for p in selected)
        if not ask_yes_no(
                self.win, _f("Supprimer {n} snapshot(s) ?", n=len(selected)),
                _("Suppression DÉFINITIVE (sous-volumes) :\n\n") + confirm):
            return
        self.set_busy(True)
        run_async(lambda: self._do_delete(selected),
                  lambda r, o: self._delete_done(r, o, len(selected)))

    def _do_delete(self, full_paths):
        results = []
        for p in full_paths:
            rc, out = CORE.delete_subvol(p)
            results.append((p, rc, out))
        CORE.sync_bootloader_entries(self.root, self.snapdir, verbose=False)
        return results

    def _delete_done(self, result, output, count):
        self.set_busy(False)
        if output.strip():
            self.log(output.rstrip())
        ok = sum(1 for _p, rc, _o in result if rc == 0)
        self.log(_f("{ok}/{n} supprimé(s).", ok=ok, n=count))
        self.refresh_snapshots()
        self.refresh_info()

    # ------------------------------------------------------------ retention
    def on_retention(self, *a):
        dlg = Gtk.Dialog(title=_("Rétention manuelle"),
                         transient_for=self.win, modal=True)
        dlg.add_buttons(Gtk.STOCK_CANCEL, Gtk.ResponseType.CANCEL,
                        Gtk.STOCK_OK, Gtk.ResponseType.OK)
        box = dlg.get_content_area()
        box.set_spacing(8)
        box.set_margin_top(12)
        box.set_margin_bottom(12)
        box.set_margin_start(12)
        box.set_margin_end(12)
        r, spin = _spin_row(_("Conserver les N plus récents (0 = tous)"),
                            0, 999, 7)
        box.pack_start(r, False, False, 0)
        dlg.show_all()
        if dlg.run() == Gtk.ResponseType.OK:
            keep = spin.get_value_as_int()
            dlg.destroy()
            snaps = CORE.list_snapshots(self.root, self.snapdir)
            to_del = len(snaps) - keep if keep > 0 else 0
            if to_del <= 0:
                message(self.win, Gtk.MessageType.INFO,
                        _("Rien à supprimer"),
                        _("Aucun snapshot à supprimer (déjà ≤ N)."))
                return
            if ask_yes_no(self.win, _f("Supprimer {n} snapshot(s) ?",
                                       n=to_del),
                          _("Garder les %d plus récents, supprimer les autres "
                            "— poursuivre ?") % keep):
                self.set_busy(True)
                run_async(lambda: CORE.prune_snapshots(self.root, self.snapdir,
                                                        keep),
                          lambda r, o: self._retention_done(r, o, keep))
        else:
            dlg.destroy()

    def _retention_done(self, result, output, keep):
        self.set_busy(False)
        if output.strip():
            self.log(output.rstrip())
        self.log(_f("✓ Rétention appliquée (garder {keep}).", keep=keep))
        self.refresh_snapshots()
        self.refresh_info()

    # ------------------------------------------------------------ schedule
    def _build_schedule_page(self):
        page = _vbox()
        page.set_name("schedule")
        self.stack.add_named(page, "schedule")

        # --- formulaire de création --------------------------------------
        form = Gtk.Frame(label=_("Créer une automatisation"))
        fb = _vbox(spacing=8, margin=8)
        form.add(fb)
        page.pack_start(form, False, False, 0)

        rtag, self.e_tag = _entry_row(_("Nom du plan (ex. daily, weekly)"),
                                      "daily")
        fb.pack_start(rtag, False, False, 0)

        # fréquence
        fb.pack_start(_label(markup="<b>" + _("Fréquence") + "</b>"),
                      False, False, 0)
        self.freq = Gtk.ComboBoxText()
        for f in (_("Chaque jour"), _("Chaque semaine"), _("Chaque mois")):
            self.freq.append_text(f)
        self.freq.set_active(0)
        self.freq.connect("changed", lambda *_: self._sync_day_visibility())
        fb.pack_start(self.freq, False, False, 0)

        # jour de la semaine (visible si hebdomadaire)
        rday, self.day_combo = _entry_row(_("Jour de la semaine (Mon..Sun)"),
                                          "Mon")
        fb.pack_start(rday, False, False, 0)
        self._freq_box = self.freq
        self._day_row = rday
        rday.set_no_show_all(True)
        rday.hide()

        # heure
        rhour, self.spin_hour = _spin_row(_("Heure (0-23)"), 0, 23, 3)
        fb.pack_start(rhour, False, False, 0)

        # au démarrage
        rb_boot, self.chk_boot, self.spin_delay = self._boot_options()
        fb.pack_start(rb_boot, False, False, 0)

        # rétention
        rkeep, self.spin_keep = _spin_row(_("Snapshots à conserver"), 0, 999, 7)
        fb.pack_start(rkeep, False, False, 0)

        create_btn = _button(_("Créer le timer systemd"), self.on_schedule_create,
                             style="suggested")
        fb.pack_start(create_btn, False, False, 0)

        # --- liste des timers existants ------------------------------------
        hdr = _label(markup="<b>" + _("Automatisations existantes") + "</b>")
        hdr.set_margin_top(12)
        page.pack_start(hdr, False, False, 0)

        self.timer_store = Gtk.ListStore(str, str, str, str)  # timer, next, last, activated
        self.timer_tree = Gtk.TreeView(model=self.timer_store)
        for i, title in ((0, _("Unité")), (1, _("Prochaine exécution")),
                         (2, _("Dernière")), (3, _("Activée"))):
            col = Gtk.TreeViewColumn(title, Gtk.CellRendererText(), text=i)
            col.set_expand(i == 0)
            self.timer_tree.append_column(col)
        tsw = Gtk.ScrolledWindow()
        tsw.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
        tsw.set_min_content_height(160)
        tsw.add(self.timer_tree)
        page.pack_start(tsw, True, True, 0)

        del_btn = _button(_("Supprimer l'automatisation sélectionnée"),
                          self.on_timer_delete, style="destructive")
        page.pack_start(del_btn, False, False, 0)

        self.chk_boot_value = False

    def _boot_options(self):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        chk = Gtk.CheckButton(label=_("Au démarrage de la machine "
                                      "(au lieu d'un horaire)"))
        r, spin = _spin_row(_("Minutes après le démarrage"), 0, 1440, 5)
        spin.set_sensitive(False)
        r.set_sensitive(False)
        chk.connect("toggled",
                    lambda c: (r.set_sensitive(c.get_active()),
                               spin.set_sensitive(c.get_active())))
        box.pack_start(chk, False, False, 0)
        box.pack_start(r, False, False, 0)
        return box, chk, spin

    def _sync_day_visibility(self, *a):
        active = self.freq.get_active()
        self._day_row.set_visible(active == 1)

    def on_schedule_create(self, *a):
        tag = self.e_tag.get_text().strip() or "daily"
        freq = self.freq.get_active()          # 0=day 1=week 2=month
        boot = self.chk_boot.get_active()
        boot_delay = self.spin_delay.get_value_as_int()
        keep = self.spin_keep.get_value_as_int()
        hour = self.spin_hour.get_value_as_int()

        schedule = ""
        if not boot:
            if freq == 0:
                schedule = f"*-*-* {hour:02d}:00:00"
            elif freq == 1:
                day = self.day_combo.get_text().strip() or "Mon"
                schedule = f"{day} *-*-* {hour:02d}:00:00"
            else:
                # mois: jour du mois
                dlg = Gtk.Dialog(title=_("Jour du mois"),
                                 transient_for=self.win, modal=True)
                dlg.add_buttons(Gtk.STOCK_CANCEL, Gtk.ResponseType.CANCEL,
                                Gtk.STOCK_OK, Gtk.ResponseType.OK)
                bx = dlg.get_content_area()
                bx.set_spacing(8)
                bx.set_margin_top(12); bx.set_margin_bottom(12)
                bx.set_margin_start(12); bx.set_margin_end(12)
                r, spd = _spin_row(_("Numéro du jour dans le mois (1-31)"),
                                   1, 31, 1)
                bx.pack_start(r, False, False, 0)
                dlg.show_all()
                if dlg.run() == Gtk.ResponseType.OK:
                    daynum = spd.get_value_as_int()
                    dlg.destroy()
                else:
                    dlg.destroy()
                    return
                schedule = f"*-*-{daynum} {hour:02d}:00:00"

        plan = (_("Au démarrage (%d min après le boot)") % boot_delay) if boot \
            else f"OnCalendar={schedule}"
        if not ask_yes_no(
                self.win,
                _f("Créer le timer systemd '{tag}' ?", tag=tag),
                _f("Planning : {plan}\n"
                   "Rétention: {keep} snapshot(s)\n"
                   "Cible    : {root}/{snapdir}",
                   plan=plan, keep=keep, root=self.root,
                   snapdir=self.snapdir)):
            return
        self.set_busy(True)
        run_async(
            lambda: CORE.write_timer_files(self.root, self.snapdir, tag,
                                           schedule, keep, boot=boot,
                                           boot_delay=boot_delay),
            lambda r, o: self._schedule_create_done(r, o, tag))

    def _schedule_create_done(self, result, output, tag):
        self.set_busy(False)
        if output.strip():
            self.log(output.rstrip())
        self.log(_f("✓ Timer '{tag}' créé et activé.", tag=tag))
        self.refresh_timers()

    def refresh_timers(self):
        self.timer_store.clear()
        run_async(CORE.list_timers, self._load_timers_done)

    def _load_timers_done(self, result, output):
        if result is None:
            return
        self.timer_store.clear()
        for t in result:
            self.timer_store.append([t["timer"], t["next"], t["last"],
                                     t["activated"]])

    def on_timer_delete(self, *a):
        model, iters = self.timer_tree.get_selection().get_selected_rows()
        if not iters:
            message(self.win, Gtk.MessageType.INFO, _("Rien à supprimer"),
                    _("Sélectionnez une automatisation dans la liste."))
            return
        timer = model.get_value(iters[0], 0)
        # extraire le tag de l'unité (même logique que le TUI)
        import re as _re
        def safe(s):
            s = _re.sub(r"[^A-Za-z0-9._-]", "-", s.strip("/"))
            return s.strip("-")
        prefix = (f"{APP}-snap-{safe(self.root) or 'root'}-"
                  f"{safe(self.snapdir) or 'snap'}-")
        tag = timer.removeprefix(prefix).removesuffix(".timer")
        if ask_yes_no(self.win,
                      _f("Supprimer l'automatisation '{tag}' ?", tag=tag),
                      _("Le timer sera désactivé et ses fichiers supprimés.")):
            self.set_busy(True)
            run_async(lambda: CORE.delete_timer_files(self.root, self.snapdir,
                                                       tag),
                      lambda r, o: self._timer_delete_done(r, o, tag))

    def _timer_delete_done(self, result, output, tag):
        self.set_busy(False)
        if output.strip():
            self.log(output.rstrip())
        self.log(_f("✓ Automatisation '{tag}' supprimée.", tag=tag))
        self.refresh_timers()

    # ---------------------------------------------------------------- boot
    def _build_boot_page(self):
        page = _vbox()
        page.set_name("boot")
        self.stack.add_named(page, "boot")

        info = _label(_("Afficher ou masquer le menu systemd-boot au "
                        "démarrage (clr-boot-manager set-timeout)."))
        info.set_margin_top(6)
        page.pack_start(info, False, False, 0)

        self.lbl_boot_loader = _label(selectable=True)
        page.pack_start(self.lbl_boot_loader, False, False, 0)

        frame = Gtk.Frame(label=_("Menu au démarrage"))
        fb = _vbox(spacing=8, margin=8)
        frame.add(fb)
        page.pack_start(frame, False, False, 0)

        fb.pack_start(_label(_("N > 0 : le menu est affiché pendant N "
                               "secondes\n0     : le menu est masqué "
                               "(boot direct)")), False, False, 0)
        rh, self.spin_timeout = _spin_row(_("Nombre de secondes (0 = masquer)"),
                                          0, 300, 5)
        fb.pack_start(rh, False, False, 0)
        apply_btn = _button(_("Appliquer"), self.on_boot_timeout,
                            style="suggested")
        fb.pack_start(apply_btn, False, False, 0)

        # liste des entrées systemd-boot générées
        bhdr = _label(markup="<b>" + _("Entrées de boot générées") + "</b>")
        bhdr.set_margin_top(12)
        page.pack_start(bhdr, False, False, 0)

        self.boot_store = Gtk.ListStore(str, str)  # fichier, sous-volume
        self.boot_tree = Gtk.TreeView(model=self.boot_store)
        for i, title in ((0, _("Fichier")), (1, _("Sous-volume"))):
            col = Gtk.TreeViewColumn(title, Gtk.CellRendererText(), text=i)
            col.set_expand(i == 1)
            self.boot_tree.append_column(col)
        bsw = Gtk.ScrolledWindow()
        bsw.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
        bsw.set_min_content_height(120)
        bsw.add(self.boot_tree)
        page.pack_start(bsw, True, True, 0)

    def refresh_boot(self):
        run_async(lambda: (CORE.detect_bootloader(),
                           CORE.sync_bootloader_entries(self.root, self.snapdir,
                                                        verbose=False)),
                  self._load_boot_done)

    def _load_boot_done(self, result, output):
        if output.strip():
            self.log(output.rstrip())
        if result is None:
            return
        bootloader, _unused = result
        self.lbl_boot_loader.set_text(_f("Chargeur de boot : {bl}", bl=bootloader))

        # lister les entrées btrfsmgr-*.conf (systemd-boot)
        entries = "/boot/loader/entries"
        self.boot_store.clear()
        if bootloader == "systemd-boot" and os.path.isdir(entries):
            for f in sorted(os.listdir(entries)):
                if not f.startswith("btrfsmgr-") or not f.endswith(".conf"):
                    continue
                subvol = ""
                p = os.path.join(entries, f)
                try:
                    with open(p) as fh:
                        for line in fh:
                            if line.startswith("options:"):
                                subvol = CORE._entry_subvol(p)
                                break
                except OSError:
                    pass
                self.boot_store.append([f, subvol])

    def on_boot_timeout(self, *a):
        secs = self.spin_timeout.get_value_as_int()
        if not ask_yes_no(self.win,
                          _f("Appliquer : clr-boot-manager set-timeout {s}",
                             s=secs),
                          _("Modifier le timeout du menu au redémarrage ?")):
            return
        self.set_busy(True)
        run_async(lambda: CORE.run(["clr-boot-manager", "set-timeout",
                                    str(secs)], check=False),
                  lambda r, o: self._boot_timeout_done(r, o, secs))

    def _boot_timeout_done(self, result, output, secs):
        self.set_busy(False)
        if output.strip():
            self.log(output.rstrip())
        rc = result[0] if isinstance(result, tuple) else 1
        if rc == 0:
            self.log(_f("✓ Menu masqué/affiché (timeout={s}s).", s=secs))
        else:
            self.log(_f("✗ Échec: clr-boot-manager set-timeout {s}", s=secs))

    # ------------------------------------------------------------- refresh
    def refresh_all(self, boot_sync=False):
        self.refresh_info()
        self.refresh_snapshots()
        self.refresh_timers()
        if boot_sync:
            # synchroniser les entrées systemd-boot + monter le snapdir
            try:
                CORE.ensure_snapdir(self.root, self.snapdir)
            except SystemExit as e:
                self.log(_f("Warning: ensure_snapdir ignorée (code {c}).", c=e.code))
            except Exception as e:  # noqa: BLE001
                self.log(_f("Warning: ensure_snapdir ignorée: {err}", err=e))
        self.refresh_boot()

    def refresh_info(self):
        run_async(lambda: (CORE.detect_bootloader(),
                           CORE.default_subvol(self.root),
                           CORE.list_timers(),
                           CORE.list_snapshots(self.root, self.snapdir),
                           CORE.run(["df", "-h", self.root], check=False)),
                  self._load_info_done)

    def _load_info_done(self, result, output):
        if output.strip():
            self.log(output.rstrip())
        if result is None:
            return
        bootloader, default_sv, timers, snaps, (dfrc, dfout) = result
        self.lbl_version.set_text(f"{APP} {VERSION}")
        self.lbl_bootloader.set_text(str(bootloader))
        self.lbl_default_subvol.set_text((default_sv or {}).get("full", "?"))
        self.lbl_snap_count.set_text(str(len(snaps or [])))
        self.lbl_timer_count.set_text(str(len(timers or [])))
        disk = "\n".join("  " + ln for ln in (dfout or "").splitlines())
        self.lbl_disk.set_text(disk)


# ---------------------------------------------------------------------------
# Lancement
# ---------------------------------------------------------------------------
def _parse_args(argv):
    """Analyse tolérante: [CHEMIN] [--snapdir DIR].  La valeur de --snapdir
    n'est jamais prise pour la racine."""
    root = "/"
    snapdir = DEFAULT_SNAPDIR
    positional = []
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--snapdir":
            if i + 1 < len(argv):
                snapdir = argv[i + 1]
                i += 2
                continue
            i += 1
            continue
        if a.startswith("--snapdir="):
            snapdir = a.split("=", 1)[1]
            i += 1
            continue
        if a.startswith("-"):
            i += 1
            continue
        positional.append(a)
        i += 1
    if positional:
        root = os.path.abspath(positional[0])
    return root, snapdir


def _relaunch_root() -> bool:
    """Relancer btrfsmgr-gui en root via pkexec en PRÉSERVANT l'environnement
    graphique.

    pkexec efface DISPLAY, WAYLAND_DISPLAY, XAUTHORITY, XDG_RUNTIME_DIR… par
    défaut, si bien que le processus root ne peut pas initialiser GTK
    (« Gtk couldn't be initialized »).  On les repasse explicitement via
    `pkexec env VAR=… <programme>`.

    Renvoie True si l'exéc (réussi) a été lancé — le processus est alors
    remplacé ; False si pkexec est indisponible.
    """
    pkexec = shutil.which("pkexec")
    if not pkexec:
        return False
    keys = ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "XDG_RUNTIME_DIR",
            "XDG_SESSION_TYPE", "XDG_SESSION_DESKTOP", "XDG_CURRENT_DESKTOP",
            "DBUS_SESSION_BUS_ADDRESS", "DESKTOP_STARTUP_ID")
    env_args = [f"{k}={os.environ[k]}" for k in keys if os.environ.get(k)]
    prog = os.path.abspath(sys.argv[0])
    if os.access(prog, os.X_OK):
        target = [prog]
    else:
        # script non exécutable (ex. « python3 btrfsmgr-gui.py ») : on repasse
        # par l'interpréteur courant.
        target = [sys.executable, prog]
    argv = [pkexec, "env", *env_args, *target, *sys.argv[1:]]
    sys.stderr.write("btrfsmgr-gui : relance en root via pkexec…\n")
    sys.stderr.flush()
    os.execvp(pkexec, argv)
    return False  # jamais atteint si execvp réussit


def main():
    root, snapdir = _parse_args(sys.argv[1:])

    if os.geteuid() != 0:
        if _relaunch_root():
            return 0
        sys.stderr.write("ERREUR: btrfsmgr-gui doit être lancé en root.\n")
        sys.stderr.write("  Essayez : sudo btrfsmgr-gui   (ou pkexec btrfsmgr-gui)\n")
        return 1

    if not CORE.btrfs_available():
        sys.stderr.write("ERREUR: 'btrfs' introuvable. Installez btrfs-progs.\n")
        return 1

    # Vérifier que GTK peut s'initialiser (affichage accessible) avant de
    # construire la fenêtre — évite une traceback brute en cas d'échec.
    ok, _argv = Gtk.init_check()
    if not ok:
        sys.stderr.write(
            "ERREUR: impossible d'initialiser GTK — aucun affichage "
            "accessible.\n"
            "  DISPLAY=%r WAYLAND_DISPLAY=%r XDG_RUNTIME_DIR=%r\n"
            % (os.environ.get("DISPLAY"), os.environ.get("WAYLAND_DISPLAY"),
               os.environ.get("XDG_RUNTIME_DIR")))
        sys.stderr.write(
            "  Lancez l'outil depuis une session graphique, ou vérifiez "
            "XAUTHORITY / WAYLAND_DISPLAY.\n")
        return 1

    app = BtrfsMgrApp(root, snapdir)
    Gtk.main()
    return 0


if __name__ == "__main__":
    sys.exit(main())