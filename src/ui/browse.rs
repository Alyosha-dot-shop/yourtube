//! Pages for things on YouTube: search (with filters), channels (with tabs),
//! YouTube playlists, and your downloads. Results keep coming as you scroll.

use super::grid::VideoGrid;
use super::{App, fmt};
use crate::db::{self, Item};
use crate::yt;
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

fn sub_page(title: &str, subtitle: &str, content: &impl IsA<gtk::Widget>) -> (adw::NavigationPage, adw::HeaderBar, adw::WindowTitle) {
    let wt = adw::WindowTitle::new(title, subtitle);
    let header = adw::HeaderBar::builder().title_widget(&wt).build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(content));
    (adw::NavigationPage::builder().title(title).child(&view).build(), header, wt)
}

fn chips() -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .max_children_per_line(6)
        .row_spacing(6)
        .column_spacing(6)
        .margin_start(12)
        .margin_end(12)
        .margin_top(10)
        .visible(false)
        .build()
}

fn pill(label: &str, icon: &str, f: impl Fn() + 'static) -> gtk::Button {
    let content = adw::ButtonContent::builder().label(label).icon_name(icon).build();
    if let Some(l) = content.first_child().and_then(|c| c.next_sibling()).and_downcast::<gtk::Label>() {
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l.set_max_width_chars(36);
    }
    let b = gtk::Button::builder().child(&content).tooltip_text(label).css_classes(["pill"]).build();
    b.connect_clicked(move |_| f());
    b
}

fn with_progress(videos: Vec<crate::feed::Video>) -> Vec<Item> {
    let mut items: Vec<Item> = videos.into_iter().map(Item::from).collect();
    if let Ok(c) = db::open() {
        let _ = db::add_progress(&c, &mut items);
    }
    items
}

// ---- search ----

const SORTS: &[(&str, yt::Sort)] = &[("Relevance", yt::Sort::Relevance), ("Popularity", yt::Sort::Views)];
const UPLOADS: &[(&str, yt::Upload)] = &[
    ("Any time", yt::Upload::Any),
    ("Last hour", yt::Upload::Hour),
    ("Today", yt::Upload::Today),
    ("This week", yt::Upload::Week),
    ("This month", yt::Upload::Month),
    ("This year", yt::Upload::Year),
];
const LENGTHS: &[(&str, yt::Length)] = &[
    ("Any length", yt::Length::Any),
    ("Under 3 minutes", yt::Length::Short),
    ("3–20 minutes", yt::Length::Medium),
    ("Over 20 minutes", yt::Length::Long),
];
const KINDS: &[(&str, yt::Kind)] = &[
    ("Everything", yt::Kind::Any),
    ("Videos", yt::Kind::Video),
    ("Channels", yt::Kind::Channel),
    ("Playlists", yt::Kind::Playlist),
    ("Movies", yt::Kind::Movie),
];

