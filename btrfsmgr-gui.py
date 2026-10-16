#!/usr/bin/env python3
"""
btrfsmgr-gui — Interface graphique (GTK4) pour gérer les sous-volumes BTRFS.

Réutilise la logique métier du TUI (btrfsmgr.py) sans la dupliquer :
  * Créer / lister / restaurer / supprimer des instantanés
  * Rétention (conserver les N plus récents)
  * Automatisation via timers systemd (création + suppression)
  * Menu systemd-boot (affichage au démarrage)
  * Infos système (chargeur de boot, timers, espace disque)

La sortie console des fonctions réutilisées est capturée et affichée dans
un panneau de journal, afin qu'aucune information ne soit perdue par rapport
au mode TUI.

Dépendances graphiques : python3-gobject (PyGObject), gtk4.
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
    # Charger la source DIRECTEMENT (compile/exec) plutôt que via importlib :
    # les fichiers installés par le paquet ont mtime=0, ce qui peut faire
    # passer un ancien __pycache__/*.pyc pour « valide » → le GUI exécuterait
    # alors une version périmée du cœur.  exec(compile(...)) lit toujours le
    # fichier réel.
    import types
    for p in candidates:
        if os.path.isfile(p):
            with open(p, "rb") as f:
                src = f.read()
            mod = types.ModuleType("_btrfsmgr_core")
            mod.__file__ = p
            sys.modules["_btrfsmgr_core"] = mod
            exec(compile(src, p, "exec"), mod.__dict__)
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
# GTK 4 via PyGObject
# ---------------------------------------------------------------------------
try:
    import gi
    gi.require_version("Gtk", "4.0")
    from gi.repository import Gtk, GLib, Gdk, Gio, GObject, Pango
except Exception as e:  # pragma: no cover
    sys.stderr.write(
        "ERREUR: interface graphique indisponible — installez "
        "python3-gobject et gtk4.\n  Détail: %s\n" % e)
    sys.exit(1)


# Boucle principale (GTK4 : Gtk.main()/Gtk.main_quit() ont été supprimés).
_MAIN_LOOP = None


def _quit_app(*a):
    """Quitter l'application (fermeture fenêtre / bouton Quitter)."""
    if _MAIN_LOOP is not None:
        _MAIN_LOOP.quit()


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
    # GTK4 : set_line_wrap() a été renommé set_wrap()
    lb.set_wrap(True)
    return lb


def _button(text, handler=None, sensitive=True, style=None):
    btn = Gtk.Button(label=text, sensitive=sensitive)
    if handler:
        btn.connect("clicked", handler)
    if style == "destructive":
        btn.add_css_class("destructive-action")
    elif style == "suggested":
        btn.add_css_class("suggested-action")
    return btn


def _spinner() -> Gtk.Spinner:
    sp = Gtk.Spinner()
    sp.set_visible(False)
    return sp


# ---------------------------------------------------------------------------
# Dialogues — GTK4 a supprimé Gtk.Dialog.run() : on affiche la fenêtre modale
# et on lance une boucle GLib imbriquée jusqu'à la réponse.
# ---------------------------------------------------------------------------
def _modal(parent, title, build_content, buttons, width=380):
    """Dialogue modal synchrone.

    `buttons` : liste de (libellé, valeur, style|None).  Renvoie la valeur du
    bouton cliqué, ou None si la fenêtre est fermée.
    """
    win = Gtk.Window(transient_for=parent, modal=True, title=title)
    win.set_default_size(width, -1)
    outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
    outer.set_margin_top(14)
    outer.set_margin_bottom(14)
    outer.set_margin_start(14)
    outer.set_margin_end(14)
    win.set_child(outer)

    build_content(outer)

    bar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
    bar.set_halign(Gtk.Align.END)
    bar.set_margin_top(4)
    outer.append(bar)

    loop = GLib.MainLoop()
    state = {"val": None}

    def choose(val):
        def cb(_btn):
            state["val"] = val
            loop.quit()
        return cb

    for label, val, style in buttons:
        bar.append(_button(label, choose(val), style=style))

    win.connect("close-request", lambda *_: (loop.quit(), True)[1])
    win.present()
    loop.run()
    win.destroy()
    return state["val"]


