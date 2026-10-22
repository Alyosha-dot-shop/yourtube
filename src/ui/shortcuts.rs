//! Keyboard shortcuts. One list drives the key bindings, the shortcuts window
//! (Ctrl+? or F1) and the Keyboard page in Preferences, where every one can be
//! changed. Your changes are saved as `shortcut.<id> = …` lines in the config.

use super::App;
use crate::config::Config;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::rc::Rc;

pub struct Shortcut {
    /// Stable name used in the config file.
    pub id: &'static str,
    pub section: &'static str,
    pub title: &'static str,
    pub default: &'static [&'static str],
    /// The window action it triggers; `None` for keys on a focused video (handled by the grid).
    pub action: Option<&'static str>,
}

const fn s(
    id: &'static str,
    section: &'static str,
    title: &'static str,
    default: &'static [&'static str],
    action: Option<&'static str>,
) -> Shortcut {
    Shortcut { id, section, title, default, action }
}

pub const SHORTCUTS: &[Shortcut] = &[
    s("search", "General", "Search or filter", &["<Ctrl>f", "<Ctrl>k"], Some("win.search")),
    s("refresh", "General", "Refresh this page", &["<Ctrl>r", "F5"], Some("win.refresh")),
    s("back", "General", "Go back", &["<Alt>Left", "Escape"], Some("win.back")),
    s("preferences", "General", "Preferences", &["<Ctrl>comma"], Some("win.preferences")),
    s("shortcuts", "General", "Keyboard shortcuts", &["<Ctrl>question", "F1"], Some("win.shortcuts")),
    s("import", "General", "Import subscriptions and history", &["<Ctrl>i"], Some("win.import")),
    s("report", "General", "Report a problem (screenshot + details)", &["<Ctrl><Shift>b"], Some("win.report")),
    s("quit", "General", "Quit", &["<Ctrl>q"], Some("app.quit")),
    s("tab.foryou", "Pages", "For You", &["<Alt>1"], Some("win.tab::foryou")),
    s("tab.subscriptions", "Pages", "Subscriptions", &["<Alt>2"], Some("win.tab::subscriptions")),
    s("tab.library", "Pages", "Library", &["<Alt>3"], Some("win.tab::library")),
    s("tab.history", "Pages", "History", &["<Alt>4"], Some("win.tab::history")),
    s("next-tab", "Pages", "Next tab (or next page when there are no tabs)", &["<Ctrl>Tab", "<Ctrl>Page_Down"], Some("win.next-tab")),
    s("prev-tab", "Pages", "Previous tab (or page)", &["<Ctrl><Shift>Tab", "<Ctrl>Page_Up"], Some("win.prev-tab")),
    s("new-tab", "Pages", "New tab", &["<Ctrl>t"], Some("win.new-tab")),
    s("close-tab", "Pages", "Close tab", &["<Ctrl>w"], Some("win.close-tab")),
    s("hide-watched", "Pages", "Hide watched videos", &["<Ctrl>h"], Some("win.hide-watched")),
    s("incognito", "Privacy and profiles", "Incognito on or off", &["<Ctrl><Shift>n"], Some("win.incognito")),
    s("profiles", "Privacy and profiles", "Switch profile", &["<Ctrl><Shift>p"], Some("win.profiles-menu")),
    s("video.play", "On a video", "Play", &["Return"], None),
    s("video.menu", "On a video", "Menu", &["Menu", "<Shift>F10"], None),
    s("video.details", "On a video", "About this video: description, chapters, comments", &["i"], None),
    s("video.new_tab", "On a video", "About this video, in a new tab", &["t"], None),
    s("video.audio", "On a video", "Listen (sound only)", &["a"], None),
    s("video.download", "On a video", "Download / delete download", &["d"], None),
    s("video.watch_later", "On a video", "Save to / remove from Watch later", &["w"], None),
    s("video.playlist", "On a video", "Add to playlist", &["p"], None),
    s("video.watched", "On a video", "Mark watched / unwatched", &["x"], None),
    s("video.more", "On a video", "More like this", &["m"], None),
    s("video.not_interested", "On a video", "Not interested", &["n"], None),
    s("video.block", "On a video", "Don't show this channel", &["b"], None),
    s("video.channel", "On a video", "Go to the channel", &["c"], None),
    s("video.copy", "On a video", "Copy link", &["l"], None),
];

/// The keys bound to a shortcut now: your setting, or the default.
pub fn keys(c: &Config, sc: &Shortcut) -> Vec<String> {
    c.shortcuts.get(sc.id).cloned().unwrap_or_else(|| sc.default.iter().map(|s| s.to_string()).collect())
}

/// Which shortcut (if any) uses this key combination, other than `except`.
pub fn used_by(c: &Config, accel: &str, except: &str) -> Option<&'static Shortcut> {
    let want = parse(accel)?;
    SHORTCUTS.iter().filter(|s| s.id != except).find(|s| keys(c, s).iter().any(|k| parse(k) == Some(want)))
}

