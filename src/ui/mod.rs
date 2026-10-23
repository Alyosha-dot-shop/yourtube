//! The window: For You, Subscriptions, Library and History tabs, with channel,
//! playlist and search pages that slide in on top.

mod browse;
pub mod fmt;
mod grid;
mod pages;
mod prefs;
mod report;
mod shortcuts;
mod theme;
mod video_page;

use crate::config::Config;
use crate::db::{self, Item};
use crate::foryou::remote as rec;
use crate::{feed, player, thumbs};
use adw::prelude::*;
use grid::{MenuEntry, VideoGrid};
use gtk::{gio, glib};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

const APP_ID: &str = "io.github.yourtube.YourTube";

pub enum Msg {
    Thumb(String),
    Refresh(feed::Progress),
    RefreshFailed(String),
    Player(player::Event),
    Download(String, crate::download::Update),
}

/// A download in progress, as shown on the Downloads page.
pub struct Active {
    pub title: String,
    pub status: String,
    pub fraction: f64,
    pub handle: Option<crate::download::Handle>,
}

pub struct App {
    config: RefCell<Config>,
    tx: async_channel::Sender<Msg>,
    pub loader: thumbs::Loader,
    /// Every picture currently showing a card, with the video it shows, so a finished thumbnail lands in place.
    pub pictures: RefCell<HashMap<gtk::Picture, String>>,
    /// Every grid, so progress changes show everywhere at once.
    pub grids: RefCell<Vec<Weak<VideoGrid>>>,
    pub hide_watched: RefCell<bool>,
    /// Grid columns for the current window width.
    pub columns: Cell<u32>,
    sizes_css: gtk::CssProvider,
    query: RefCell<String>,
    refreshing: Cell<bool>,
    window: OnceCell<adw::ApplicationWindow>,
    /// The Home tab: For You, Subscriptions, Library, History and whatever was opened from them.
    home: adw::NavigationView,
    /// Browser-style tabs. Home is the first, pinned; every other tab has its own NavigationView.
    tab_view: adw::TabView,
    toasts: adw::ToastOverlay,
    stack: adw::ViewStack,
    refresh_btn: gtk::Button,
    search_bar: gtk::SearchBar,
    search: gtk::SearchEntry,
    pages: OnceCell<pages::Tabs>,
    theme: OnceCell<theme::Theme>,
    /// Videos marked "not interested" and hidden channels (current profile).
    hidden: RefCell<HashSet<String>>,
    blocked: RefCell<HashSet<String>>,
    banner: adw::Banner,
    avatar: adw::Avatar,
    profile_btn: gtk::MenuButton,
    /// Set while [`App::in_new_tab`] runs: the next page opens in a new tab.
    new_tab: Cell<bool>,
    /// Downloads running or waiting, by video id.
    pub downloads: RefCell<HashMap<String, Active>>,
    /// Called when a download changes, while the Downloads page is open.
    pub on_downloads: RefCell<Option<Rc<dyn Fn()>>>,
}

thread_local! {
    static APP: OnceCell<Rc<App>> = const { OnceCell::new() };
}

/// Open the window. `args` may hold video links to play right away.
pub fn run(args: &[String]) {
    let gapp = adw::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::HANDLES_OPEN).build();
    gapp.connect_activate(|gapp| {
        app_for(gapp).present();
    });
    // yourtube://watch?v=ID, YouTube links and bare ids open straight in mpv.
    gapp.connect_open(|gapp, files, _| {
        let app = app_for(gapp);
        app.present();
        for f in files {
            match video_id_from(&f.uri()) {
                Some(id) => app.play_id(&id),
                None => app.toast(&format!("Not a YouTube video link: {}", f.uri())),
            }
        }
    });
    // The rest of the shortcuts are set in shortcuts.rs.
    gapp.set_accels_for_action("window.close", &["<Ctrl>w"]);
    let quit = gio::ActionEntry::builder("quit").activate(|a: &adw::Application, _, _| a.quit()).build();
    gapp.add_action_entries([quit]);
    let args: Vec<String> = std::env::args().take(1).chain(args.iter().filter(|a| video_id_from(a).is_some()).cloned()).collect();
    gapp.run_with_args(&args);
}

/// The one App for this process, built on first use.
fn app_for(gapp: &adw::Application) -> Rc<App> {
    APP.with(|cell| cell.get_or_init(|| build(gapp)).clone())
}

impl App {
    pub fn config(&self) -> Config {
        self.config.borrow().clone()
    }

    /// New settings from Preferences: save them and reload what depends on them.
    pub fn set_config(self: &Rc<Self>, c: Config) {
        let source_changed = c.recommender != self.config.borrow().recommender;
        if let Err(e) = crate::config::save(&c) {
            self.toast(&format!("Couldn't save settings: {e}"));
        }
        *self.config.borrow_mut() = c;
        self.apply_appearance();
        shortcuts::apply(self);
        if source_changed {
            self.tabs().load_for_you(self);
        }
    }

    /// Spinner on the refresh button while something is loading.
    pub fn set_busy(&self, on: bool, what: &str) {
        if on {
            self.refresh_btn.add_css_class("yt-busy");
            self.refresh_btn.set_tooltip_text(Some(what));
        } else if !self.refreshing.get() && !self.tabs().for_you_busy() {
            self.refresh_btn.remove_css_class("yt-busy");
            self.refresh_btn.set_tooltip_text(Some("Refresh (Ctrl+R)"));
        }
    }

    fn present(&self) {
        if let Some(w) = self.window.get() {
            w.present();
        }
    }

