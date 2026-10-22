//! The tabs and the pages pushed on top of them.

use super::grid::{MenuEntry, VideoGrid};
use super::{App, fmt};
use crate::db::{self, Item};
use crate::foryou::remote as rec;
use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

const FEED_LIMIT: i64 = 3000;
const HISTORY_LIMIT: i64 = 3000;
const FOR_YOU_LIMIT: usize = 120;
/// Make a new built-in For You page when the last one is older than this (seconds).
const FOR_YOU_STALE: i64 = 6 * 3600;

pub struct Tabs {
    pub feed: Rc<VideoGrid>,
    pub for_you: Rc<VideoGrid>,
    /// A built-in For You refresh is running.
    for_you_busy: Cell<bool>,
    pub history: Rc<VideoGrid>,
    library: gtk::ListBox,
    data_rows: adw::PreferencesGroup,
    searches_row: adw::ActionRow,
    pub history_dirty: Cell<bool>,
    pub library_dirty: Cell<bool>,
    feed_dirty: Cell<bool>,
    for_you_dirty: Cell<bool>,
    /// Picks already reported as shown, so each is reported once per run.
    reported: RefCell<HashSet<(i64, String)>>,
}

impl Tabs {
    pub fn build(app: &Rc<App>) -> Self {
        let for_you = VideoGrid::new(
            app,
            "Nothing here yet",
            "For You learns from what you search and watch in YourTube. Press Refresh (Ctrl+R) to find videos.",
            "starred-symbolic",
        );
        for_you_menu(app, &for_you);
        app.stack.add_titled_with_icon(&for_you.widget, Some("foryou"), "For You", "starred-symbolic");

        let feed = VideoGrid::new(
            app,
            "No subscriptions yet",
            "Subscribe from any channel page, or bring yours over: menu → Import subscriptions and history (Ctrl+I). Google Takeout, NewPipe and FreeTube all work.",
            "video-display-symbolic",
        );
        app.stack.add_titled_with_icon(&feed.widget, Some("subscriptions"), "Subscriptions", "video-display-symbolic");

        let library = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        let new_btn = gtk::Button::builder().label("New playlist").halign(gtk::Align::Start).css_classes(["pill"]).build();
        new_btn.connect_clicked(glib::clone!(
            #[weak]
            app,
            move |_| {
                name_dialog(&app, "New playlist", "", "Create", move |app, name| {
                    if let Ok(c) = db::open() {
                        let _ = db::create_playlist(&c, &name);
                    }
                    app.tabs().load_library(app);
                })
            }
        ));
        let lib_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        lib_box.append(&library);
        lib_box.append(&new_btn);
        let (data_rows, searches_row) = data_group(app);
        lib_box.append(&data_rows);
        let lib_scroll = gtk::ScrolledWindow::builder()
            .child(&adw::Clamp::builder().maximum_size(720).child(&lib_box).build())
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        app.stack.add_titled_with_icon(&lib_scroll, Some("library"), "Library", "view-list-symbolic");

        let history = VideoGrid::new(
            app,
            "Nothing watched yet",
            "Videos you play show up here. Incognito keeps them out.",
            "document-open-recent-symbolic",
        );
        history.set_respect_hidden(false);
        app.stack.add_titled_with_icon(&history.widget, Some("history"), "History", "document-open-recent-symbolic");

        Tabs {
            feed,
            for_you,
            for_you_busy: Cell::new(false),
            history,
            library,
            data_rows,
            searches_row,
            history_dirty: Cell::new(true),
            library_dirty: Cell::new(true),
            feed_dirty: Cell::new(true),
            for_you_dirty: Cell::new(true),
            reported: RefCell::default(),
        }
    }

    /// Everything changed (new profile, import): reload the tab you're on now, the rest when you open them.
    /// Loading only the visible tab is what keeps start-up quick.
    pub fn load_all(&self, app: &Rc<App>) {
        for d in [&self.feed_dirty, &self.history_dirty, &self.library_dirty, &self.for_you_dirty] {
            d.set(true);
        }
        let current = app.stack.visible_child_name().map(|s| s.to_string()).unwrap_or_default();
        self.shown(app, &current);
    }