const MODS: gdk::ModifierType = gdk::ModifierType::CONTROL_MASK
    .union(gdk::ModifierType::SHIFT_MASK)
    .union(gdk::ModifierType::ALT_MASK)
    .union(gdk::ModifierType::SUPER_MASK);

fn parse(accel: &str) -> Option<(gdk::Key, gdk::ModifierType)> {
    let (key, mods) = gtk::accelerator_parse(accel)?;
    Some((key.to_lower(), mods & MODS))
}

/// The video shortcut matching a key press on a focused card.
pub fn video_action(c: &Config, key: gdk::Key, mods: gdk::ModifierType) -> Option<&'static str> {
    let pressed = (key.to_lower(), mods & MODS);
    SHORTCUTS.iter().filter(|s| s.action.is_none()).find(|s| keys(c, s).iter().any(|k| parse(k) == Some(pressed))).map(|s| s.id)
}

/// Bind every window shortcut (call again after changes).
pub fn apply(app: &App) {
    let Some(gapp) = app.window.get().and_then(|w| w.application()) else { return };
    let c = app.config();
    for sc in SHORTCUTS {
        if let Some(action) = sc.action {
            let k = keys(&c, sc);
            let refs: Vec<&str> = k.iter().map(String::as_str).collect();
            gapp.set_accels_for_action(action, &refs);
        }
    }
}

/// How a key combination looks to people ("Ctrl+Shift+N").
pub fn label(accel: &str) -> String {
    gtk::accelerator_parse(accel).map_or_else(|| accel.to_owned(), |(k, m)| gtk::accelerator_get_label(k, m).to_string())
}

pub fn install(app: &Rc<App>, window: &adw::ApplicationWindow) {
    let back = gio::ActionEntry::builder("back")
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, _| {
                if app.search_bar.is_search_mode() {
                    app.search_bar.set_search_mode(false);
                } else {
                    app.nav().pop();
                }
            }
        ))
        .build();
    let cycle = |name: &'static str, step: i32| {
        gio::ActionEntry::builder(name)
            .activate(glib::clone!(
                #[weak]
                app,
                move |_: &adw::ApplicationWindow, _, _| {
                    // With tabs open these switch tabs, like a browser; otherwise Home's pages.
                    if app.cycle_tabs(step) {
                        return;
                    }
                    let names = ["foryou", "subscriptions", "library", "history"];
                    let cur = app.stack.visible_child_name().map(|s| s.to_string()).unwrap_or_default();
                    let i = names.iter().position(|n| *n == cur).unwrap_or(0) as i32;
                    app.go_home();
                    app.stack.set_visible_child_name(names[(i + step).rem_euclid(names.len() as i32) as usize]);
                }
            ))
            .build()
    };
    let new_tab = gio::ActionEntry::builder("new-tab")
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, _| app.new_search_tab()
        ))
        .build();
    let close_tab = gio::ActionEntry::builder("close-tab")
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, _| app.close_tab()
        ))
        .build();
    window.add_action_entries([back, cycle("next-tab", 1), cycle("prev-tab", -1), new_tab, close_tab]);
    apply(app);
}

/// The read-only overview (Ctrl+?). Change them in Preferences → Keyboard.
pub fn show(app: &Rc<App>) {
    let c = app.config();
    let dialog = adw::ShortcutsDialog::new();
    let mut section: Option<(&str, adw::ShortcutsSection)> = None;
    for sc in SHORTCUTS {
        if section.as_ref().is_none_or(|(s, _)| *s != sc.section) {
            if let Some((_, s)) = section.take() {
                dialog.add(s);
            }
            section = Some((sc.section, adw::ShortcutsSection::new(Some(sc.section))));
        }
        let k = keys(&c, sc);
        if let (Some((_, s)), false) = (&section, k.is_empty()) {
            s.add(adw::ShortcutsItem::new(sc.title, &k.join(" ")));
        }
    }
    if let Some((_, s)) = section {
        dialog.add(s);
    }
    dialog.present(app.window.get());
}

