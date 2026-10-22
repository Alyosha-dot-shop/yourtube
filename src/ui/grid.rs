//! A grid of video cards, used by every page.
//!
//! GridView only builds widgets for the cards on screen and reuses them while
//! scrolling, so a page of thousands of videos stays light. Each card shows the
//! thumbnail, duration, a progress bar, title, channel/age/views and an
//! optional note (why it was recommended, when you watched or saved it).

use super::{App, fmt};
use crate::db::Item;
use crate::thumbs;
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// One entry in a card's right-click menu.
pub struct MenuEntry {
    pub label: String,
    pub run: Box<dyn Fn()>,
}

impl MenuEntry {
    pub fn new(label: impl Into<String>, run: impl Fn() + 'static) -> Self {
        MenuEntry { label: label.into(), run: Box::new(run) }
    }
}

type ExtraMenu = Box<dyn Fn(&Item) -> Vec<MenuEntry>>;

pub struct VideoGrid {
    pub widget: gtk::Stack,
    app: std::rc::Weak<App>,
    /// Leave out "not interested" videos and hidden channels (off for History and playlists).
    respect_hidden: std::cell::Cell<bool>,
    /// On pages from YouTube (search, channels), a video without a length is live or upcoming.
    no_length_means_live: std::cell::Cell<bool>,
    /// DeArrow titles, by video id.
    branded: RefCell<HashMap<String, String>>,
    /// Called once when you scroll near the end ("load more"); armed again by `append_items`.
    on_end: RefCell<Option<Rc<dyn Fn()>>>,
    end_armed: std::cell::Cell<bool>,
    grid: gtk::GridView,
    store: gio::ListStore,
    items: RefCell<HashMap<String, Item>>,
    order: RefCell<Vec<String>>,
    query: RefCell<String>,
    hide_watched: RefCell<bool>,
    empty: adw::StatusPage,
    /// Page-specific menu entries, shown above the common ones.
    extra_menu: RefCell<Option<ExtraMenu>>,
    /// Called when the cards change, with the ids now visible (For You uses it to report impressions).
    on_shown: RefCell<Option<Box<dyn Fn(&[String])>>>,
}

struct Card {
    root: gtk::Box,
    picture: gtk::Picture,
    duration: gtk::Label,
    progress: gtk::ProgressBar,
    title: gtk::Label,
    meta: gtk::Label,
    note: gtk::Label,
}