    /// A tab came into view: reload what may have changed meanwhile.
    pub fn shown(&self, app: &Rc<App>, name: &str) {
        match name {
            "history" if self.history_dirty.replace(false) => self.load_history(),
            "library" if self.library_dirty.replace(false) => self.load_library(app),
            "subscriptions" if self.feed_dirty.replace(false) => self.load_feed(),
            "foryou" if self.for_you_dirty.replace(false) => self.load_for_you(app),
            _ => {}
        }
    }

    pub fn load_feed(&self) {
        self.feed_dirty.set(false);
        self.feed.set_items(db::open().and_then(|c| db::feed(&c, FEED_LIMIT)).unwrap_or_default());
    }

    pub fn load_history(&self) {
        self.history_dirty.set(false);
        let rows = db::open().and_then(|c| db::history(&c, HISTORY_LIMIT)).unwrap_or_default();
        self.history.set_items(
            rows.into_iter()
                .map(|(mut it, at)| {
                    it.note = Some(format!("Watched {}", fmt::age(at)));
                    it
                })
                .collect(),
        );
    }

    pub fn for_you_busy(&self) -> bool {
        self.for_you_busy.get()
    }

    /// Show For You: the external recommender's picks, or the built-in page (made fresh if it's old).
    pub fn load_for_you(&self, app: &Rc<App>) {
        self.for_you_dirty.set(false);
        let grid = self.for_you.clone();
        let Some(base) = app.config().recommender.clone() else {
            let (picks, last) = db::open().and_then(|c| Ok((db::rec_picks(&c)?, db::rec_last_run(&c)?))).unwrap_or_default();
            grid.set_items(picks);
            if last.is_none_or(|t| db::now() - t > FOR_YOU_STALE) {
                self.refresh_for_you(app);
            }
            return;
        };
        grid.loading();
        App::bg(
            move || {
                let mut picks = rec::picks(&base, FOR_YOU_LIMIT).map_err(|e| e.to_string())?;
                if let Ok(c) = db::open() {
                    let _ = db::add_progress(&c, &mut picks);
                }
                Ok::<_, String>(picks)
            },
            glib::clone!(
                #[weak]
                app,
                move |r| match r {
                    Ok(picks) => grid.set_items(picks),
                    Err(e) => {
                        grid.set_empty_text(
                            "Recommender not reachable",
                            &format!("{e}\n\nIs it running at {}?", app.config().recommender.as_deref().unwrap_or("")),
                        );
                        grid.set_items(Vec::new());
                    }
                }
            ),
        );
    }

    /// Find new picks (built-in For You). With an external recommender this just reloads.
    pub fn refresh_for_you(&self, app: &Rc<App>) {
        if app.config().recommender.is_some() {
            return self.load_for_you(app);
        }
        if self.for_you_busy.replace(true) {
            return;
        }
        self.for_you.loading();
        app.set_busy(true, "Finding videos for you…");
        let cfg = app.config();
        App::bg(
            move || crate::foryou::refresh(&cfg, |_| {}).map_err(|e| e.to_string()),
            glib::clone!(
                #[weak]
                app,
                move |r: Result<crate::foryou::Stats, String>| {
                    let tabs = app.tabs();
                    tabs.for_you_busy.set(false);
                    app.set_busy(false, "");
                    match r {
                        Ok(s) => {
                            let picks = db::open().and_then(|c| db::rec_picks(&c)).unwrap_or_default();
                            tabs.for_you.set_items(picks);
                            if let Some(e) = s.ai_error {
                                app.toast(&format!("For You updated without AI: {e}"));
                            } else if s.picks == 0 {
                                tabs.for_you
                                    .set_empty_text("Nothing found yet", "Search for something or watch a few videos, then refresh.");
                            }
                        }
                        Err(e) => {
                            tabs.for_you.set_empty_text("Couldn't make For You", &e);
                            tabs.for_you.set_items(Vec::new());
                            app.problem(crate::report::Code::guess(&e), &format!("For You: {e}"));
                        }
                    }
                }
            ),
        );
    }