def _text_body(title, text):
    def build(box):
        box.append(_label(markup=f"<b>{title}</b>"))
        if text:
            box.append(_label(text))
    return build


def message(parent, kind, title, text):
    """Boîte d'information (le type `kind` est accepté pour compat, sans effet)."""
    _modal(parent, title, _text_body(title, text), [(_("OK"), True, None)])


def ask_yes_no(parent, title, text) -> bool:
    return _modal(parent, title, _text_body(title, text),
                  [(_("Non"), False, None), (_("Oui"), True, "suggested")],
                  width=440) is True


def ask_boot_action(parent, action: str, body: str = "") -> str | None:
    """Prévient l'utilisateur qu'une opération touche les instantanés et lui
    laisse le choix d'adapter (ou non) les entrées de démarrage systemd-boot.

    Renvoie :
      * "adapt" → adapter le menu systemd-boot après l'opération ;
      * "skip"  → effectuer l'opération sans toucher au menu de démarrage ;
      * None    → annuler (ne rien faire).
    """
    title = _("Adapter les entrées de démarrage systemd-boot ?")
    text = _f("Opération : {action}\n", action=action)
    if body:
        text += "\n" + body.rstrip() + "\n"
    text += _(
        "\nLe menu de démarrage systemd-boot peut être synchronisé avec les "
        "instantanés présents (ajout des entrées manquantes, mise à jour, "
        "suppression des entrées orphelines).\n\n"
        "• Adapter systemd-boot : mettre le menu de démarrage à jour.\n"
        "• Ne pas adapter : laisser le menu de démarrage inchangé.\n"
        "• Annuler : ne rien faire.")
    return _modal(parent, title, _text_body(title, text),
                  [(_("Annuler"), None, None),
                   (_("Ne pas adapter"), "skip", None),
                   (_("Adapter systemd-boot"), "adapt", "suggested")],
                  width=600)


def _entry_row(label_text, default="") -> tuple[Gtk.Box, Gtk.Entry]:
    box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
    lb = _label(label_text)
    entry = Gtk.Entry()
    entry.set_text(default)
    entry.set_hexpand(True)
    box.append(lb)
    box.append(entry)
    return box, entry


def _spin_row(label_text, lo, hi, default) -> tuple[Gtk.Box, Gtk.SpinButton]:
    box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
    lb = _label(label_text)
    adj = Gtk.Adjustment(value=default, lower=lo, upper=hi, step_increment=1)
    spin = Gtk.SpinButton(adjustment=adj, climb_rate=1.0, digits=0)
    box.append(lb)
    box.append(spin)
    return box, spin


# ---------------------------------------------------------------------------
# Listes (GTK4 : Gtk.TreeView/ListStore sont dépréciés → Gtk.ColumnView +
# Gio.ListStore + Gtk.SignalListItemFactory)
# ---------------------------------------------------------------------------
class _ListRow(GObject.Object):
    """Ligne de liste générique : valeurs texte indexables."""
    __gtype_name__ = "BtrfsMgrListRow"

    def __init__(self, values):
        super().__init__()
        self.values = list(values)


def _list_column(title, index, expand=False) -> Gtk.ColumnViewColumn:
    factory = Gtk.SignalListItemFactory()

    def _setup(_factory, item):
        lbl = Gtk.Label()
        lbl.set_xalign(0)
        lbl.set_ellipsize(Pango.EllipsizeMode.END)
        item.set_child(lbl)

    def _bind(_factory, item):
        row = item.get_item()
        item.get_child().set_text(str(row.values[index]))

    factory.connect("setup", _setup)
    factory.connect("bind", _bind)
    col = Gtk.ColumnViewColumn(title=title, factory=factory)
    col.set_expand(expand)
    return col