impl VideoGrid {
    pub fn new(app: &Rc<App>, empty_title: &str, empty_text: &str, empty_icon: &str) -> Rc<Self> {
        let store = gio::ListStore::new::<gtk::StringObject>();
        let factory = gtk::SignalListItemFactory::new();
        let grid = gtk::GridView::builder()
            .model(&gtk::NoSelection::new(Some(store.clone())))
            .factory(&factory)
            .min_columns(app.columns.get())
            .max_columns(app.columns.get())
            .single_click_activate(true)
            .css_classes(["yt-grid"])
            .build();
        let scroller = gtk::ScrolledWindow::builder().child(&grid).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
        let empty = adw::StatusPage::builder()
            .icon_name(empty_icon)
            .title(glib::markup_escape_text(empty_title))
            .description(glib::markup_escape_text(empty_text))
            .build();
        // Placeholder cards are made only while loading: their pulse would otherwise keep the CPU busy.
        let loading = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let widget = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
        widget.add_named(&scroller, Some("grid"));
        widget.add_named(&empty, Some("empty"));
        widget.add_named(&loading, Some("loading"));
        widget.set_visible_child_name("empty");

        let this = Rc::new(VideoGrid {
            widget,
            app: Rc::downgrade(app),
            respect_hidden: std::cell::Cell::new(true),
            no_length_means_live: std::cell::Cell::new(false),
            branded: RefCell::default(),
            on_end: RefCell::default(),
            end_armed: std::cell::Cell::new(false),
            grid: grid.clone(),
            store,
            items: RefCell::default(),
            order: RefCell::default(),
            query: RefCell::default(),
            hide_watched: RefCell::new(*app.hide_watched.borrow()),
            empty,
            extra_menu: RefCell::default(),
            on_shown: RefCell::default(),
        });
        this.setup_factory(app, &factory);
        grid.connect_activate(glib::clone!(
            #[weak]
            this,
            #[weak]
            app,
            move |_, pos| {
                if let Some(it) = this.item_at(pos) {
                    app.play(&it);
                }
            }
        ));
        // Near the bottom? Ask the page for more (search results, channel videos, playlists).
        scroller.vadjustment().connect_value_changed(glib::clone!(
            #[weak]
            this,
            move |adj| {
                if this.end_armed.get() && adj.value() + adj.page_size() >= adj.upper() - adj.page_size() {
                    this.end_armed.set(false);
                    let f = this.on_end.borrow().clone();
                    if let Some(f) = f {
                        f();
                    }
                }
            }
        ));
        // Keys on the focused video (see shortcuts.rs for the list).
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak]
            this,
            #[weak]
            app,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, mods| this.on_key(&app, key, mods)
        ));
        grid.add_controller(keys);
        app.grids.borrow_mut().push(Rc::downgrade(&this));
        this
    }

    pub fn set_respect_hidden(&self, on: bool) {
        self.respect_hidden.set(on);
    }

    pub fn set_no_length_means_live(&self, on: bool) {
        self.no_length_means_live.set(on);
    }

    /// Load more when scrolled to the end. `more` should call `append_items` (or `end_reached`).
    pub fn set_on_end(&self, more: impl Fn() + 'static) {
        *self.on_end.borrow_mut() = Some(Rc::new(more));
        self.end_armed.set(true);
    }

    /// No more to load.
    pub fn end_reached(&self) {
        *self.on_end.borrow_mut() = None;
        self.end_armed.set(false);
    }

    /// Add cards at the end (the next page of results).
    pub fn append_items(self: &Rc<Self>, items: Vec<Item>) {
        let new: Vec<Item> = items.into_iter().filter(|i| !self.items.borrow().contains_key(&i.id)).collect();
        let ids: Vec<String> = new.iter().map(|i| i.id.clone()).collect();
        {
            let mut map = self.items.borrow_mut();
            let mut order = self.order.borrow_mut();
            for it in new {
                order.push(it.id.clone());
                map.insert(it.id.clone(), it);
            }
        }
        let start = self.store.n_items();
        let visible: Vec<gtk::StringObject> = ids
            .iter()
            .filter(|id| self.items.borrow().get(*id).is_some_and(|v| self.visible(v)))
            .map(|id| gtk::StringObject::new(id))
            .collect();
        self.store.splice(start, 0, &visible);
        self.end_armed.set(self.on_end.borrow().is_some());
        self.fetch_branding(ids);
    }

    /// Ask DeArrow for better titles (when it's turned on), then show them.
    fn fetch_branding(self: &Rc<Self>, ids: Vec<String>) {
        let Some(app) = self.app.upgrade() else { return };
        if !app.config.borrow().dearrow || ids.is_empty() {
            return;
        }
        let this = Rc::downgrade(self);
        App::bg(
            move || {
                let refs: Vec<&str> = ids.iter().take(300).map(String::as_str).collect();
                crate::extras::dearrow_many(&refs).into_iter().filter_map(|(id, b)| b.title.map(|t| (id, t))).collect::<Vec<_>>()
            },
            move |titles| {
                let Some(this) = this.upgrade() else { return };
                if titles.is_empty() {
                    return;
                }
                this.branded.borrow_mut().extend(titles);
                // Rebind the cards so the new titles show.
                let n = this.store.n_items();
                let all: Vec<gtk::StringObject> = (0..n)
                    .filter_map(|i| this.store.item(i).and_downcast::<gtk::StringObject>())
                    .map(|s| gtk::StringObject::new(&s.string()))
                    .collect();
                this.store.splice(0, n, &all);
            },
        );
    }

    fn visible(&self, v: &Item) -> bool {
        let (hide_live, q, hide) =
            (self.app.upgrade().is_some_and(|a| a.config.borrow().hide_live), self.query.borrow().clone(), *self.hide_watched.borrow());
        (!hide || !v.finished())
            && (!self.respect_hidden.get() || self.app.upgrade().is_none_or(|a| a.allowed(v)))
            && (!hide_live || !self.no_length_means_live.get() || v.length.is_some())
            && (q.is_empty() || v.title.to_lowercase().contains(&q) || v.channel.to_lowercase().contains(&q))
    }

    /// The video whose card has keyboard focus, if any.
    fn focused(&self) -> Option<(Item, gtk::Widget)> {
        let mut w = self.grid.root()?.focus()?;
        // Focus sits on the grid's cell; our card box is its child (or an ancestor of a focused button).
        if let Some(child) = w.first_child() {
            if let Some(it) = self.items.borrow().get(child.widget_name().as_str()) {
                return Some((it.clone(), child));
            }
        }
        loop {
            if let Some(it) = self.items.borrow().get(w.widget_name().as_str()) {
                return Some((it.clone(), w));
            }
            w = w.parent()?;
        }
    }

    fn on_key(self: &Rc<Self>, app: &Rc<App>, key: gtk::gdk::Key, mods: gtk::gdk::ModifierType) -> glib::Propagation {
        let Some(action) = super::shortcuts::video_action(&app.config(), key, mods) else { return glib::Propagation::Proceed };
        let Some((it, card)) = self.focused() else { return glib::Propagation::Proceed };
        match action {
            "video.play" => app.play(&it),
            "video.audio" => app.listen(&it),
            "video.details" => app.open_video(&it),
            "video.new_tab" => app.in_new_tab(|a| a.open_video(&it)),
            "video.download" => app.toggle_download(&it),
            "video.watch_later" => app.toggle_watch_later(&it),
            "video.playlist" => super::pages::add_to_playlist_dialog(app, &it),
            "video.watched" => app.toggle_watched(&it),
            "video.more" => app.more_like_this(&it),
            "video.not_interested" => app.not_interested(&it),
            "video.block" => app.block_channel_of(&it),
            "video.channel" if !it.channel_id.is_empty() => app.open_channel(&it.channel_id, &it.channel),
            "video.copy" => app.copy_link(&it),
            "video.menu" => self.popup_menu(app, &it, &card, card.width() as f64 / 2.0, card.height() as f64 / 3.0),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }

    /// Fixed number of columns, chosen by the window from its width.
    pub fn set_columns(&self, n: u32) {
        self.grid.set_min_columns(n);
        self.grid.set_max_columns(n);
    }

    pub fn set_extra_menu(&self, f: impl Fn(&Item) -> Vec<MenuEntry> + 'static) {
        *self.extra_menu.borrow_mut() = Some(Box::new(f));
    }

    pub fn set_on_shown(&self, f: impl Fn(&[String]) + 'static) {
        *self.on_shown.borrow_mut() = Some(Box::new(f));
    }

    pub fn loading(&self) {
        if self.order.borrow().is_empty() {
            if let Some(slot) = self.widget.child_by_name("loading").and_downcast::<gtk::Box>() {
                if slot.first_child().is_none() {
                    slot.append(&skeleton());
                }
            }
            self.widget.set_visible_child_name("loading");
        }
    }

    /// Show a page other than "loading", and drop the placeholder cards.
    fn show(&self, name: &str) {
        self.widget.set_visible_child_name(name);
        if let Some(slot) = self.widget.child_by_name("loading").and_downcast::<gtk::Box>() {
            while let Some(c) = slot.first_child() {
                slot.remove(&c);
            }
        }
    }

    pub fn set_empty_text(&self, title: &str, text: &str) {
        self.empty.set_title(title);
        self.empty.set_description(Some(&glib::markup_escape_text(text)));
    }

    pub fn focus(&self) {
        self.grid.grab_focus();
    }

    /// Replace all cards.
    pub fn set_items(self: &Rc<Self>, items: Vec<Item>) {
        let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
        self.set_items_inner(items);
        self.end_armed.set(self.on_end.borrow().is_some());
        self.fetch_branding(ids);
    }

    fn set_items_inner(&self, items: Vec<Item>) {
        *self.order.borrow_mut() = items.iter().map(|i| i.id.clone()).collect();
        let mut map = HashMap::with_capacity(items.len());
        for it in items {
            map.entry(it.id.clone()).or_insert(it);
        }
        self.order.borrow_mut().dedup();
        *self.items.borrow_mut() = map;
        self.apply_filter();
    }

    pub fn set_filter(&self, query: &str, hide_watched: bool) {
        *self.query.borrow_mut() = query.to_lowercase();
        *self.hide_watched.borrow_mut() = hide_watched;
        self.apply_filter();
    }

    fn apply_filter(&self) {
        let items = self.items.borrow();
        let mut seen = std::collections::HashSet::new();
        let visible: Vec<gtk::StringObject> = self
            .order
            .borrow()
            .iter()
            .filter(|id| seen.insert(id.as_str()))
            .filter_map(|id| items.get(id))
            .filter(|v| self.visible(v))
            .map(|v| gtk::StringObject::new(&v.id))
            .collect();
        let ids: Vec<String> = visible.iter().map(|s| s.string().to_string()).collect();
        let fresh = self.store.n_items() == 0 || self.widget.visible_child_name().as_deref() != Some("grid");
        self.store.splice(0, self.store.n_items(), &visible);
        self.show(if visible.is_empty() { "empty" } else { "grid" });
        if fresh {
            // Cards fade in when a page (re)loads; re-adding the class restarts the animation.
            // It comes off again once they're in, so later updates (progress every few seconds,
            // background refreshes, cards scrolled into view) happen silently.
            self.grid.remove_css_class("yt-enter");
            let g = self.grid.clone();
            glib::idle_add_local_once(move || {
                g.add_css_class("yt-enter");
                glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || g.remove_css_class("yt-enter"));
            });
        }
        drop(items);
        if let Some(f) = self.on_shown.borrow().as_ref() {
            f(&ids);
        }
    }

    /// The recommender run a card came from (For You).
    pub fn run_of(&self, id: &str) -> Option<i64> {
        self.items.borrow().get(id).and_then(|i| i.run_id)
    }

    fn item_at(&self, pos: u32) -> Option<Item> {
        let id = self.store.item(pos).and_downcast::<gtk::StringObject>()?.string();
        self.items.borrow().get(id.as_str()).cloned()
    }

    /// New progress for a video: update its card if it's on this page.
    pub fn update_progress(&self, id: &str, position: Option<f64>, length: Option<f64>) {
        {
            let mut items = self.items.borrow_mut();
            let Some(v) = items.get_mut(id) else { return };
            v.watched = position.is_some();
            v.position = position;
            if length.is_some() {
                v.length = length;
            }
        }
        for i in 0..self.store.n_items() {
            if self.store.item(i).and_downcast::<gtk::StringObject>().is_some_and(|s| s.string() == id) {
                self.store.splice(i, 1, &[gtk::StringObject::new(id)]); // rebinds the card
                break;
            }
        }
    }

    fn setup_factory(self: &Rc<Self>, app: &Rc<App>, factory: &gtk::SignalListItemFactory) {
        // Cards are built once and reused for whatever scrolls into view; each keeps its widgets here.
        let cards: Rc<RefCell<HashMap<gtk::Box, Card>>> = Rc::default();
        let this = Rc::downgrade(self);

        factory.connect_setup(glib::clone!(
            #[strong]
            cards,
            #[strong]
            this,
            #[weak]
            app,
            move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let card = card_widgets();
                let click = gtk::GestureClick::builder().button(3).build();
                click.connect_pressed(glib::clone!(
                    #[strong]
                    this,
                    #[weak]
                    app,
                    #[weak(rename_to = root)]
                    card.root,
                    move |g, _, x, y| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        let Some(grid) = this.upgrade() else { return };
                        let id = root.widget_name();
                        let Some(it) = grid.items.borrow().get(id.as_str()).cloned() else { return };
                        grid.popup_menu(&app, &it, &root, x, y);
                    }
                ));
                card.root.add_controller(click);
                // Middle-click opens the video's page in a new tab, like a link in a browser.
                let middle = gtk::GestureClick::builder().button(2).build();
                middle.connect_pressed(glib::clone!(
                    #[strong]
                    this,
                    #[weak]
                    app,
                    #[weak(rename_to = root)]
                    card.root,
                    move |g, _, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        let Some(grid) = this.upgrade() else { return };
                        let Some(it) = grid.items.borrow().get(root.widget_name().as_str()).cloned() else { return };
                        app.in_new_tab(|a| a.open_video(&it));
                    }
                ));
                card.root.add_controller(middle);
                // Long-press does the same on touch screens.
                let press = gtk::GestureLongPress::new();
                press.connect_pressed(glib::clone!(
                    #[strong]
                    this,
                    #[weak]
                    app,
                    #[weak(rename_to = root)]
                    card.root,
                    move |_, x, y| {
                        let Some(grid) = this.upgrade() else { return };
                        let Some(it) = grid.items.borrow().get(root.widget_name().as_str()).cloned() else { return };
                        grid.popup_menu(&app, &it, &root, x, y);
                    }
                ));
                card.root.add_controller(press);
                item.set_child(Some(&card.root));
                cards.borrow_mut().insert(card.root.clone(), card);
            }
        ));

        factory.connect_bind(glib::clone!(
            #[strong]
            cards,
            #[strong]
            this,
            #[weak]
            app,
            move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let Some(grid) = this.upgrade() else { return };
                let Some(root) = item.child().and_downcast::<gtk::Box>() else { return };
                let Some(id) = item.item().and_downcast::<gtk::StringObject>().map(|s| s.string().to_string()) else {
                    return;
                };
                let cards = cards.borrow();
                let Some(card) = cards.get(&root) else { return };
                let items = grid.items.borrow();
                let Some(v) = items.get(&id) else { return };
                root.set_widget_name(&id);
                let hide_views = app.config.borrow().hide_views;
                fill_card(card, v, hide_views, grid.branded.borrow().get(&id).map(String::as_str));
                let file = thumbs::path(&id);
                if file.exists() {
                    card.picture.set_filename(Some(&file));
                } else {
                    card.picture.set_paintable(gtk::gdk::Paintable::NONE);
                    app.loader.request(&id);
                }
                app.pictures.borrow_mut().insert(card.picture.clone(), id);
            }
        ));

        factory.connect_teardown(glib::clone!(
            #[strong]
            cards,
            #[weak]
            app,
            move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                if let Some(root) = item.child().and_downcast::<gtk::Box>() {
                    if let Some(card) = cards.borrow_mut().remove(&root) {
                        app.pictures.borrow_mut().remove(&card.picture);
                    }
                }
            }
        ));
    }

    fn popup_menu(self: &Rc<Self>, app: &Rc<App>, it: &Item, root: &impl IsA<gtk::Widget>, x: f64, y: f64) {
        let mut entries = self.extra_menu.borrow().as_ref().map(|f| f(it)).unwrap_or_default();
        entries.extend(app.common_menu(it));
        let pop = gtk::Popover::builder().has_arrow(true).build();
        let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for e in entries {
            let b = gtk::Button::builder().css_classes(["flat"]).build();
            b.set_child(Some(&gtk::Label::builder().label(&e.label).xalign(0.0).build()));
            let run = e.run;
            b.connect_clicked(glib::clone!(
                #[weak]
                pop,
                move |_| {
                    pop.popdown();
                    run();
                }
            ));
            col.append(&b);
        }
        pop.set_child(Some(&col));
        pop.set_parent(root);
        pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        pop.popup();
    }
}