pub fn search_page(app: &Rc<App>, query: &str) -> adw::NavigationPage {
    let grid = VideoGrid::new(app, "No results", "Try other words or fewer filters.", "system-search-symbolic");
    grid.set_no_length_means_live(true);
    let found = chips();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&found);
    content.append(&grid.widget);
    let (page, header, title) = sub_page(&format!("“{query}”"), "Searching YouTube…", &content);

    // Filters live in a popover; changing one searches again.
    let filters = Rc::new(RefCell::new(yt::Filters::default()));
    let pop_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    let filter_btn = gtk::MenuButton::builder().icon_name("view-more-horizontal-symbolic").tooltip_text("Filters").build();
    filter_btn.set_popover(Some(&gtk::Popover::builder().child(&pop_box).build()));
    header.pack_end(&filter_btn);

    let next: Rc<RefCell<Option<String>>> = Rc::default();
    let run = {
        let (app, grid, found, title, filters, next, q) =
            (Rc::downgrade(app), grid.clone(), found.clone(), title.clone(), filters.clone(), next.clone(), query.to_owned());
        Rc::new(move || {
            let f = filters.borrow().clone();
            let q = q.clone();
            grid.set_items(Vec::new());
            grid.loading();
            title.set_subtitle("Searching YouTube…");
            while let Some(c) = found.first_child() {
                found.remove(&c);
            }
            found.set_visible(false);
            let (app, grid, found, title, next) = (app.clone(), grid.clone(), found.clone(), title.clone(), next.clone());
            App::bg(
                move || yt::search_with(&q, &f).map_err(|e| e.to_string()).map(|r| (with_progress(r.videos.clone()), r)),
                move |r: Result<(Vec<Item>, yt::SearchResults), String>| {
                    let Some(app) = app.upgrade() else { return };
                    match r {
                        Ok((items, res)) => {
                            title.set_subtitle(&format!(
                                "{} results{}",
                                items.len(),
                                if res.next.is_some() { ", more as you scroll" } else { "" }
                            ));
                            for ch in res.channels {
                                let label = ch.subscribers.as_ref().map_or_else(|| ch.name.clone(), |s| format!("{} · {s}", ch.name));
                                let a = Rc::downgrade(&app);
                                found.append(&pill(&label, "avatar-default-symbolic", move || {
                                    if let Some(a) = a.upgrade() {
                                        a.open_channel(&ch.id, &ch.name);
                                    }
                                }));
                                found.set_visible(true);
                            }
                            for pl in res.playlists {
                                let label = match &pl.video_count_text {
                                    Some(n) => format!("{} · {n}", pl.title),
                                    None => pl.title.clone(),
                                };
                                let a = Rc::downgrade(&app);
                                found.append(&pill(&label, "view-list-symbolic", move || {
                                    if let Some(a) = a.upgrade() {
                                        a.show(&playlist_page(&a, &pl.id, &pl.title));
                                    }
                                }));
                                found.set_visible(true);
                            }
                            *next.borrow_mut() = res.next;
                            grid.set_items(items);
                            grid.focus();
                        }
                        Err(e) => {
                            title.set_subtitle("");
                            grid.set_empty_text("Search failed", &e);
                            grid.set_items(Vec::new());
                            app.problem(crate::report::Code::guess(&e), &format!("Search: {e}"));
                        }
                    }
                },
            );
        })
    };

    // Endless results.
    let more = {
        let (grid_w, next) = (Rc::downgrade(&grid), next.clone());
        move || {
            let Some(tok) = next.borrow_mut().take() else {
                if let Some(g) = grid_w.upgrade() {
                    g.end_reached();
                }
                return;
            };
            let (grid_w, next) = (grid_w.clone(), next.clone());
            App::bg(
                move || yt::search_more(&tok).map_err(|e| e.to_string()).map(|r| (with_progress(r.videos.clone()), r.next)),
                move |r: Result<(Vec<Item>, Option<String>), String>| {
                    let Some(g) = grid_w.upgrade() else { return };
                    match r {
                        Ok((items, n)) => {
                            *next.borrow_mut() = n;
                            g.append_items(items);
                        }
                        Err(_) => g.end_reached(),
                    }
                },
            );
        }
    };
    grid.set_on_end(more);

    let dropdown = |label: &str, names: Vec<&str>, set: Box<dyn Fn(&mut yt::Filters, usize)>| {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build());
        let dd = gtk::DropDown::from_strings(&names);
        let (filters, run) = (filters.clone(), run.clone());
        dd.connect_selected_notify(move |d| {
            set(&mut filters.borrow_mut(), d.selected() as usize);
            run();
        });
        row.append(&dd);
        pop_box.append(&row);
    };
    dropdown("Sort", SORTS.iter().map(|s| s.0).collect(), Box::new(|f, i| f.sort = SORTS[i].1));
    dropdown("Uploaded", UPLOADS.iter().map(|s| s.0).collect(), Box::new(|f, i| f.upload = UPLOADS[i].1));
    dropdown("Length", LENGTHS.iter().map(|s| s.0).collect(), Box::new(|f, i| f.duration = LENGTHS[i].1));
    dropdown("Type", KINDS.iter().map(|s| s.0).collect(), Box::new(|f, i| f.kind = KINDS[i].1));
    let toggle = |label: &str, set: fn(&mut yt::Filters, bool)| {
        let c = gtk::CheckButton::with_label(label);
        let (filters, run) = (filters.clone(), run.clone());
        c.connect_toggled(move |c| {
            set(&mut filters.borrow_mut(), c.is_active());
            run();
        });
        pop_box.append(&c);
    };
    toggle("Live now", |f, on| f.live = on);
    toggle("HD", |f, on| f.hd = on);
    toggle("4K", |f, on| f.four_k = on);
    toggle("Subtitles", |f, on| f.subtitles = on);
    toggle("Creative Commons", |f, on| f.creative_commons = on);
    pop_box.append(
        &gtk::Label::builder()
            .label("YouTube no longer sorts searches by date; “Uploaded” narrows it down instead.")
            .wrap(true)
            .max_width_chars(30)
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .build(),
    );

    run();
    page
}