    pub fn load_library(&self, app: &Rc<App>) {
        self.library_dirty.set(false);
        let n: i64 = db::open().and_then(|c| Ok(c.query_row("SELECT COUNT(*) FROM searches", [], |r| r.get(0))?)).unwrap_or(0);
        self.searches_row.set_subtitle(&if n == 1 { "1 search".to_owned() } else { format!("{n} searches") });
        let _ = &self.data_rows;
        while let Some(row) = self.library.first_child() {
            self.library.remove(&row);
        }
        let lists = db::open().and_then(|c| {
            db::watch_later(&c)?; // make sure it exists
            db::playlists(&c)
        });
        let n_dl = db::open().and_then(|c| crate::download::list(&c)).map(|l| l.len()).unwrap_or(0) + app.downloads.borrow().len();
        let dl_row = adw::ActionRow::builder()
            .title("Downloads")
            .subtitle(if n_dl == 1 { "1 video".to_owned() } else { format!("{n_dl} videos") })
            .activatable(true)
            .build();
        dl_row.add_prefix(&gtk::Image::from_icon_name("folder-download-symbolic"));
        dl_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        dl_row.connect_activated(glib::clone!(
            #[weak]
            app,
            move |_| app.show(&super::browse::downloads_page(&app))
        ));
        self.library.append(&dl_row);
        for pl in lists.unwrap_or_default() {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&pl.name))
                .subtitle(if pl.count == 1 { "1 video".to_owned() } else { format!("{} videos", pl.count) })
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(if pl.name == db::WATCH_LATER { "alarm-symbolic" } else { "view-list-symbolic" }));
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            row.connect_activated(glib::clone!(
                #[weak]
                app,
                move |_| app.open_playlist(&pl)
            ));
            self.library.append(&row);
        }
    }

    /// Count For You picks as seen, once per run (picks you keep skipping sink).
    fn report_shown(&self, app: &App, items: &[(i64, String)]) {
        let mut reported = self.reported.borrow_mut();
        let fresh: Vec<(i64, String)> = items.iter().filter(|k| reported.insert((*k).clone())).cloned().collect();
        let Some(run) = fresh.first().map(|f| f.0) else { return };
        let ids: Vec<String> = fresh.into_iter().filter(|(r, _)| *r == run).map(|(_, id)| id).collect();
        let base = app.config().recommender.clone();
        std::thread::spawn(move || {
            let _ = match base {
                Some(base) => rec::impressions(&base, run, &ids),
                None => db::open().and_then(|c| db::mark_shown(&c, &ids, run)),
            };
        });
    }
}

/// For You reports which picks you saw, so ones you keep skipping sink. (Feedback is in every card's menu.)
fn for_you_menu(app: &Rc<App>, grid: &Rc<VideoGrid>) {
    let weak = Rc::downgrade(grid);
    grid.set_on_shown(glib::clone!(
        #[weak]
        app,
        move |ids: &[String]| {
            let Some(g) = weak.upgrade() else { return };
            let Some(tabs) = app.pages.get() else { return };
            // Only the first screenful or so counts as seen.
            let keyed: Vec<(i64, String)> = ids.iter().take(24).filter_map(|id| g.run_of(id).map(|r| (r, id.clone()))).collect();
            tabs.report_shown(&app, &keyed);
        }
    ));
}

fn sub_page(title: &str, subtitle: &str, content: &impl IsA<gtk::Widget>) -> (adw::NavigationPage, adw::HeaderBar, adw::WindowTitle) {
    let wt = adw::WindowTitle::new(title, subtitle);
    let header = adw::HeaderBar::builder().title_widget(&wt).build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(content));
    let page = adw::NavigationPage::builder().title(title).child(&view).build();
    (page, header, wt)
}