/// Grey placeholder cards that pulse while a page loads.
fn skeleton() -> gtk::Widget {
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(1)
        .max_children_per_line(6)
        .valign(gtk::Align::Start)
        .margin_top(12)
        .margin_start(12)
        .margin_end(12)
        .column_spacing(12)
        .row_spacing(18)
        .can_target(false)
        .build();
    for i in 0..12 {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
        card.set_size_request(200, -1);
        for (h, w) in [(112, -1), (14, 180), (12, 120)] {
            let b = gtk::Box::builder()
                .height_request(h)
                .width_request(w)
                .halign(if w > 0 { gtk::Align::Start } else { gtk::Align::Fill })
                .css_classes(["yt-skeleton"])
                .build();
            card.append(&b);
        }
        // Stagger the pulse a little so it ripples.
        card.add_css_class(&format!("yt-skel-{}", i % 4));
        flow.append(&card);
    }
    gtk::ScrolledWindow::builder().child(&flow).vexpand(true).build().upcast()
}

fn card_widgets() -> Card {
    // The height comes from CSS (.yt-pic), set from the column width so thumbnails stay 16:9.
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .can_shrink(true)
        .hexpand(true)
        .css_classes(["yt-thumb", "yt-pic"])
        .build();
    let duration = gtk::Label::builder().halign(gtk::Align::End).valign(gtk::Align::End).css_classes(["yt-duration"]).build();
    let progress = gtk::ProgressBar::builder().valign(gtk::Align::End).css_classes(["yt-progress"]).build();
    let overlay = gtk::Overlay::builder().child(&picture).overflow(gtk::Overflow::Hidden).css_classes(["yt-thumb"]).build();
    overlay.add_overlay(&duration);
    overlay.add_overlay(&progress);
    let label = |classes: &[&str], lines: i32| {
        gtk::Label::builder()
            .xalign(0.0)
            .wrap(lines > 1)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .lines(lines)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(1)
            .hexpand(true)
            .css_classes(classes.to_vec())
            .build()
    };
    let title = label(&["heading"], 2);
    let meta = label(&["dim-label", "caption"], 1);
    let note = label(&["caption", "yt-note"], 2);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    root.set_width_request(120);
    root.append(&overlay);
    root.append(&title);
    root.append(&meta);
    root.append(&note);
    Card { root, picture, duration, progress, title, meta, note }
}