    pub fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::builder().title(glib::markup_escape_text(text)).timeout(4).build());
    }

    fn tabs(&self) -> &pages::Tabs {
        self.pages.get().expect("pages are built")
    }

    /// Run `work` on a worker thread, then `done` with its result on the main thread.
    pub fn bg<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
        glib::spawn_future_local(async move {
            if let Ok(v) = gio::spawn_blocking(work).await {
                done(v);
            }
        });
    }

    pub fn play(self: &Rc<Self>, it: &Item) {
        if let Some(run) = it.run_id {
            // A For You pick was opened: that's the strongest "yes".
            let (id, base) = (it.id.clone(), self.config().recommender);
            std::thread::spawn(move || {
                let _ = match base {
                    Some(base) => rec::click(&base, run, &id),
                    None => db::open().and_then(|c| db::mark_clicked(&c, &id)),
                };
            });
        }
        self.start_player(&it.id, &it.title);
    }

    pub fn play_id(self: &Rc<Self>, id: &str) {
        let title = db::open()
            .ok()
            .and_then(|c| {
                c.query_row("SELECT title FROM videos WHERE id = ?1 UNION SELECT title FROM history WHERE video_id = ?1", [id], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
            })
            .unwrap_or_else(|| id.to_owned());
        self.start_player(id, &title);
    }

    fn start_player(&self, id: &str, title: &str) {
        let it = Item { id: id.to_owned(), title: title.to_owned(), ..Default::default() };
        self.play_items(vec![it], false);
    }

    /// Play one or more videos in one mpv window (a queue), optionally sound only.
    pub fn play_items(&self, items: Vec<Item>, audio_only: bool) {
        let Some(first) = items.first() else { return };
        let what = if items.len() > 1 { format!("{} videos", items.len()) } else { format!("“{}”", first.title) };
        let how = if audio_only { "Listening to" } else { "Opening" };
        self.toasts.add_toast(adw::Toast::builder().title(glib::markup_escape_text(&format!("{how} {what} in mpv"))).timeout(2).build());
        let mut opts = player::Options::from_config(&self.config());
        opts.audio_only = audio_only;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let etx = tx.clone();
            if let Err(e) = player::play_queue(&items, &opts, move |ev| {
                let _ = etx.send_blocking(Msg::Player(ev));
            }) {
                let _ = tx.send_blocking(Msg::RefreshFailed(format!("could not start mpv: {e}")));
            }
        });
    }

    /// The page about a video: description, chapters, comments, related.
    pub fn open_video(self: &Rc<Self>, it: &Item) {
        self.show(&video_page::page(self, it));
    }

    /// The navigation of the tab you're looking at.
    pub fn nav(&self) -> adw::NavigationView {
        self.tab_view.selected_page().and_then(|p| p.child().downcast().ok()).unwrap_or_else(|| self.home.clone())
    }

    /// Open a page in the current tab, or in a new one while [`App::in_new_tab`] runs.
    pub fn show(&self, page: &adw::NavigationPage) {
        if self.new_tab.get() {
            self.open_tab(page);
        } else {
            self.nav().push(page);
        }
    }

    /// Run `f` (which opens a page) so that the page lands in a new tab.
    pub fn in_new_tab(self: &Rc<Self>, f: impl FnOnce(&Rc<App>)) {
        self.new_tab.set(true);
        f(self);
        self.new_tab.set(false);
    }

    /// A new tab starting at `page`. Its title follows whatever page the tab shows.
    pub fn open_tab(&self, page: &adw::NavigationPage) {
        self.new_tab.set(false);
        let nav = adw::NavigationView::new();
        nav.add(page);
        let tab = self.tab_view.append(&nav);
        let follow = |p: &adw::NavigationPage, tab: &adw::TabPage| p.bind_property("title", tab, "title").sync_create().build();
        let binding = Rc::new(RefCell::new(follow(page, &tab)));
        nav.connect_visible_page_notify(glib::clone!(
            #[weak]
            tab,
            move |nav| {
                if let Some(p) = nav.visible_page() {
                    binding.borrow().unbind();
                    *binding.borrow_mut() = follow(&p, &tab);
                }
            }
        ));
        self.tab_view.set_selected_page(&tab);
    }

    /// Ctrl+T: a new tab with a search box.
    pub fn new_search_tab(self: &Rc<Self>) {
        let entry = gtk::SearchEntry::builder().placeholder_text("Search YouTube or paste a link").width_request(360).build();
        let hint = gtk::Label::builder().label("Results open in this tab.").css_classes(["dim-label"]).build();
        let col = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        col.append(&entry);
        col.append(&hint);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&col));
        let page = adw::NavigationPage::builder().title("New tab").child(&view).build();
        self.open_tab(&page);
        entry.connect_activate(glib::clone!(
            #[weak(rename_to = app)]
            self,
            #[weak]
            page,
            move |e| {
                if let Some(results) = app.search_page_for(&e.text()) {
                    // Replace the search box, so Back doesn't return to an empty tab.
                    if let Some(nav) = page.parent().and_downcast::<adw::NavigationView>() {
                        nav.replace(&[results]);
                    }
                }
            }
        ));
        glib::idle_add_local_once(move || {
            entry.grab_focus();
        });
    }

    /// Close the tab you're looking at (never Home).
    pub fn close_tab(&self) {
        if let Some(p) = self.tab_view.selected_page().filter(|p| !p.is_pinned()) {
            self.tab_view.close_page(&p);
        }
    }

    /// Back to the Home tab's main page.
    pub fn go_home(&self) {
        if let Some(home) = (0..self.tab_view.n_pages()).map(|i| self.tab_view.nth_page(i)).find(|p| p.child() == self.home) {
            self.tab_view.set_selected_page(&home);
        }
        self.home.pop_to_tag("main");
    }

    /// Next or previous tab; false when there's only Home.
    pub fn cycle_tabs(&self, step: i32) -> bool {
        let n = self.tab_view.n_pages();
        let Some(cur) = self.tab_view.selected_page().filter(|_| n > 1) else { return false };
        let i = (self.tab_view.page_position(&cur) + step).rem_euclid(n);
        self.tab_view.set_selected_page(&self.tab_view.nth_page(i));
        true
    }

    /// Play from a given second (a chapter).
    pub fn play_from(&self, it: &Item, at: f64) {
        let mut opts = player::Options::from_config(&self.config());
        opts.start_at = Some(at);
        let (tx, items) = (self.tx.clone(), vec![it.clone()]);
        std::thread::spawn(move || {
            let etx = tx.clone();
            if let Err(e) = player::play_queue(&items, &opts, move |ev| {
                let _ = etx.send_blocking(Msg::Player(ev));
            }) {
                let _ = tx.send_blocking(Msg::RefreshFailed(format!("could not start mpv: {e}")));
            }
        });
    }

    pub fn listen(self: &Rc<Self>, it: &Item) {
        self.play_items(vec![it.clone()], true);
    }

    /// Download a video (or delete the copy if it's already downloaded).
    pub fn toggle_download(self: &Rc<Self>, it: &Item) {
        if let Some(a) = self.downloads.borrow().get(&it.id) {
            if let Some(h) = &a.handle {
                h.cancel();
            }
            return;
        }
        if let Ok(Some(_)) = db::open().and_then(|c| crate::download::get(&c, &it.id)) {
            let _ = db::open().and_then(|c| crate::download::delete(&c, &it.id));
            self.toast("Download deleted");
            self.downloads_changed();
            return;
        }
        let tx = self.tx.clone();
        let id = it.id.clone();
        match crate::download::start(it, crate::download::Options::from_config(&self.config()), move |u| {
            let _ = tx.send_blocking(Msg::Download(id.clone(), u));
        }) {
            Ok(h) => {
                self.downloads
                    .borrow_mut()
                    .insert(it.id.clone(), Active { title: it.title.clone(), status: "Waiting…".into(), fraction: 0.0, handle: Some(h) });
                self.toast(&format!("Downloading “{}” — see Library → Downloads", it.title));
                self.downloads_changed();
            }
            Err(e) => self.problem(crate::report::Code::Download, &format!("Couldn't start the download: {e}")),
        }
    }

    fn downloads_changed(&self) {
        let f = self.on_downloads.borrow().clone();
        if let Some(f) = f {
            f();
        }
        self.tabs().library_dirty.set(true);
    }

    /// A video's progress or watched state changed: update it on every page.
    pub fn changed(&self, id: &str) {
        let saved = db::open().ok().and_then(|c| db::position(&c, id).ok()).flatten();
        let (pos, len) = saved.map_or((None, None), |(p, l)| (Some(p), l));
        self.grids.borrow_mut().retain(|g| g.strong_count() > 0);
        for g in self.grids.borrow().iter().filter_map(Weak::upgrade) {
            g.update_progress(id, pos, len);
        }
        self.tabs().history_dirty.set(true);
    }

    /// Menu entries every card has. Each one has a key when the card is focused (see shortcuts.rs).
    pub fn common_menu(self: &Rc<Self>, it: &Item) -> Vec<MenuEntry> {
        let conn = db::open().ok();
        let in_wl =
            conn.as_ref().and_then(|c| db::watch_later(c).ok().map(|w| db::in_playlist(c, &w, &it.id).unwrap_or(false))).unwrap_or(false);
        let entry = |label: String, f: fn(&Rc<App>, &Item)| {
            let (app, it) = (Rc::downgrade(self), it.clone());
            MenuEntry::new(label, move || {
                if let Some(app) = app.upgrade() {
                    f(&app, &it)
                }
            })
        };
        let downloaded = self.downloads.borrow().contains_key(&it.id)
            || conn.as_ref().is_some_and(|c| matches!(crate::download::get(c, &it.id), Ok(Some(_))));
        let mut out = vec![
            entry("About this video (I)".into(), App::open_video),
            entry("Open in a new tab (T)".into(), |app, it| app.in_new_tab(|a| a.open_video(it))),
            entry("Listen (sound only) (A)".into(), App::listen),
            entry(
                if self.downloads.borrow().contains_key(&it.id) {
                    "Cancel download (D)"
                } else if downloaded {
                    "Delete download (D)"
                } else {
                    "Download (D)"
                }
                .into(),
                App::toggle_download,
            ),
            entry(if in_wl { "Remove from Watch later (W)" } else { "Save to Watch later (W)" }.into(), App::toggle_watch_later),
            entry("Add to playlist… (P)".into(), |app, it| pages::add_to_playlist_dialog(app, it)),
            entry(if it.watched { "Mark as unwatched (X)" } else { "Mark as watched (X)" }.into(), App::toggle_watched),
            entry("More like this (M)".into(), App::more_like_this),
            entry("Not interested (N)".into(), App::not_interested),
        ];
        if !it.channel_id.is_empty() {
            let name = if it.channel.is_empty() { "this channel".to_owned() } else { it.channel.clone() };
            out.push(entry(format!("Don't show {name} (B)"), App::block_channel_of));
            out.push(entry(format!("Go to {name} (C)"), |app, it| app.open_channel(&it.channel_id, &it.channel)));
        }
        out.push(entry("Copy link (L)".into(), App::copy_link));
        out
    }

    pub fn toggle_watched(self: &Rc<Self>, it: &Item) {
        if let Ok(c) = db::open() {
            let _ = db::set_watched(&c, &it.id, !it.watched);
        }
        self.changed(&it.id);
    }

    pub fn toggle_watch_later(self: &Rc<Self>, it: &Item) {
        let Ok(c) = db::open() else { return };
        let Ok(wl) = db::watch_later(&c) else { return };
        let saved = db::in_playlist(&c, &wl, &it.id).unwrap_or(false);
        let r = if saved { db::remove_from_playlist(&c, &wl, &it.id) } else { db::add_to_playlist(&c, &wl, it) };
        match r {
            Ok(()) => self.toast(if saved { "Removed from Watch later" } else { "Saved to Watch later" }),
            Err(e) => self.toast(&format!("Couldn't change Watch later: {e}")),
        }
        self.tabs().library_dirty.set(true);
    }

    pub fn copy_link(self: &Rc<Self>, it: &Item) {
        if let Some(w) = self.window.get() {
            w.clipboard().set_text(&format!("https://www.youtube.com/watch?v={}", it.id));
        }
        self.toast("Link copied");
    }

    /// Feedback for For You. Saved locally, and sent to an external recommender if you use one.
    fn feedback(self: &Rc<Self>, it: &Item, kind: rec::Feedback) -> bool {
        let saved = db::open().and_then(|c| db::add_feedback(&c, it, kind.kind()));
        if let Some(base) = self.config().recommender {
            let id = it.id.clone();
            std::thread::spawn(move || {
                let _ = rec::feedback(&base, &id, kind);
            });
        }
        if let Err(e) = &saved {
            self.toast(&format!("Couldn't save that: {e}"));
        }
        saved.is_ok()
    }

    pub fn more_like_this(self: &Rc<Self>, it: &Item) {
        if self.feedback(it, rec::Feedback::More) {
            self.toast("Got it — more like this on For You");
        }
    }

    pub fn not_interested(self: &Rc<Self>, it: &Item) {
        if !self.feedback(it, rec::Feedback::NotInterested) {
            return;
        }
        self.hidden.borrow_mut().insert(it.id.clone());
        self.apply_filter();
        let id = it.id.clone();
        self.toast_undo("Hidden. You'll see less like this", move |app| {
            if let Ok(c) = db::open() {
                let _ = db::remove_feedback(&c, &id, "not_interested");
            }
            app.hidden.borrow_mut().remove(&id);
            app.apply_filter();
        });
    }

    /// Hide a channel everywhere and unsubscribe (with Undo).
    pub fn block_channel_of(self: &Rc<Self>, it: &Item) {
        if it.channel_id.is_empty() {
            return;
        }
        let Ok(c) = db::open() else { return };
        let was_subscribed = db::is_subscribed(&c, &it.channel_id).unwrap_or(false);
        if let Err(e) = db::block_channel(&c, &it.channel_id, &it.channel) {
            return self.toast(&format!("Couldn't hide the channel: {e}"));
        }
        self.blocked.borrow_mut().insert(it.channel_id.clone());
        self.apply_filter();
        self.tabs().load_feed();
        let (cid, name) = (it.channel_id.clone(), it.channel.clone());
        let label = if name.is_empty() { "Channel hidden".to_owned() } else { format!("{name} won't show up anymore") };
        self.toast_undo(&label, move |app| app.unblock_channel(&cid, &name, was_subscribed));
    }

    pub fn unblock_channel(self: &Rc<Self>, channel_id: &str, name: &str, resubscribe: bool) {
        if let Ok(c) = db::open() {
            let _ = db::unblock_channel(&c, channel_id);
            if resubscribe {
                let _ = db::set_subscribed(&c, channel_id, name, true);
            }
        }
        self.blocked.borrow_mut().remove(channel_id);
        self.apply_filter();
        self.tabs().load_feed();
    }

    /// Show a problem: a toast with its error code and a Report button, remembered for bug reports.
    pub fn problem(self: &Rc<Self>, code: crate::report::Code, message: &str) {
        crate::report::record(code, message);
        let t = adw::Toast::builder()
            .title(glib::markup_escape_text(&format!("{message} [{}]", code.id())))
            .button_label("Report")
            .timeout(8)
            .build();
        let app = Rc::downgrade(self);
        t.connect_button_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                report::show(&app);
            }
        });
        self.toasts.add_toast(t);
    }

    /// A toast with an Undo button.
    pub fn toast_undo(self: &Rc<Self>, text: &str, undo: impl Fn(&Rc<App>) + 'static) {
        let t = adw::Toast::builder().title(glib::markup_escape_text(text)).button_label("Undo").timeout(6).build();
        let app = Rc::downgrade(self);
        t.connect_button_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                undo(&app);
            }
        });
        self.toasts.add_toast(t);
    }

    /// Load the not-interested videos and hidden channels for the current profile.
    fn reload_hidden(&self) {
        let c = db::open().ok();
        *self.hidden.borrow_mut() = c.as_ref().and_then(|c| db::rejected_videos(c).ok()).unwrap_or_default().into_iter().collect();
        *self.blocked.borrow_mut() =
            c.as_ref().and_then(|c| db::blocked_channels(c).ok()).unwrap_or_default().into_iter().map(|(id, _)| id).collect();
    }

    /// Whether a card may be shown on pages that hide rejected videos and hidden channels.
    pub fn allowed(&self, it: &Item) -> bool {
        !self.hidden.borrow().contains(&it.id) && !self.blocked.borrow().contains(&it.channel_id)
    }

    /// Switch to another profile: everything reloads from its own database.
    pub fn switch_profile(self: &Rc<Self>, name: &str) {
        if name == db::profile_name() {
            return;
        }
        db::set_profile(Some(name.to_owned()));
        let mut c = self.config();
        c.profile = (name != db::DEFAULT_PROFILE).then(|| name.to_owned());
        let _ = crate::config::save(&c);
        *self.config.borrow_mut() = c;
        // Other tabs show the old profile's data.
        for p in (0..self.tab_view.n_pages()).rev().map(|i| self.tab_view.nth_page(i)).filter(|p| !p.is_pinned()) {
            self.tab_view.close_page(&p);
        }
        self.go_home();
        self.reload_hidden();
        self.tabs().load_all(self);
        self.update_profile_ui();
        self.toast(&format!("Switched to {name}"));
    }

    pub fn update_profile_ui(&self) {
        let name = db::profile_name();
        self.avatar.set_text(Some(&name));
        self.profile_btn.set_tooltip_text(Some(&format!("Profile: {name} (Ctrl+Shift+P)")));
        if let Some(w) = self.window.get() {
            if let Some(a) = w.lookup_action("profile") {
                a.change_state(&name.to_variant());
            }
        }
    }

    /// The avatar's menu: every profile, then "New profile…".
    pub fn rebuild_profile_menu(&self) {
        let menu = gio::Menu::new();
        let list = gio::Menu::new();
        for name in db::profiles() {
            let item = gio::MenuItem::new(Some(&name), None);
            item.set_action_and_target_value(Some("win.profile"), Some(&name.to_variant()));
            list.append_item(&item);
        }
        menu.append_section(Some("Profiles"), &list);
        let manage = gio::Menu::new();
        manage.append(Some("New profile…"), Some("win.new-profile"));
        menu.append_section(None, &manage);
        self.profile_btn.set_menu_model(Some(&menu));
    }

    pub fn set_incognito(&self, on: bool) {
        db::set_incognito(on);
        self.banner.set_revealed(on);
        if let Some(w) = self.window.get() {
            if on {
                w.add_css_class("yt-incognito");
            } else {
                w.remove_css_class("yt-incognito");
            }
        }
    }

    pub fn apply_appearance(&self) {
        if let (Some(t), Some(w)) = (self.theme.get(), self.window.get()) {
            t.apply(&self.config(), w);
            self.stack.set_enable_transitions(self.config().animations);
        }
        if let Some(w) = self.window.get() {
            self.layout(w.width().max(1));
        }
    }

    pub fn open_channel(self: &Rc<Self>, id: &str, name: &str) {
        self.show(&browse::channel_page(self, id, name));
    }

    pub fn open_playlist(self: &Rc<Self>, pl: &db::Playlist) {
        self.show(&pages::playlist_page(self, pl));
    }

    pub fn search_youtube(self: &Rc<Self>, query: &str) {
        if let Some(page) = self.search_page_for(query) {
            self.show(&page);
        }
    }

    /// The results page for a search, saved to search history. A pasted video link plays instead (None).
    fn search_page_for(self: &Rc<Self>, query: &str) -> Option<adw::NavigationPage> {
        let q = query.trim();
        if q.is_empty() {
            return None;
        }
        if let Some(id) = video_id_from(q).filter(|_| q.contains('/')) {
            self.play_id(&id);
            return None;
        }
        if self.config().remember_searches {
            if let Ok(c) = db::open() {
                let _ = db::record_search(&c, q);
            }
        }
        Some(browse::search_page(self, q))
    }

    fn start_refresh(self: &Rc<Self>) {
        let tabs = self.tabs();
        match self.stack.visible_child_name().as_deref() {
            Some("foryou") => return tabs.refresh_for_you(self),
            Some("history") => return tabs.load_history(),
            Some("library") => return tabs.load_library(self),
            _ => {}
        }
        if self.refreshing.replace(true) {
            return;
        }
        self.set_busy(true, "Refreshing subscriptions…");
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let ptx = tx.clone();
            if let Err(e) = feed::refresh(move |p| {
                let _ = ptx.send_blocking(Msg::Refresh(p));
            }) {
                let _ = tx.send_blocking(Msg::RefreshFailed(e.to_string()));
            }
        });
    }

    fn refresh_done(&self) {
        self.refreshing.set(false);
        self.set_busy(false, "");
        self.tabs().load_feed();
    }

    fn handle(self: &Rc<Self>, msg: Msg) {
        match msg {
            Msg::Thumb(id) => {
                let file = thumbs::path(&id);
                for (pic, shown) in self.pictures.borrow().iter() {
                    if *shown == id {
                        pic.set_filename(Some(&file));
                    }
                }
            }
            Msg::Refresh(feed::Progress::Rss { done, total }) => {
                self.refresh_btn.set_tooltip_text(Some(&format!("Checking channels {done}/{total}")));
                if done % 60 == 0 {
                    self.tabs().load_feed();
                }
            }
            Msg::Refresh(feed::Progress::Fallback { done, total }) => {
                self.refresh_btn.set_tooltip_text(Some(&format!("Asking yt-dlp {done}/{total}")));
            }
            Msg::Refresh(feed::Progress::Finished { videos, failed }) => {
                self.refresh_done();
                self.toast(&match failed {
                    0 => format!("Subscriptions updated ({videos} videos checked)"),
                    n => format!("Subscriptions updated; {n} channels had no videos"),
                });
            }
            Msg::RefreshFailed(e) => {
                if self.refreshing.get() {
                    self.refresh_done();
                }
                self.problem(crate::report::Code::guess(&e), &e);
            }
            Msg::Player(player::Event::Saved { video_id, .. }) => self.changed(&video_id),
            Msg::Player(player::Event::Exited { video_id, summary }) => {
                self.changed(&video_id);
                if summary.contains("KILLED") {
                    self.problem(crate::report::Code::Player, &format!("mpv was killed; your place is saved ({summary})"));
                }
            }
            Msg::Player(player::Event::Skipped { category, seconds, total, .. }) => {
                let saved = if total > seconds + 0.5 {
                    format!(" · {} saved in this video", crate::sponsorblock::clock(total))
                } else {
                    String::new()
                };
                self.toasts.add_toast(
                    adw::Toast::builder()
                        .title(glib::markup_escape_text(&format!(
                            "SponsorBlock skipped {} ({}){saved}",
                            crate::sponsorblock::label(&category).to_lowercase(),
                            crate::sponsorblock::clock(seconds)
                        )))
                        .timeout(3)
                        .build(),
                );
            }
            Msg::Player(player::Event::Started { video_id }) => self.changed(&video_id),
            Msg::Download(id, u) => {
                use crate::download::Update;
                let mut finished = None;
                {
                    let mut map = self.downloads.borrow_mut();
                    if let Some(a) = map.get_mut(&id) {
                        match &u {
                            Update::Queued => a.status = "Waiting for a free slot…".into(),
                            Update::Started => a.status = "Starting…".into(),
                            Update::Progress(p) => {
                                a.status = p.summary();
                                a.fraction = p.percent.unwrap_or(0.0) / 100.0;
                            }
                            Update::Processing(s) => a.status = s.clone(),
                            Update::Done(_) | Update::Failed(_) | Update::Cancelled => finished = Some(a.title.clone()),
                        }
                    }
                    if finished.is_some() {
                        map.remove(&id);
                    }
                }
                match (u, finished) {
                    (Update::Done(_), Some(t)) => self.toast(&format!("Downloaded “{t}”")),
                    (Update::Failed(e), Some(t)) => self.problem(crate::report::Code::Download, &format!("“{t}” didn't download: {e}")),
                    (Update::Cancelled, Some(_)) => self.toast("Download cancelled"),
                    _ => {}
                }
                self.downloads_changed();
            }
        }
    }

    /// Pick the column count and thumbnail height for a content width (px).
    fn layout(&self, width: i32) {
        const PAD: i32 = 6; // grid padding and card padding, see style.css
        let size = self.config.borrow().card_size.clone();
        let card_target = theme::CARD_SIZES.iter().find(|(n, _)| *n == size).map_or(280, |(_, w)| *w);
        let usable = (width - 2 * PAD).max(1);
        let cols = (usable / card_target).clamp(1, 8);
        let card = usable / cols - 2 * PAD;
        let thumb = (card * 9 / 16).max(40);
        if self.columns.replace(cols as u32) != cols as u32 {
            for g in self.grids.borrow().iter().filter_map(Weak::upgrade) {
                g.set_columns(cols as u32);
            }
        }
        self.sizes_css.load_from_string(&format!(".yt-pic {{ min-height: {thumb}px; }}"));
    }

    /// The search box filters the current tab.
    fn apply_filter(&self) {
        let q = self.query.borrow().clone();
        let hide = *self.hide_watched.borrow();
        self.grids.borrow_mut().retain(|g| g.strong_count() > 0);
        for g in self.grids.borrow().iter().filter_map(Weak::upgrade) {
            g.set_filter(&q, hide);
        }
    }
}