// ---- channels ----

const TABS: &[(&str, yt::Tab)] =
    &[("Videos", yt::Tab::Videos), ("Live", yt::Tab::Streams), ("Shorts", yt::Tab::Shorts), ("Playlists", yt::Tab::Playlists)];

pub fn channel_page(app: &Rc<App>, channel_id: &str, name: &str) -> adw::NavigationPage {
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vexpand(true).build();
    let switcher = gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Center).margin_top(8).build();
    let about = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .lines(3)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .margin_start(14)
        .margin_end(14)
        .margin_top(8)
        .css_classes(["dim-label"])
        .visible(false)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.append(&about);
    content.append(&switcher);
    content.append(&stack);
    let (page, header, title) = sub_page(if name.is_empty() { "Channel" } else { name }, "", &content);

    let subscribed = db::open().and_then(|c| db::is_subscribed(&c, channel_id)).unwrap_or(false);
    let sub_btn = gtk::Button::builder().label(if subscribed { "Unsubscribe" } else { "Subscribe" }).build();
    if !subscribed {
        sub_btn.add_css_class("suggested-action");
    }
    header.pack_end(&sub_btn);
    let chan_name = Rc::new(RefCell::new(name.to_owned()));
    let first_videos: Rc<RefCell<Vec<crate::feed::Video>>> = Rc::default();
    sub_btn.connect_clicked(glib::clone!(
        #[weak]
        app,
        #[strong]
        chan_name,
        #[strong]
        first_videos,
        #[strong(rename_to = cid)]
        channel_id.to_owned(),
        move |b| {
            let on = b.label().as_deref() == Some("Subscribe");
            let name = chan_name.borrow().clone();
            if let Ok(c) = db::open() {
                let _ = db::set_subscribed(&c, &cid, &name, on);
                if on {
                    let _ = db::store_videos(&c, &first_videos.borrow());
                }
            }
            b.set_label(if on { "Unsubscribe" } else { "Subscribe" });
            if on {
                b.remove_css_class("suggested-action");
            } else {
                b.add_css_class("suggested-action");
            }
            app.toast(&if on { format!("Subscribed to {name}") } else { format!("Unsubscribed from {name}") });
            app.tabs().load_feed();
        }
    ));

    let mut group: Option<gtk::ToggleButton> = None;
    for (i, (label, tab)) in TABS.iter().enumerate() {
        let b = gtk::ToggleButton::builder().label(*label).build();
        if let Some(g) = &group {
            b.set_group(Some(g));
        } else {
            group = Some(b.clone());
        }
        switcher.append(&b);
        let loaded = Rc::new(std::cell::Cell::new(false));
        let (app_w, stack2, cid, title2, about2, chan_name2, first_videos2, tab) = (
            Rc::downgrade(app),
            stack.clone(),
            channel_id.to_owned(),
            title.clone(),
            about.clone(),
            chan_name.clone(),
            first_videos.clone(),
            *tab,
        );
        b.connect_toggled(move |b| {
            if !b.is_active() {
                return;
            }
            stack2.set_visible_child_name(&i.to_string());
            if loaded.replace(true) {
                return;
            }
            let Some(app) = app_w.upgrade() else { return };
            let page_widget = channel_tab(&app, &cid, tab, &title2, &about2, &chan_name2, &first_videos2);
            stack2.add_named(&page_widget, Some(&i.to_string()));
            stack2.set_visible_child_name(&i.to_string());
        });
        if i == 0 {
            b.set_active(true);
        }
    }
    page
}