def _make_list_view(columns, store, multi=False):
    """Construit (ColumnView, SelectionModel, ScrolledWindow) autour de `store`."""
    sel = (Gtk.MultiSelection(model=store) if multi
           else Gtk.SingleSelection(model=store))
    view = Gtk.ColumnView(model=sel)
    view.set_vexpand(True)
    for title, index, expand in columns:
        view.append_column(_list_column(title, index, expand))
    sw = Gtk.ScrolledWindow()
    sw.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
    sw.set_vexpand(True)
    sw.set_child(view)
    return view, sel, sw


# ---------------------------------------------------------------------------
# Application principale
# ---------------------------------------------------------------------------
class BtrfsMgrApp:
    def __init__(self, root: str, snapdir: str = DEFAULT_SNAPDIR):
        self.root = self._validate_root(root)
        self.snapdir = snapdir
        self._busy = False

        win = Gtk.Window()
        win.set_title(_f(" {app} — Gestionnaire BTRFS", app=APP.capitalize()))
        win.set_default_size(940, 620)
        win.connect("close-request", lambda *_: _quit_app() or False)

        # ---- barre d'en-tête --------------------------------------------
        header = Gtk.HeaderBar()
        header.set_show_title_buttons(True)
        win.set_titlebar(header)

        title_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL,
                            valign=Gtk.Align.CENTER)
        t_lbl = Gtk.Label(label=_f("{app} — Gestionnaire de snapshots BTRFS",
                                   app=APP.capitalize()))
        t_lbl.add_css_class("title")
        s_lbl = Gtk.Label(label=_f("v{ver} — racine: {root}",
                                   ver=VERSION, root=self.root))
        s_lbl.add_css_class("subtitle")
        title_box.append(t_lbl)
        title_box.append(s_lbl)
        header.set_title_widget(title_box)

        refresh_btn = Gtk.Button(label=_("Actualiser"))
        refresh_btn.connect("clicked", lambda *_: self.refresh_all())
        header.pack_end(refresh_btn)

        quit_btn = Gtk.Button(label=_("Quitter"))
        quit_btn.connect("clicked", lambda *_: _quit_app())
        header.pack_end(quit_btn)

        # ---- layout principal : sidebar (gauche) + pile (droite) ----------
        self.main = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        win.set_child(self.main)

        paned = Gtk.Paned(orientation=Gtk.Orientation.HORIZONTAL)
        paned.set_vexpand(True)
        self.main.append(paned)

        # sidebar des boutons de navigation
        sidebar = _vbox(spacing=4, margin=10)
        sidebar.set_size_request(210, -1)
        sidebar_label = _label(markup="<b>" + _("Actions") + "</b>")
        sidebar.append(sidebar_label)
        paned.set_start_child(sidebar)
        paned.set_resize_start_child(False)
        paned.set_shrink_start_child(False)

        self.stack = Gtk.Stack()
        self.stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        paned.set_end_child(self.stack)

        # --- panneau de journal (bas de la fenêtre) ----------------------
        log_label = _label(markup="<b>" + _("Journal") + "</b>")
        self.main.append(log_label)
        log_sw = Gtk.ScrolledWindow()
        log_sw.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
        log_sw.set_min_content_height(120)
        self.textlog = Gtk.TextView()
        self.textlog.set_editable(False)
        self.textlog.set_monospace(True)
        self.textlog.set_wrap_mode(Gtk.WrapMode.NONE)
        log_sw.set_child(self.textlog)
        self.main.append(log_sw)
        self.logbuf = self.textlog.get_buffer()

        # barre de statut avec spinner
        statusbar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        statusbar.set_margin_start(10)
        statusbar.set_margin_end(10)
        statusbar.set_margin_bottom(6)
        self.spinner = _spinner()
        self.status = _label("")
        self.status.set_hexpand(True)
        statusbar.append(self.spinner)
        statusbar.append(self.status)
        self.main.append(statusbar)

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
            sidebar.append(nav)

        self.win = win
        win.present()
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
            mark = self.logbuf.create_mark(
                "end", self.logbuf.get_end_iter(), False)
            self.textlog.scroll_mark_onscreen(mark)
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
        card.set_child(grid)
        card.set_margin_top(6)
        page.append(card)

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
        page.append(hint)

    # ------------------------------------------------------------ snapshots
    def _build_snapshots_page(self):
        page = _vbox()
        page.set_name("snapshots")
        self.stack.add_named(page, "snapshots")

        toolbar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        page.append(toolbar)

        btn_create = _button(_("Créer un instantané"), self.on_create,
                             style="suggested")
        btn_restore = _button(_("Restaurer"), self.on_restore)
        btn_del = _button(_("Supprimer"), self.on_delete,
                          style="destructive")
        btn_retention = _button(_("Rétention (garder N)"), self.on_retention)
        for b in (btn_create, btn_restore, btn_del, btn_retention):
            toolbar.append(b)

        # colonnes : nom | génération | chemin
        self.snap_store = Gio.ListStore.new(_ListRow)
        self.snap_view, self.snap_sel, sw = _make_list_view(
            [(_("Nom"), 0, False), (_("Gen"), 1, False),
             (_("Chemin complet"), 2, True)],
            self.snap_store, multi=True)
        page.append(sw)

        hint = _label(_("Astuce : sélectionnez plusieurs lignes (Ctrl/Shift) "
                        "pour supprimer en une fois."))
        page.append(hint)

    def _snap_rows(self) -> list[tuple[str, str, str]]:
        snaps = CORE.list_snapshots(self.root, self.snapdir)
        # plus récent en premier (comme le TUI)
        rows = [(s["path"], str(s["gen"]), s["full"])
                for s in reversed(snaps)]
        return rows

    def _selected_paths(self) -> list[str]:
        out = []
        bitset = self.snap_sel.get_selection()
        for i in range(bitset.get_size()):
            out.append(self.snap_store.get_item(bitset.get_nth(i)).values[2])
        return out

    def refresh_snapshots(self):
        self.snap_store.remove_all()
        run_async(lambda: CORE.list_snapshots(self.root, self.snapdir),
                  self._load_snaps_done)

    def _load_snaps_done(self, result, output):
        if output.strip():
            self.log(output.rstrip())
        if result is None:
            return
        self.snap_store.remove_all()
        rows = [(s["path"], str(s["gen"]), s["full"])
                for s in reversed(result)]
        if not rows:
            self.log(_("Aucun snapshot."))
        for r in rows:
            self.snap_store.append(_ListRow(r))

    # -------------------------------------------------------------- create
    def on_create(self, *a):
        state = {}

        def build(box):
            r, entry = _entry_row(_("Nom du snapshot"), CORE.now_stamp())
            state["entry"] = entry
            box.append(r)

        if _modal(self.win, _("Créer un instantané"), build,
                  [(_("Annuler"), False, None),
                   (_("Créer"), True, "suggested")]) is not True:
            return
        name = state["entry"].get_text().strip()
        if not name:
            message(self.win, Gtk.MessageType.ERROR, _("Nom vide"),
                    _("Veuillez saisir un nom de snapshot."))
            return
        boot = ask_boot_action(
            self.win, _f("Créer l'instantané « {name} »", name=name))
        if boot is None:
            return
        adapt = (boot == "adapt")
        self.set_busy(True)
        run_async(
            lambda: (CORE.create_snapshot(self.root, self.snapdir, name),
                     CORE.sync_bootloader_entries(self.root, self.snapdir,
                                                  verbose=False)
                     if adapt else None),
            lambda r, o: self._create_done(r, o, name))

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
        body = _f("Le snapshot prendra la place de « {at} » par échange CoW "
                  "(deux mv).\nL'ancien sous-volume sera conservé dans "
                  "{snapdir}/.\n\nRedémarrez ensuite pour booter sur la "
                  "version restaurée.\n\nCette opération est destructive.",
                  at=at, snapdir=self.snapdir)
        # Prévenir si le noyau a changé depuis l'instantané : le noyau de
        # démarrage vit dans l'ESP (/boot), pas dans l'instantané — la racine
        # restaurée peut ne plus avoir les modules du noyau qui démarrera.
        try:
            kinfo = CORE.snapshot_kernel_info(self.root, self.snapdir, name)
        except Exception:  # noqa: BLE001
            kinfo = None
        if kinfo and kinfo.get("mismatch"):
            snap_ks = ", ".join(kinfo.get("snapshot_kernels") or []) or "?"
            body = _f(CORE.KERNEL_MISMATCH_WARNING,
                      cur=kinfo.get("current", "?"), snaps=snap_ks) \
                + "\n\n" + body
        boot = ask_boot_action(
            self.win, _f("Restaurer « {name} »", name=name), body)
        if boot is None:
            return
        adapt = (boot == "adapt")
        self.set_busy(True)
        run_async(
            lambda: (CORE.restore_snapshot(self.root, self.snapdir, name),
                     CORE.sync_bootloader_entries(self.root, self.snapdir,
                                                  verbose=False)
                     if adapt else None),
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
        boot = ask_boot_action(
            self.win, _f("Supprimer {n} snapshot(s)", n=len(selected)),
            _("Suppression DÉFINITIVE (sous-volumes) :\n\n") + confirm)
        if boot is None:
            return
        adapt = (boot == "adapt")
        self.set_busy(True)
        run_async(lambda: self._do_delete(selected, adapt),
                  lambda r, o: self._delete_done(r, o, len(selected)))

    def _do_delete(self, full_paths, adapt=True):
        results = []
        for p in full_paths:
            rc, out = CORE.delete_subvol(p)
            results.append((p, rc, out))
        if adapt:
            CORE.sync_bootloader_entries(self.root, self.snapdir,
                                         verbose=False)
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
        state = {}

        def build(box):
            r, spin = _spin_row(_("Conserver les N plus récents (0 = tous)"),
                                0, 999, 7)
            state["spin"] = spin
            box.append(r)

        if _modal(self.win, _("Rétention manuelle"), build,
                  [(_("Annuler"), False, None),
                   (_("OK"), True, "suggested")]) is not True:
            return
        keep = state["spin"].get_value_as_int()
        snaps = CORE.list_snapshots(self.root, self.snapdir)
        to_del = len(snaps) - keep if keep > 0 else 0
        if to_del <= 0:
            message(self.win, Gtk.MessageType.INFO,
                    _("Rien à supprimer"),
                    _("Aucun snapshot à supprimer (déjà ≤ N)."))
            return
        boot = ask_boot_action(
            self.win, _f("Rétention : garder {keep}", keep=keep),
            (_("Garder les %d plus récents, supprimer les autres.") % keep)
            + "\n" + _f("Supprimer {n} snapshot(s)", n=to_del))
        if boot is None:
            return
        adapt = (boot == "adapt")
        self.set_busy(True)
        run_async(
            lambda: (CORE.prune_snapshots(self.root, self.snapdir, keep),
                     CORE.sync_bootloader_entries(self.root, self.snapdir,
                                                  verbose=False)
                     if adapt else None),
            lambda r, o: self._retention_done(r, o, keep))

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
        form.set_child(fb)
        page.append(form)

        rtag, self.e_tag = _entry_row(_("Nom du plan (ex. daily, weekly)"),
                                      "daily")
        fb.append(rtag)

        # fréquence
        fb.append(_label(markup="<b>" + _("Fréquence") + "</b>"))
        self.freq = Gtk.DropDown.new_from_strings(
            [_("Chaque jour"), _("Chaque semaine"), _("Chaque mois")])
        self.freq.set_selected(0)
        self.freq.connect("notify::selected",
                          lambda *_: self._sync_day_visibility())
        fb.append(self.freq)

        # jour de la semaine (visible si hebdomadaire)
        rday, self.day_combo = _entry_row(_("Jour de la semaine (Mon..Sun)"),
                                          "Mon")
        fb.append(rday)
        self._freq_box = self.freq
        self._day_row = rday
        rday.set_visible(False)

        # heure
        rhour, self.spin_hour = _spin_row(_("Heure (0-23)"), 0, 23, 3)
        fb.append(rhour)

        # au démarrage
        rb_boot, self.chk_boot, self.spin_delay = self._boot_options()
        fb.append(rb_boot)

        # rétention
        rkeep, self.spin_keep = _spin_row(_("Snapshots à conserver"), 0, 999, 7)
        fb.append(rkeep)

        create_btn = _button(_("Créer le timer systemd"), self.on_schedule_create,
                             style="suggested")
        fb.append(create_btn)

        # --- liste des timers existants ------------------------------------
        hdr = _label(markup="<b>" + _("Automatisations existantes") + "</b>")
        hdr.set_margin_top(12)
        page.append(hdr)

        self.timer_store = Gio.ListStore.new(_ListRow)
        self.timer_view, self.timer_sel, tsw = _make_list_view(
            [(_("Unité"), 0, True), (_("Prochaine exécution"), 1, False),
             (_("Dernière"), 2, False), (_("Activée"), 3, False)],
            self.timer_store)
        tsw.set_min_content_height(160)
        page.append(tsw)

        del_btn = _button(_("Supprimer l'automatisation sélectionnée"),
                          self.on_timer_delete, style="destructive")
        page.append(del_btn)

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
        box.append(chk)
        box.append(r)
        return box, chk, spin

    def _sync_day_visibility(self, *a):
        active = self.freq.get_selected()
        self._day_row.set_visible(active == 1)

    def on_schedule_create(self, *a):
        tag = self.e_tag.get_text().strip() or "daily"
        freq = self.freq.get_selected()        # 0=day 1=week 2=month
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
                state = {}

                def build(box):
                    r, spd = _spin_row(
                        _("Numéro du jour dans le mois (1-31)"), 1, 31, 1)
                    state["spin"] = spd
                    box.append(r)

                if _modal(self.win, _("Jour du mois"), build,
                          [(_("Annuler"), False, None),
                           (_("OK"), True, "suggested")]) is not True:
                    return
                daynum = state["spin"].get_value_as_int()
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
        self.timer_store.remove_all()
        run_async(CORE.list_timers, self._load_timers_done)

    def _load_timers_done(self, result, output):
        if result is None:
            return
        self.timer_store.remove_all()
        for t in result:
            self.timer_store.append(_ListRow([t["timer"], t["next"], t["last"],
                                              t["activated"]]))

    def on_timer_delete(self, *a):
        idx = self.timer_sel.get_selected()
        if idx == Gtk.INVALID_LIST_POSITION:
            message(self.win, Gtk.MessageType.INFO, _("Rien à supprimer"),
                    _("Sélectionnez une automatisation dans la liste."))
            return
        timer = self.timer_store.get_item(idx).values[0]
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
        page.append(info)

        self.lbl_boot_loader = _label(selectable=True)
        page.append(self.lbl_boot_loader)

        frame = Gtk.Frame(label=_("Menu au démarrage"))
        fb = _vbox(spacing=8, margin=8)
        frame.set_child(fb)
        page.append(frame)

        fb.append(_label(_("N > 0 : le menu est affiché pendant N "
                           "secondes\n0     : le menu est masqué "
                           "(boot direct)")))
        rh, self.spin_timeout = _spin_row(_("Nombre de secondes (0 = masquer)"),
                                          0, 300, 5)
        fb.append(rh)
        apply_btn = _button(_("Appliquer"), self.on_boot_timeout,
                            style="suggested")
        fb.append(apply_btn)

        # liste des entrées systemd-boot générées
        bhdr = _label(markup="<b>" + _("Entrées de boot générées") + "</b>")
        bhdr.set_margin_top(12)
        page.append(bhdr)

        self.boot_store = Gio.ListStore.new(_ListRow)
        self.boot_view, self.boot_sel, bsw = _make_list_view(
            [(_("Fichier"), 0, False), (_("Sous-volume"), 1, True)],
            self.boot_store)
        bsw.set_min_content_height(120)
        page.append(bsw)

        # synchronisation explicite du menu systemd-boot (jamais implicite :
        # l'utilisateur décide quand le menu de démarrage est modifié).
        adapt_btn = _button(_("Adapter les entrées systemd-boot"),
                            self.on_boot_adapt, style="suggested")
        page.append(adapt_btn)

    def on_boot_adapt(self, *a):
        if not ask_yes_no(
                self.win, _("Adapter les entrées de démarrage systemd-boot ?"),
                _("Ajouter les entrées manquantes, mettre à jour, supprimer "
                  "les entrées orphelines.")):
            return
        self.set_busy(True)
        run_async(lambda: CORE.sync_bootloader_entries(self.root, self.snapdir,
                                                       verbose=False),
                  self._boot_adapt_done)

    def _boot_adapt_done(self, result, output):
        self.set_busy(False)
        if output.strip():
            self.log(output.rstrip())
        self.log(_("✓ Entrées systemd-boot synchronisées."))
        self.refresh_boot()

    def refresh_boot(self):
        # On ne synchronise PAS ici (aucune modification implicite du menu de
        # démarrage) : on se contente de détecter le chargeur et de lister les
        # entrées déjà présentes.  La synchronisation se fait sur demande
        # explicite (bouton « Adapter les entrées systemd-boot ») ou après
        # une opération où l'utilisateur a choisi de l'adapter.
        run_async(self._detect_boot, self._load_boot_done)

    @staticmethod
    def _detect_boot():
        """Monter /boot puis détecter le chargeur (sinon « Inconnu » sur Solus)."""
        CORE.ensure_boot_mounted()
        return CORE.detect_bootloader()

    def _load_boot_done(self, result, output):
        if output.strip():
            self.log(output.rstrip())
        bootloader = result or "unknown"
        self.lbl_boot_loader.set_text(
            _f("Chargeur de boot : {bl}", bl=CORE.bootloader_display(bootloader)))

        # lister les entrées btrfsmgr-*.conf (systemd-boot)
        entries = "/boot/loader/entries"
        self.boot_store.remove_all()
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
                self.boot_store.append(_ListRow([f, subvol]))

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
        run_async(lambda: (self._detect_boot(),
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
        self.lbl_bootloader.set_text(CORE.bootloader_display(bootloader))
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
    défaut, si bien que le processus root ne peut pas initialiser GTK.  On les
    repasse explicitement via `pkexec env VAR=… <programme>`.

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


def _display_available() -> bool:
    """Vrai si un affichage est accessible à GTK4.

    En GTK4, Gtk.init_check() renvoie True même sans affichage ; la source
    fiable est Gdk.Display.get_default() (None si aucun affichage).
    """
    try:
        Gtk.init_check()
    except Exception:
        pass
    try:
        return Gdk.Display.get_default() is not None
    except Exception:
        return False


def main():
    global _MAIN_LOOP
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

    if not _display_available():
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

    _MAIN_LOOP = GLib.MainLoop()
    BtrfsMgrApp(root, snapdir)
    _MAIN_LOOP.run()
    return 0


if __name__ == "__main__":
    sys.exit(main())