fn fill_card(card: &Card, v: &Item, hide_views: bool, better_title: Option<&str>) {
    match better_title {
        Some(t) => {
            card.title.set_label(t);
            card.root.set_tooltip_text(Some(&format!("{t}\n\nOriginal title: {}", v.title)));
        }
        None => {
            card.title.set_label(&v.title);
            card.root.set_tooltip_text(Some(&v.title));
        }
    }
    let mut meta = Vec::new();
    if !v.channel.is_empty() {
        meta.push(v.channel.clone());
    }
    if let Some(p) = v.published {
        meta.push(fmt::age(p));
    }
    if let Some(n) = v.views.filter(|_| !hide_views) {
        meta.push(fmt::views(n));
    }
    card.meta.set_label(&meta.join(" · "));
    card.note.set_label(v.note.as_deref().unwrap_or(""));
    card.note.set_visible(v.note.is_some());
    match v.length {
        Some(l) if l > 0.0 => {
            card.duration.set_label(&fmt::clock(l));
            card.duration.set_visible(true);
        }
        _ => card.duration.set_visible(false),
    }
    let frac = match (v.position, v.length) {
        (Some(p), Some(l)) if l > 0.0 => (p / l).clamp(0.0, 1.0),
        _ => 0.0,
    };
    card.progress.set_fraction(frac);
    card.progress.set_visible(frac > 0.01);
    if v.finished() {
        card.root.add_css_class("yt-watched");
    } else {
        card.root.remove_css_class("yt-watched");
    }
}