#[allow(clippy::too_many_arguments)]
fn channel_tab(
    app: &Rc<App>,
    cid: &str,
    tab: yt::Tab,
    title: &adw::WindowTitle,
    about: &gtk::Label,
    chan_name: &Rc<RefCell<String>>,
    first_videos: &Rc<RefCell<Vec<crate::feed::Video>>>,
) -> gtk::Widget {
    if tab == yt::Tab::Playlists {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_top(12)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(12)
            .build();
        let spinner = adw::Spinner::builder().height_request(32).margin_top(24).build();
        let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
        col.append(&spinner);
        col.append(&list);
        let cid = cid.to_owned();
        let app_w = Rc::downgrade(app);
        App::bg(
            move || yt::channel_tab(&cid, yt::Tab::Playlists).map_err(|e| e.to_string()),
            glib::clone!(
                #[weak]
                list,
                #[weak]
                spinner,
                move |r: Result<yt::TabPage, String>| {
                    spinner.set_visible(false);
                    let Ok(p) = r else {
                        list.append(&adw::ActionRow::builder().title("Couldn't load playlists").build());
                        return;
                    };
                    if p.playlists.is_empty() {
                        list.append(&adw::ActionRow::builder().title("No playlists").build());
                    }
                    for pl in p.playlists {
                        let row = adw::ActionRow::builder()
                            .title(glib::markup_escape_text(&pl.title))
                            .subtitle(pl.video_count_text.clone().unwrap_or_default())
                            .activatable(true)
                            .build();
                        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                        let a = app_w.clone();
                        row.connect_activated(move |_| {
                            if let Some(a) = a.upgrade() {
                                a.show(&playlist_page(&a, &pl.id, &pl.title));
                            }
                        });
                        list.append(&row);
                    }
                }
            ),
        );
        return gtk::ScrolledWindow::builder()
            .child(&adw::Clamp::builder().maximum_size(800).child(&col).build())
            .vexpand(true)
            .build()
            .upcast();
    }

    let grid =
        VideoGrid::new(app, "Nothing here", "This channel has nothing on this tab, or YouTube didn't answer.", "video-display-symbolic");
    grid.set_no_length_means_live(true);
    grid.loading();
    let next: Rc<RefCell<Option<String>>> = Rc::default();
    let (cid2, next2, grid_w) = (cid.to_owned(), next.clone(), Rc::downgrade(&grid));
    grid.set_on_end(move || {
        let Some(tok) = next2.borrow_mut().take() else {
            if let Some(g) = grid_w.upgrade() {
                g.end_reached();
            }
            return;
        };
        let (next3, grid_w2, cid3) = (next2.clone(), grid_w.clone(), cid2.clone());
        App::bg(
            move || yt::channel_more(&tok).map_err(|e| e.to_string()),
            move |r: Result<yt::TabPage, String>| {
                let Some(g) = grid_w2.upgrade() else { return };
                match r {
                    Ok(p) => {
                        *next3.borrow_mut() = p.next;
                        let videos: Vec<crate::feed::Video> = p
                            .videos
                            .into_iter()
                            .map(|mut v| {
                                if v.channel_id.is_empty() {
                                    v.channel_id = cid3.clone();
                                }
                                v
                            })
                            .collect();
                        g.append_items(with_progress(videos));
                    }
                    Err(_) => g.end_reached(),
                }
            },
        );
    });
    let (cid, title, about, chan_name, first_videos, is_videos) =
        (cid.to_owned(), title.clone(), about.clone(), chan_name.clone(), first_videos.clone(), tab == yt::Tab::Videos);
    App::bg(
        move || yt::channel_tab(&cid, tab).map_err(|e| e.to_string()),
        glib::clone!(
            #[strong]
            grid,
            move |r: Result<yt::TabPage, String>| match r {
                Ok(p) => {
                    if let Some(info) = &p.info {
                        title.set_title(&info.name);
                        title.set_subtitle(info.subscribers.as_deref().unwrap_or(""));
                        *chan_name.borrow_mut() = info.name.clone();
                        if let Some(d) = info.description.as_deref().filter(|d| !d.trim().is_empty()) {
                            about.set_label(d);
                            about.set_tooltip_text(Some(d));
                            about.set_visible(true);
                        }
                    }
                    let name = chan_name.borrow().clone();
                    let videos: Vec<crate::feed::Video> = p
                        .videos
                        .into_iter()
                        .map(|mut v| {
                            if v.channel.is_empty() {
                                v.channel.clone_from(&name);
                            }
                            v
                        })
                        .collect();
                    if is_videos {
                        *first_videos.borrow_mut() = videos.clone();
                        if let Ok(c) = db::open() {
                            if db::is_subscribed(&c, &videos.first().map(|v| v.channel_id.clone()).unwrap_or_default()).unwrap_or(false) {
                                let _ = db::store_videos(&c, &videos);
                            }
                        }
                    }
                    *next.borrow_mut() = p.next;
                    grid.set_items(with_progress(videos));
                }
                Err(e) => {
                    grid.set_empty_text("Couldn't load this channel", &e);
                    grid.set_items(Vec::new());
                }
            }
        ),
    );
    grid.widget.clone().upcast()
}