pub fn playlist_page(app: &Rc<App>, pl: &db::Playlist) -> adw::NavigationPage {
    let grid =
        VideoGrid::new(app, "Empty playlist", "Right-click a video anywhere (or press P) and choose a playlist.", "view-list-symbolic");
    grid.set_respect_hidden(false);
    let (page, header, title) = sub_page(&pl.name, "", &grid.widget);
    let load = {
        let (grid, id, title) = (grid.clone(), pl.id.clone(), title.clone());
        move || {
            let rows = db::open().and_then(|c| db::playlist_items(&c, &id)).unwrap_or_default();
            title.set_subtitle(&if rows.len() == 1 { "1 video".into() } else { format!("{} videos", rows.len()) });
            grid.set_items(
                rows.into_iter()
                    .map(|(mut it, added)| {
                        if added > 0 {
                            it.note = Some(format!("Added {}", fmt::age(added)));
                        }
                        it
                    })
                    .collect(),
            );
        }
    };
    load();
    let load = Rc::new(load);
    let pid = pl.id.clone();
    grid.set_extra_menu(glib::clone!(
        #[weak]
        app,
        #[strong]
        load,
        #[upgrade_or_default]
        move |it: &Item| {
            let (pid, vid, load) = (pid.clone(), it.id.clone(), load.clone());
            vec![MenuEntry::new(
                "Remove from this playlist",
                glib::clone!(
                    #[weak]
                    app,
                    move || {
                        if let Ok(c) = db::open() {
                            let _ = db::remove_from_playlist(&c, &pid, &vid);
                        }
                        load();
                        app.tabs().library_dirty.set(true);
                    }
                ),
            )]
        }
    ));

    let play_all = gtk::Button::builder().icon_name("media-playback-start-symbolic").tooltip_text("Play all").build();
    let pid_play = pl.id.clone();
    play_all.connect_clicked(glib::clone!(
        #[weak]
        app,
        move |_| {
            let mut items: Vec<Item> =
                db::open().and_then(|c| db::playlist_items(&c, &pid_play)).unwrap_or_default().into_iter().map(|(it, _)| it).collect();
            // Oldest added first, like a queue.
            items.reverse();
            if !items.is_empty() {
                app.play_items(items, false);
            }
        }
    ));
    header.pack_end(&play_all);
    if pl.name != db::WATCH_LATER {
        let rename = gtk::Button::builder().icon_name("document-edit-symbolic").tooltip_text("Rename").build();
        let (pid, old) = (pl.id.clone(), pl.name.clone());
        rename.connect_clicked(glib::clone!(
            #[weak]
            app,
            #[weak]
            title,
            #[weak]
            page,
            move |_| {
                let (pid, title, page) = (pid.clone(), title.clone(), page.clone());
                name_dialog(&app, "Rename playlist", &old, "Rename", move |app, name| {
                    if let Ok(c) = db::open() {
                        let _ = db::rename_playlist(&c, &pid, &name);
                    }
                    title.set_title(&name);
                    page.set_title(&name);
                    app.tabs().load_library(app);
                })
            }
        ));
        let delete = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Delete playlist").build();
        let (pid, name) = (pl.id.clone(), pl.name.clone());
        delete.connect_clicked(glib::clone!(
            #[weak]
            app,
            move |_| {
                let d = adw::AlertDialog::new(
                    Some(&format!("Delete “{name}”?")),
                    Some("The videos stay on YouTube; only this list is removed."),
                );
                d.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
                d.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
                let pid = pid.clone();
                d.connect_response(
                    Some("delete"),
                    glib::clone!(
                        #[weak]
                        app,
                        move |_, _| {
                            if let Ok(c) = db::open() {
                                let _ = db::delete_playlist(&c, &pid);
                            }
                            app.nav().pop();
                            app.tabs().load_library(&app);
                        }
                    ),
                );
                d.present(app.window.get());
            }
        ));
        header.pack_end(&delete);
        header.pack_end(&rename);
    }
    page
}