fn build(gapp: &adw::Application) -> Rc<App> {
    let (tx, rx) = async_channel::unbounded::<Msg>();
    let thumb_tx = tx.clone();
    let loader = thumbs::Loader::new(6, move |id| {
        let _ = thumb_tx.send_blocking(Msg::Thumb(id));
    });
    let config = crate::config::load();

    let stack = adw::ViewStack::new();
    let refresh_btn = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh_btn.add_css_class("yt-refresh");
    refresh_btn.set_tooltip_text(Some("Refresh (Ctrl+R)"));
    refresh_btn.set_action_name(Some("win.refresh"));
    let search = gtk::SearchEntry::builder().placeholder_text("Filter this page · Enter searches YouTube").hexpand(true).build();
    let search_bar = gtk::SearchBar::builder().show_close_button(true).build();
    search_bar.set_child(Some(&adw::Clamp::builder().maximum_size(560).child(&search).build()));
    search_bar.connect_entry(&search);

    let app = Rc::new(App {
        config: RefCell::new(config),
        tx,
        loader,
        pictures: RefCell::default(),
        grids: RefCell::default(),
        hide_watched: RefCell::new(false),
        columns: Cell::new(3),
        sizes_css: gtk::CssProvider::new(),
        query: RefCell::default(),
        refreshing: Cell::new(false),
        window: OnceCell::new(),
        home: adw::NavigationView::new(),
        tab_view: adw::TabView::new(),
        new_tab: Cell::new(false),
        toasts: adw::ToastOverlay::new(),
        stack: stack.clone(),
        refresh_btn: refresh_btn.clone(),
        search_bar: search_bar.clone(),
        search: search.clone(),
        pages: OnceCell::new(),
        theme: OnceCell::new(),
        hidden: RefCell::default(),
        blocked: RefCell::default(),
        banner: adw::Banner::builder()
            .title("Incognito: nothing you watch or search is saved")
            .button_label("Leave")
            .action_name("win.incognito")
            .build(),
        avatar: adw::Avatar::builder().size(24).show_initials(true).build(),
        profile_btn: gtk::MenuButton::new(),
        downloads: RefCell::default(),
        on_downloads: RefCell::default(),
    });
    app.reload_hidden();

    let tabs = pages::Tabs::build(&app);
    let _ = app.pages.set(tabs);

    // Header: tabs in the middle when wide; a bottom bar takes over when narrow.
    let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
    let narrow_title = adw::WindowTitle::new("YourTube", "");
    let header = adw::HeaderBar::builder().title_widget(&switcher).build();
    header.pack_start(&refresh_btn);
    let search_btn = gtk::ToggleButton::builder()
        .icon_name("system-search-symbolic")
        .tooltip_text("Filter or search (Ctrl+F)")
        .css_classes(["yt-search-btn"])
        .build();
    search_btn.bind_property("active", &search_bar, "search-mode-enabled").bidirectional().build();
    let menu = gio::Menu::new();
    let view = gio::Menu::new();
    view.append(Some("Hide watched videos"), Some("win.hide-watched"));
    view.append(Some("Incognito"), Some("win.incognito"));
    menu.append_section(None, &view);
    let data = gio::Menu::new();
    data.append(Some("Import subscriptions & history…"), Some("win.import"));
    menu.append_section(None, &data);
    let more = gio::Menu::new();
    more.append(Some("Preferences"), Some("win.preferences"));
    more.append(Some("Keyboard shortcuts"), Some("win.shortcuts"));
    more.append(Some("Report a problem…"), Some("win.report"));
    more.append(Some("About YourTube"), Some("win.about"));
    menu.append_section(None, &more);
    let menu_btn = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).tooltip_text("Menu").primary(true).build();
    header.pack_end(&menu_btn);
    header.pack_end(&search_btn);
    let incognito_btn = gtk::ToggleButton::builder()
        .icon_name("yt-incognito-symbolic")
        .tooltip_text("Incognito (Ctrl+Shift+N)")
        .action_name("win.incognito")
        .build();
    header.pack_end(&incognito_btn);
    // Profiles: an avatar with the profile's initials opens the profile menu.
    app.profile_btn.set_child(Some(&app.avatar));
    app.profile_btn.add_css_class("flat");
    app.profile_btn.add_css_class("yt-profile");
    header.pack_start(&app.profile_btn);
    let bottom = adw::ViewSwitcherBar::builder().stack(&stack).build();

    let main_view = adw::ToolbarView::new();
    main_view.add_top_bar(&header);
    main_view.add_top_bar(&app.banner);
    main_view.add_top_bar(&search_bar);
    main_view.set_content(Some(&stack));
    main_view.add_bottom_bar(&bottom);
    let main_page = adw::NavigationPage::builder().title("YourTube").tag("main").child(&main_view).build();
    app.home.add(&main_page);
    // Tabs: Home is pinned (an icon, can't be closed); the bar only shows once there's a second tab.
    // Our own shortcuts (shortcuts.rs) replace libadwaita's, which would take Alt+1… and Ctrl+Tab.
    app.tab_view.set_shortcuts(adw::TabViewShortcuts::NONE);
    let home_tab = app.tab_view.append(&app.home);
    home_tab.set_title("Home");
    home_tab.set_icon(Some(&gio::ThemedIcon::new("go-home-symbolic")));
    app.tab_view.set_page_pinned(&home_tab, true);
    // Shown only once there's a second tab (autohide still shows a lone pinned Home tab).
    let tab_bar = adw::TabBar::builder().view(&app.tab_view).autohide(false).visible(false).build();
    app.tab_view.connect_n_pages_notify(glib::clone!(
        #[weak]
        tab_bar,
        move |v| tab_bar.set_visible(v.n_pages() > 1)
    ));
    let tabbed = gtk::Box::new(gtk::Orientation::Vertical, 0);
    tabbed.append(&tab_bar);
    tabbed.append(&app.tab_view);
    app.tab_view.set_vexpand(true);
    // An invisible layer over everything reports the window's content width, which sets the grid layout.
    let sizer = gtk::DrawingArea::builder().can_target(false).build();
    let layers = gtk::Overlay::builder().child(&tabbed).build();
    layers.add_overlay(&sizer);
    sizer.connect_resize(glib::clone!(
        #[weak]
        app,
        move |_, w, _| app.layout(w)
    ));
    app.toasts.set_child(Some(&layers));

    let window = adw::ApplicationWindow::builder()
        .application(gapp)
        .title("YourTube")
        .icon_name(APP_ID)
        .default_width(1400)
        .default_height(900)
        .width_request(280)
        .height_request(300)
        .content(&app.toasts)
        .build();
    let bp = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 640sp").expect("valid condition"));
    bp.add_setter(&header, "title-widget", Some(&narrow_title.to_value()));
    bp.add_setter(&bottom, "reveal", Some(&true.to_value()));
    window.add_breakpoint(bp);
    search_bar.set_key_capture_widget(Some(&window));
    // The search bar lives on Home; opening it from another tab brings you there.
    search_bar.connect_search_mode_enabled_notify(glib::clone!(
        #[weak]
        app,
        move |bar| {
            if bar.is_search_mode() && app.nav() != app.home {
                if let Some(home) = (0..app.tab_view.n_pages()).map(|i| app.tab_view.nth_page(i)).find(|p| p.child() == app.home) {
                    app.tab_view.set_selected_page(&home);
                }
            }
        }
    ));
    let _ = app.window.set(window.clone());

    let display = WidgetExt::display(&window);
    let css = gtk::CssProvider::new();
    css.load_from_string(include_str!("style.css"));
    gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    gtk::style_context_add_provider_for_display(&display, &app.sizes_css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    theme::install_icons(&display);
    let _ = app.theme.set(theme::Theme::install(&display));
    app.apply_appearance();
    search_btn.connect_toggled(|b| {
        // A little pop when the search opens.
        b.remove_css_class("yt-pop");
        if b.is_active() {
            let b = b.clone();
            glib::idle_add_local_once(move || b.add_css_class("yt-pop"));
        }
    });

    // Search: typing filters the page; Enter searches YouTube.
    search.connect_search_changed(glib::clone!(
        #[weak]
        app,
        move |e| {
            *app.query.borrow_mut() = e.text().to_string();
            app.apply_filter();
        }
    ));
    search.connect_activate(glib::clone!(
        #[weak]
        app,
        move |e| {
            let q = e.text().to_string();
            app.search_bar.set_search_mode(false);
            app.search_youtube(&q);
        }
    ));
    search_bar.connect_search_mode_enabled_notify(glib::clone!(
        #[weak]
        app,
        move |bar| {
            if !bar.is_search_mode() {
                app.search.set_text("");
            }
        }
    ));
    stack.connect_visible_child_name_notify(glib::clone!(
        #[weak]
        app,
        move |s| app.tabs().shown(&app, s.visible_child_name().as_deref().unwrap_or(""))
    ));

    add_actions(&app, &window);
    shortcuts::install(&app, &window);
    app.update_profile_ui();
    app.set_incognito(db::incognito());

    glib::spawn_future_local(glib::clone!(
        #[weak]
        app,
        async move {
            while let Ok(msg) = rx.recv().await {
                app.handle(msg);
            }
        }
    ));

    app.tabs().load_all(&app);
    // Refresh the feed on start if it's more than an hour old.
    let stale = db::open()
        .and_then(|c| Ok(c.query_row("SELECT COALESCE(MAX(last_fetch), 0) FROM channels", [], |r| r.get::<_, i64>(0))?))
        .map(|t| db::now() - t > 3600)
        .unwrap_or(true);
    if stale {
        app.stack.set_visible_child_name("subscriptions");
        app.start_refresh();
    }
    app
}