// ---- YouTube playlists ----

pub fn playlist_page(app: &Rc<App>, id: &str, name: &str) -> adw::NavigationPage {
    let grid = VideoGrid::new(app, "Empty playlist", "This playlist has no videos you can watch.", "view-list-symbolic");
    let (page, header, title) = sub_page(name, "YouTube playlist", &grid.widget);
    let play = gtk::Button::builder().icon_name("media-playback-start-symbolic").tooltip_text("Play all").sensitive(false).build();
    let save = gtk::Button::builder().icon_name("list-add-symbolic").tooltip_text("Save to your Library").sensitive(false).build();
    header.pack_end(&play);
    header.pack_end(&save);
    let all: Rc<RefCell<Vec<Item>>> = Rc::default();
    play.connect_clicked(glib::clone!(
        #[weak]
        app,
        #[strong]
        all,
        move |_| app.play_items(all.borrow().clone(), false)
    ));
    let pl_name = name.to_owned();
    save.connect_clicked(glib::clone!(
        #[weak]
        app,
        #[strong]
        all,
        move |b| {
            if let Ok(c) = db::open() {
                if let Ok(local) = db::create_playlist(&c, &pl_name) {
                    for it in all.borrow().iter() {
                        let _ = db::add_to_playlist(&c, &local, it);
                    }
                    app.toast(&format!("Saved “{pl_name}” to your Library ({} videos)", all.borrow().len()));
                    app.tabs().library_dirty.set(true);
                    b.set_sensitive(false);
                }
            }
        }
    ));
    grid.loading();
    let next: Rc<RefCell<Option<String>>> = Rc::default();
    let (next2, grid_w, all2) = (next.clone(), Rc::downgrade(&grid), all.clone());
    grid.set_on_end(move || {
        let Some(tok) = next2.borrow_mut().take() else {
            if let Some(g) = grid_w.upgrade() {
                g.end_reached();
            }
            return;
        };
        let (next3, grid_w2, all3) = (next2.clone(), grid_w.clone(), all2.clone());
        App::bg(
            move || yt::playlist_more(&tok).map_err(|e| e.to_string()),
            move |r: Result<yt::PlaylistPage, String>| {
                let Some(g) = grid_w2.upgrade() else { return };
                match r {
                    Ok(p) => {
                        *next3.borrow_mut() = p.next;
                        let items = with_progress(p.videos);
                        all3.borrow_mut().extend(items.iter().cloned());
                        g.append_items(items);
                    }
                    Err(_) => g.end_reached(),
                }
            },
        );
    });
    let id = id.to_owned();
    App::bg(
        move || yt::playlist(&id).map_err(|e| e.to_string()),
        glib::clone!(
            #[strong]
            grid,
            move |r: Result<yt::Playlist, String>| match r {
                Ok(p) => {
                    title.set_title(&p.title);
                    title.set_subtitle(&match p.video_count {
                        Some(n) => format!("{} · {n} videos", p.channel),
                        None => p.channel.clone(),
                    });
                    *next.borrow_mut() = p.next;
                    let items = with_progress(p.videos);
                    *all.borrow_mut() = items.clone();
                    play.set_sensitive(!items.is_empty());
                    save.set_sensitive(!items.is_empty());
                    grid.set_items(items);
                }
                Err(e) => {
                    grid.set_empty_text("Couldn't load this playlist", &e);
                    grid.set_items(Vec::new());
                }
            }
        ),
    );
    page
}