/// Pick a playlist for a video, or make a new one.
pub fn add_to_playlist_dialog(app: &Rc<App>, it: &Item) {
    let d = adw::AlertDialog::new(Some("Add to playlist"), Some(&it.title));
    d.add_response("close", "Close");
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let conn = db::open().ok();
    if let Some(c) = &conn {
        let _ = db::watch_later(c);
    }
    let lists = conn.as_ref().and_then(|c| db::playlists(c).ok()).unwrap_or_default();
    for pl in lists {
        let present = conn.as_ref().is_some_and(|c| db::in_playlist(c, &pl.id, &it.id).unwrap_or(false));
        let check = gtk::CheckButton::builder().active(present).valign(gtk::Align::Center).build();
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(&pl.name)).activatable_widget(&check).build();
        row.add_prefix(&check);
        let (pid, it2) = (pl.id.clone(), it.clone());
        check.connect_toggled(glib::clone!(
            #[weak]
            app,
            move |c| {
                if let Ok(conn) = db::open() {
                    let _ =
                        if c.is_active() { db::add_to_playlist(&conn, &pid, &it2) } else { db::remove_from_playlist(&conn, &pid, &it2.id) };
                }
                app.tabs().library_dirty.set(true);
            }
        ));
        list.append(&row);
    }
    let new_row = adw::ButtonRow::builder().title("New playlist…").start_icon_name("list-add-symbolic").build();
    let it2 = it.clone();
    new_row.connect_activated(glib::clone!(
        #[weak]
        app,
        #[weak]
        d,
        move |_| {
            d.close();
            let it3 = it2.clone();
            name_dialog(&app, "New playlist", "", "Create and add", move |app, name| {
                if let Ok(c) = db::open() {
                    if let Ok(id) = db::create_playlist(&c, &name) {
                        let _ = db::add_to_playlist(&c, &id, &it3);
                        app.toast(&format!("Added to {name}"));
                    }
                }
                app.tabs().load_library(app);
            });
        }
    ));
    list.append(&new_row);
    d.set_extra_child(Some(&list));
    d.present(app.window.get());
}

/// Ask for a name, then call `done` with it (trimmed, not empty).
pub fn name_dialog(app: &Rc<App>, heading: &str, initial: &str, ok: &str, done: impl Fn(&Rc<App>, String) + 'static) {
    let d = adw::AlertDialog::new(Some(heading), None);
    let entry = gtk::Entry::builder().text(initial).activates_default(true).placeholder_text("Name").build();
    d.set_extra_child(Some(&entry));
    d.add_responses(&[("cancel", "Cancel"), ("ok", ok)]);
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("ok"));
    d.connect_response(
        Some("ok"),
        glib::clone!(
            #[weak]
            app,
            #[weak]
            entry,
            move |_, _| {
                let name = entry.text().trim().to_owned();
                if !name.is_empty() {
                    done(&app, name);
                }
            }
        ),
    );
    d.present(app.window.get());
    entry.grab_focus();
}

/// Make a new profile and switch to it.
pub fn new_profile_dialog(app: &Rc<App>) {
    name_dialog(app, "New profile", "", "Create", |app, name| match db::create_profile(&name) {
        Ok(n) => {
            app.rebuild_profile_menu();
            app.switch_profile(&n);
        }
        Err(e) => app.toast(&e.to_string()),
    });
}

/// Bring subscriptions, history, searches and playlists over from Google Takeout, NewPipe or FreeTube.
pub fn import_dialog(app: &Rc<App>) {
    let d = adw::AlertDialog::new(
        Some("Bring your stuff over"),
        Some(
            "Google Takeout: on takeout.google.com choose only “YouTube and YouTube Music”, download, and pick the .zip here.\n\n\
             NewPipe: a subscriptions export or a backup .zip.\n\n\
             FreeTube: its data folder or export files.\n\n\
             Importing twice is fine; nothing gets doubled.",
        ),
    );
    d.add_responses(&[("cancel", "Cancel"), ("folder", "Choose a folder…"), ("file", "Choose a file…")]);
    let freetube = crate::import::freetube_dir();
    if freetube.join("profiles.db").exists() {
        d.add_response("freetube", "From FreeTube here");
    }
    d.set_response_appearance("file", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("file"));
    d.connect_response(
        None,
        glib::clone!(
            #[weak]
            app,
            move |_, resp| {
                let window = app.window.get().cloned();
                let chooser = gtk::FileDialog::builder().title("Import into YourTube").modal(true).build();
                let run = glib::clone!(
                    #[weak]
                    app,
                    move |path: std::path::PathBuf| run_import(&app, path)
                );
                match resp {
                    "freetube" => run(freetube.clone()),
                    "file" => chooser.open(window.as_ref(), gtk::gio::Cancellable::NONE, move |r| {
                        if let Some(p) = r.ok().and_then(|f| f.path()) {
                            run(p);
                        }
                    }),
                    "folder" => chooser.select_folder(window.as_ref(), gtk::gio::Cancellable::NONE, move |r| {
                        if let Some(p) = r.ok().and_then(|f| f.path()) {
                            run(p);
                        }
                    }),
                    _ => {}
                }
            }
        ),
    );
    d.present(app.window.get());
}