/// Preferences → Keyboard: every shortcut, click to change.
pub fn prefs_page(app: &Rc<App>, edit: &Rc<std::cell::RefCell<Config>>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder().title("Keyboard").icon_name("input-keyboard-symbolic").build();
    let mut group: Option<(&str, adw::PreferencesGroup)> = None;
    let mut rows: Vec<(&'static Shortcut, adw::ActionRow, gtk::Label)> = Vec::new();
    for sc in SHORTCUTS {
        if group.as_ref().is_none_or(|(s, _)| *s != sc.section) {
            if let Some((_, g)) = group.take() {
                page.add(&g);
            }
            let g = adw::PreferencesGroup::builder().title(sc.section).build();
            if sc.section == "On a video" {
                g.set_description(Some("When a video has keyboard focus (move with the arrow keys)."));
            }
            group = Some((sc.section, g));
        }
        let row = adw::ActionRow::builder().title(sc.title).activatable(true).build();
        let keys_label = gtk::Label::builder().css_classes(["dim-label"]).valign(gtk::Align::Center).build();
        row.add_suffix(&keys_label);
        if let Some((_, g)) = &group {
            g.add(&row);
        }
        rows.push((sc, row, keys_label));
    }
    if let Some((_, g)) = group {
        page.add(&g);
    }
    let rows = Rc::new(rows);
    let refresh_labels = {
        let (rows, edit) = (rows.clone(), edit.clone());
        Rc::new(move || {
            let c = edit.borrow();
            for (sc, row, label) in rows.iter() {
                let k = keys(&c, sc);
                label.set_label(&if k.is_empty() {
                    "Off".to_owned()
                } else {
                    k.iter().map(|a| label_of(a)).collect::<Vec<_>>().join("  or  ")
                });
                row.set_subtitle(if c.shortcuts.contains_key(sc.id) { "Changed" } else { "" });
            }
        })
    };
    refresh_labels();
    for (sc, row, _) in rows.iter() {
        let sc: &'static Shortcut = sc;
        row.connect_activated(glib::clone!(
            #[weak]
            app,
            #[strong]
            edit,
            #[strong]
            refresh_labels,
            move |row| capture(&app, row, sc, &edit, refresh_labels.clone())
        ));
    }
    let reset_group = adw::PreferencesGroup::new();
    let reset = adw::ButtonRow::builder().title("Reset all shortcuts").build();
    reset.add_css_class("destructive-action");
    reset.connect_activated(glib::clone!(
        #[weak]
        app,
        #[strong]
        edit,
        #[strong]
        refresh_labels,
        move |_| {
            edit.borrow_mut().shortcuts.clear();
            commit(&app, &edit);
            refresh_labels();
        }
    ));
    reset_group.add(&reset);
    page.add(&reset_group);
    page
}

fn label_of(accel: &str) -> String {
    label(accel)
}

/// Apply edited shortcuts right away (they're saved with the rest when Preferences closes).
fn commit(app: &Rc<App>, edit: &Rc<std::cell::RefCell<Config>>) {
    app.config.borrow_mut().shortcuts = edit.borrow().shortcuts.clone();
    apply(app);
}

/// Ask for a new key combination for one shortcut.
fn capture(app: &Rc<App>, row: &adw::ActionRow, sc: &'static Shortcut, edit: &Rc<std::cell::RefCell<Config>>, done: Rc<dyn Fn()>) {
    let d =
        adw::AlertDialog::new(Some(sc.title), Some("Press the new key combination.\nBackspace turns this shortcut off, Escape cancels."));
    let shown = gtk::Label::builder().label("…").css_classes(["title-2"]).margin_top(12).build();
    d.set_extra_child(Some(&shown));
    d.add_responses(&[("default", "Restore default"), ("cancel", "Cancel")]);
    d.set_close_response("cancel");
    let keys_ctl = gtk::EventControllerKey::new();
    keys_ctl.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys_ctl.connect_key_pressed(glib::clone!(
        #[weak]
        app,
        #[weak]
        d,
        #[strong]
        edit,
        #[strong]
        done,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, mods| {
            use gdk::Key;
            let mods = mods & MODS;
            match key {
                Key::Escape if mods.is_empty() => {
                    d.close();
                }
                Key::BackSpace if mods.is_empty() => {
                    edit.borrow_mut().shortcuts.insert(sc.id.to_owned(), Vec::new());
                    commit(&app, &edit);
                    done();
                    d.close();
                }
                Key::Shift_L
                | Key::Shift_R
                | Key::Control_L
                | Key::Control_R
                | Key::Alt_L
                | Key::Alt_R
                | Key::Super_L
                | Key::Super_R
                | Key::Meta_L
                | Key::Meta_R => {}
                _ => {
                    let accel = gtk::accelerator_name(key.to_lower(), mods).to_string();
                    // A key that's taken moves here; the other shortcut loses it.
                    let clash = used_by(&edit.borrow(), &accel, sc.id).map(|o| (o.id, o.title));
                    {
                        let mut e = edit.borrow_mut();
                        if let Some((other, _)) = clash {
                            let o = SHORTCUTS.iter().find(|s| s.id == other).expect("known shortcut");
                            let left: Vec<String> = keys(&e, o).into_iter().filter(|k| parse(k) != parse(&accel)).collect();
                            e.shortcuts.insert(other.to_owned(), left);
                        }
                        e.shortcuts.insert(sc.id.to_owned(), vec![accel.clone()]);
                    }
                    commit(&app, &edit);
                    done();
                    if let Some((_, title)) = clash {
                        app.toast(&format!("{} was used by “{title}”; it moved here", label(&accel)));
                    }
                    d.close();
                }
            }
            glib::Propagation::Stop
        }
    ));
    d.add_controller(keys_ctl);
    d.connect_response(
        Some("default"),
        glib::clone!(
            #[weak]
            app,
            #[strong]
            edit,
            move |_, _| {
                edit.borrow_mut().shortcuts.remove(sc.id);
                commit(&app, &edit);
                done();
            }
        ),
    );
    d.present(Some(row));
}