fn add_actions(app: &Rc<App>, window: &adw::ApplicationWindow) {
    let refresh = gio::ActionEntry::builder("refresh")
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, _| app.start_refresh()
        ))
        .build();
    let search = gio::ActionEntry::builder("search")
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, _| {
                app.search_bar.set_search_mode(true);
                app.search.grab_focus();
            }
        ))
        .build();
    let hide = gio::ActionEntry::builder("hide-watched")
        .state(false.to_variant())
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, action, _| {
                let on = !action.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
                action.set_state(&on.to_variant());
                *app.hide_watched.borrow_mut() = on;
                app.apply_filter();
            }
        ))
        .build();
    let tab = gio::ActionEntry::builder("tab")
        .parameter_type(Some(glib::VariantTy::STRING))
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, p| {
                if let Some(name) = p.and_then(|p| p.get::<String>()) {
                    if app.stack.child_by_name(&name).is_some() {
                        app.go_home();
                        app.stack.set_visible_child_name(&name);
                    }
                }
            }
        ))
        .build();
    let import = gio::ActionEntry::builder("import")
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, _| pages::import_dialog(&app)
        ))
        .build();
    let about = gio::ActionEntry::builder("about")
        .activate(|w: &adw::ApplicationWindow, _, _| {
            adw::AboutDialog::builder()
                .application_name("YourTube")
                .application_icon(APP_ID)
                .version(env!("CARGO_PKG_VERSION"))
                .developer_name("Alyosha")
                .comments("A YouTube app that keeps your history and recommendations on your own computer.\n\nMade with love by Alyosha :)")
                .license_type(gtk::License::Gpl30)
                .build()
                .present(Some(w));
        })
        .build();
    // For scripts and other apps: open a YouTube search or a channel page by id.
    let search_for = gio::ActionEntry::builder("search-youtube")
        .parameter_type(Some(glib::VariantTy::STRING))
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, p| {
                if let Some(q) = p.and_then(|p| p.get::<String>()) {
                    app.search_youtube(&q);
                }
            }
        ))
        .build();
    let channel = gio::ActionEntry::builder("channel")
        .parameter_type(Some(glib::VariantTy::STRING))
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, p| {
                if let Some(id) = p.and_then(|p| p.get::<String>()).filter(|id| id.starts_with("UC")) {
                    app.open_channel(&id, "");
                }
            }
        ))
        .build();
    let video = gio::ActionEntry::builder("video")
        .parameter_type(Some(glib::VariantTy::STRING))
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, p| {
                if let Some(id) = p.and_then(|p| p.get::<String>()).and_then(|s| video_id_from(&s)) {
                    app.open_video(&Item { id: id.clone(), title: id, ..Default::default() });
                }
            }
        ))
        .build();
    // The same in a new tab: win.video-tab("ID").
    let video_tab = gio::ActionEntry::builder("video-tab")
        .parameter_type(Some(glib::VariantTy::STRING))
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, p| {
                if let Some(id) = p.and_then(|p| p.get::<String>()).and_then(|s| video_id_from(&s)) {
                    app.in_new_tab(|a| a.open_video(&Item { id: id.clone(), title: id, ..Default::default() }));
                }
            }
        ))
        .build();
    // Saves a PNG of the window (for docs and automated checks): win.save-screenshot("/path/file.png").
    let snap = gio::ActionEntry::builder("save-screenshot")
        .parameter_type(Some(glib::VariantTy::STRING))
        .activate(|w: &adw::ApplicationWindow, _, p| {
            if let (Some(path), Some(t)) = (p.and_then(|p| p.get::<String>()), report::screenshot(w.upcast_ref())) {
                let _ = t.save_to_png(path);
            }
        })
        .build();
    let prefs = gio::ActionEntry::builder("preferences")
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, _| prefs::show(&app)
        ))
        .build();
    let incognito = gio::ActionEntry::builder("incognito")
        .state(db::incognito().to_variant())
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, action, _| {
                let on = !action.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
                action.set_state(&on.to_variant());
                app.set_incognito(on);
            }
        ))
        .build();
    let profile = gio::ActionEntry::builder("profile")
        .parameter_type(Some(glib::VariantTy::STRING))
        .state(db::profile_name().to_variant())
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, p| {
                if let Some(name) = p.and_then(|p| p.get::<String>()) {
                    app.switch_profile(&name);
                }
            }
        ))
        .build();
    let new_profile = gio::ActionEntry::builder("new-profile")
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, _| pages::new_profile_dialog(&app)
        ))
        .build();
    let profiles_menu = gio::ActionEntry::builder("profiles-menu")
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, _| app.profile_btn.popup()
        ))
        .build();
    let report_action = gio::ActionEntry::builder("report")
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, _| report::show(&app)
        ))
        .build();
    let shortcuts = gio::ActionEntry::builder("shortcuts")
        .activate(glib::clone!(
            #[weak]
            app,
            move |_: &adw::ApplicationWindow, _, _| shortcuts::show(&app)
        ))
        .build();
    window.add_action_entries([
        refresh,
        search,
        hide,
        tab,
        import,
        about,
        search_for,
        channel,
        video,
        video_tab,
        snap,
        prefs,
        incognito,
        profile,
        new_profile,
        profiles_menu,
        shortcuts,
        report_action,
    ]);
    app.rebuild_profile_menu();
}