fn run_import(app: &Rc<App>, path: std::path::PathBuf) {
    app.set_busy(true, "Importing…");
    App::bg(
        move || -> Result<crate::import::Counts, String> {
            let mut c = db::open().map_err(|e| e.to_string())?;
            crate::import::import_any(&mut c, &path).map_err(|e| e.to_string())
        },
        glib::clone!(
            #[weak]
            app,
            move |r: Result<crate::import::Counts, String>| {
                app.set_busy(false, "");
                match r {
                    Ok(n) => {
                        let mut parts = Vec::new();
                        for (count, what) in [
                            (n.subscriptions, "subscriptions"),
                            (n.history, "watched videos"),
                            (n.searches, "searches"),
                            (n.playlists, "playlists"),
                        ] {
                            if count > 0 {
                                parts.push(format!("{count} {what}"));
                            }
                        }
                        app.toast(&if parts.is_empty() {
                            "Nothing new to import — you already have all of it".to_owned()
                        } else {
                            format!("Imported {}", parts.join(", "))
                        });
                        if n.skipped > 0 {
                            app.toast(&format!("{} items couldn't be imported (channels without an id, ads, removed videos)", n.skipped));
                        }
                    }
                    Err(e) => app.problem(crate::report::Code::ImportExport, &e),
                }
                app.tabs().load_all(&app);
            }
        ),
    );
}

/// Library → "Your data": look at it, take it with you, or clear it.
fn data_group(app: &Rc<App>) -> (adw::PreferencesGroup, adw::ActionRow) {
    let g = adw::PreferencesGroup::builder()
        .title("Your data")
        .description("It all lives on this computer. Look at it, take it anywhere, or clear it.")
        .build();
    let searches = adw::ActionRow::builder().title("Search history").activatable(true).build();
    searches.add_prefix(&gtk::Image::from_icon_name("system-search-symbolic"));
    searches.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    searches.connect_activated(glib::clone!(
        #[weak]
        app,
        move |_| app.show(&searches_page(&app))
    ));
    let export = adw::ActionRow::builder()
        .title("Export everything…")
        .subtitle("JSON with everything, plus CSV and OPML that other apps import")
        .activatable(true)
        .build();
    export.add_prefix(&gtk::Image::from_icon_name("document-save-symbolic"));
    export.connect_activated(glib::clone!(
        #[weak]
        app,
        move |_| {
            let chooser = gtk::FileDialog::builder().title("Export your data to…").build();
            chooser.select_folder(
                app.window.get(),
                gtk::gio::Cancellable::NONE,
                glib::clone!(
                    #[weak]
                    app,
                    move |r| {
                        let Some(folder) = r.ok().and_then(|f| f.path()) else { return };
                        App::bg(
                            move || db::open().and_then(|c| crate::export::export_all(&c, &folder)).map_err(|e| e.to_string()),
                            glib::clone!(
                                #[weak]
                                app,
                                move |r: Result<crate::export::Summary, String>| match r {
                                    Ok(s) => {
                                        app.toast(&format!(
                                            "Exported {} subscriptions, {} watched videos, {} searches and {} playlists",
                                            s.subscriptions, s.history, s.searches, s.playlists
                                        ));
                                        gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(&s.folder))).launch(
                                            app.window.get(),
                                            gtk::gio::Cancellable::NONE,
                                            |_| {},
                                        );
                                    }
                                    Err(e) => app.problem(crate::report::Code::ImportExport, &format!("Export failed: {e}")),
                                }
                            ),
                        );
                    }
                ),
            );
        }
    ));
    let import = adw::ActionRow::builder().title("Import…").subtitle("Google Takeout, NewPipe or FreeTube").activatable(true).build();
    import.add_prefix(&gtk::Image::from_icon_name("document-open-symbolic"));
    import.connect_activated(glib::clone!(
        #[weak]
        app,
        move |_| import_dialog(&app)
    ));
    let clear = adw::ActionRow::builder().title("Clear watch history…").activatable(true).build();
    clear.add_prefix(&gtk::Image::from_icon_name("user-trash-symbolic"));
    clear.connect_activated(glib::clone!(
        #[weak]
        app,
        move |_| {
            let d = adw::AlertDialog::new(
                Some("Clear your watch history?"),
                Some("Every watched video and resume point in this profile is forgotten. Export first if you want a copy."),
            );
            d.add_responses(&[("cancel", "Cancel"), ("clear", "Clear history")]);
            d.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
            d.connect_response(
                Some("clear"),
                glib::clone!(
                    #[weak]
                    app,
                    move |_, _| {
                        let n = db::open().and_then(|c| crate::export::clear_history(&c)).unwrap_or(0);
                        app.toast(&format!("Forgot {n} watched videos"));
                        app.tabs().load_all(&app);
                    }
                ),
            );
            d.present(app.window.get());
        }
    ));
    for r in [&searches, &export, &import, &clear] {
        g.add(r);
    }
    (g, searches)
}