// ---- downloads ----

pub fn downloads_page(app: &Rc<App>) -> adw::NavigationPage {
    let active = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .visible(false)
        .build();
    let grid = VideoGrid::new(
        app,
        "No downloads yet",
        "Right-click a video (or press D) to download it. Downloads play from your disk, even offline.",
        "folder-download-symbolic",
    );
    grid.set_respect_hidden(false);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&active);
    content.append(&grid.widget);
    let folder = crate::download::dir(&app.config());
    let (page, header, title) = sub_page("Downloads", &folder.display().to_string(), &content);
    let open = gtk::Button::builder().icon_name("folder-open-symbolic").tooltip_text("Open the downloads folder").build();
    open.connect_clicked(glib::clone!(
        #[weak]
        app,
        move |_| {
            let _ = std::fs::create_dir_all(&folder);
            gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(&folder))).launch(app.window.get(), gtk::gio::Cancellable::NONE, |_| {});
        }
    ));
    header.pack_end(&open);

    let refresh = {
        let (app_w, active, grid, title) = (Rc::downgrade(app), active.clone(), grid.clone(), title.clone());
        Rc::new(move || {
            let Some(app) = app_w.upgrade() else { return };
            while let Some(c) = active.first_child() {
                active.remove(&c);
            }
            for (id, a) in app.downloads.borrow().iter() {
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&a.title))
                    .subtitle(glib::markup_escape_text(&a.status))
                    .build();
                let bar = gtk::ProgressBar::builder().fraction(a.fraction).valign(gtk::Align::Center).width_request(120).build();
                row.add_suffix(&bar);
                let cancel = gtk::Button::builder()
                    .icon_name("process-stop-symbolic")
                    .tooltip_text("Cancel")
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                let (app_w, id) = (Rc::downgrade(&app), id.clone());
                cancel.connect_clicked(move |_| {
                    if let Some(a) = app_w.upgrade() {
                        if let Some(h) = a.downloads.borrow().get(&id).and_then(|d| d.handle.as_ref()) {
                            h.cancel();
                        }
                    }
                });
                row.add_suffix(&cancel);
                active.append(&row);
            }
            active.set_visible(!app.downloads.borrow().is_empty());
            let done = db::open().and_then(|c| crate::download::list(&c)).unwrap_or_default();
            let bytes: u64 = done.iter().filter_map(|d| d.bytes).sum();
            title.set_subtitle(&format!("{} videos · {:.1} GB", done.len(), bytes as f64 / 1e9));
            grid.set_items(
                done.iter()
                    .map(|d| {
                        let mut it = d.item();
                        it.note = Some(format!("Downloaded {}{}", fmt::age(d.at), if d.audio_only { " · sound only" } else { "" }));
                        it
                    })
                    .collect(),
            );
        })
    };
    refresh();
    let r2 = refresh.clone();
    *app.on_downloads.borrow_mut() = Some(Rc::new(move || r2()));
    let app_w = Rc::downgrade(app);
    page.connect_hidden(move |_| {
        if let Some(a) = app_w.upgrade() {
            *a.on_downloads.borrow_mut() = None;
        }
    });
    page
}