/// The video id in a tube://, youtube.com or youtu.be link, or a bare 11-character id.
pub fn video_id_from(s: &str) -> Option<String> {
    let valid = |id: &str| id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let s = s.trim();
    if valid(s) {
        return Some(s.to_owned());
    }
    let after = |marker: &str| {
        s.split(marker)
            .nth(1)
            .map(|rest| rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect::<String>())
    };
    ["v=", "youtu.be/", "/shorts/", "/live/", "/embed/"].iter().filter(|m| s.contains(*m)).find_map(|m| after(m)).filter(|id| valid(id))
}

#[cfg(test)]
mod tests {
    use super::video_id_from;

    #[test]
    fn links() {
        let id = Some("dQw4w9WgXcQ".to_owned());
        assert_eq!(video_id_from("yourtube://watch?v=dQw4w9WgXcQ"), id);
        assert_eq!(video_id_from("tube://watch?v=dQw4w9WgXcQ"), id);
        assert_eq!(video_id_from("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42"), id);
        assert_eq!(video_id_from("https://youtu.be/dQw4w9WgXcQ?si=x"), id);
        assert_eq!(video_id_from("https://www.youtube.com/shorts/dQw4w9WgXcQ"), id);
        assert_eq!(video_id_from("dQw4w9WgXcQ"), id);
        assert_eq!(video_id_from("cooking"), None);
        assert_eq!(video_id_from("tube://watch?v=short"), None);
    }
}