/// Every search you made, newest first. Tap to search again; remove one or all.
fn searches_page(app: &Rc<App>) -> adw::NavigationPage {
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let rows = db::open().and_then(|c| db::recent_searches(&c, 0)).unwrap_or_default();
    let count = rows.len();
    for (q, at) in rows {
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(&q)).subtitle(fmt::age(at)).activatable(true).build();
        let del = gtk::Button::builder()
            .icon_name("edit-delete-symbolic")
            .tooltip_text("Forget this search")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let q2 = q.clone();
        del.connect_clicked(glib::clone!(
            #[weak]
            row,
            move |_| {
                if let Ok(c) = db::open() {
                    let _ = crate::export::forget_searches(&c, Some((&q2, at)));
                }
                row.set_visible(false);
            }
        ));
        row.add_suffix(&del);
        row.connect_activated(glib::clone!(
            #[weak]
            app,
            move |_| app.search_youtube(&q)
        ));
        list.append(&row);
    }
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .build();
    body.append(
        &gtk::Label::builder()
            .label("Your searches shape For You. Recent ones count the most; older ones fade but still show what you're into.")
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build(),
    );
    if count == 0 {
        body.append(&adw::StatusPage::builder().icon_name("system-search-symbolic").title("No searches yet").build());
    } else {
        body.append(&list);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&adw::Clamp::builder().maximum_size(720).child(&body).build())
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let (page, header, _title) = sub_page("Search history", &format!("{count} searches"), &scroll);
    let clear = gtk::Button::builder().label("Clear all").css_classes(["destructive-action"]).build();
    clear.connect_clicked(glib::clone!(
        #[weak]
        app,
        #[weak]
        list,
        move |_| {
            let d =
                adw::AlertDialog::new(Some("Forget every search?"), Some("For You will lean on what you watch until you search again."));
            d.add_responses(&[("cancel", "Cancel"), ("clear", "Forget all")]);
            d.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
            d.connect_response(
                Some("clear"),
                glib::clone!(
                    #[weak]
                    app,
                    #[weak]
                    list,
                    move |_, _| {
                        let n = db::open().and_then(|c| crate::export::forget_searches(&c, None)).unwrap_or(0);
                        list.set_visible(false);
                        app.toast(&format!("Forgot {n} searches"));
                        app.tabs().load_library(&app);
                    }
                ),
            );
            d.present(app.window.get());
        }
    ));
    header.pack_end(&clear);
    page
}
