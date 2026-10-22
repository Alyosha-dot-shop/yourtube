//! The page about one video: buttons, channel, description, chapters, comments, related.

use super::{App, fmt};
use crate::db::{self, Item};
use crate::{thumbs, video};
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

pub fn page(app: &Rc<App>, it: &Item) -> adw::NavigationPage {
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(18)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    let clamp = adw::Clamp::builder().maximum_size(900).child(&body).build();
    let scroll = gtk::ScrolledWindow::builder().child(&clamp).hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build();
    let wt = adw::WindowTitle::new(&it.title, &it.channel);
    let header = adw::HeaderBar::builder().title_widget(&wt).build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&scroll));
    let page = adw::NavigationPage::builder().title(&it.title).child(&view).build();

    // Thumbnail and the main buttons show right away; the rest arrives from YouTube.
    let pic =
        gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).can_shrink(true).height_request(260).css_classes(["yt-thumb"]).build();
    let file = thumbs::path(&it.id);
    if file.exists() {
        pic.set_filename(Some(&file));
    } else {
        app.loader.request(&it.id);
        app.pictures.borrow_mut().insert(pic.clone(), it.id.clone());
    }
    body.append(&pic);

    let buttons =
        gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).max_children_per_line(6).column_spacing(6).row_spacing(6).build();
    let button = |label: &str, icon: &str, suggested: bool, f: fn(&Rc<App>, &Item)| {
        let content = adw::ButtonContent::builder().label(label).icon_name(icon).build();
        let b = gtk::Button::builder().child(&content).css_classes(["pill"]).build();
        if suggested {
            b.add_css_class("suggested-action");
        }
        let (app, it) = (Rc::downgrade(app), it.clone());
        b.connect_clicked(move |_| {
            if let Some(app) = app.upgrade() {
                f(&app, &it);
            }
        });
        buttons.append(&b);
    };
    button("Play", "media-playback-start-symbolic", true, |app, it| app.play(it));
    button("Listen", "audio-headphones-symbolic", false, App::listen);
    button("Download", "folder-download-symbolic", false, App::toggle_download);
    button("Watch later", "alarm-symbolic", false, App::toggle_watch_later);
    button("Playlist…", "view-list-symbolic", false, |app, it| super::pages::add_to_playlist_dialog(app, it));
    button("Copy link", "edit-copy-symbolic", false, App::copy_link);
    body.append(&buttons);

    let title = gtk::Label::builder().label(&it.title).wrap(true).xalign(0.0).selectable(true).css_classes(["title-2"]).build();
    body.append(&title);
    let rest = gtk::Box::new(gtk::Orientation::Vertical, 18);
    rest.append(&adw::Spinner::builder().height_request(32).build());
    body.append(&rest);

    let (id, want_dislikes, want_dearrow) = (it.id.clone(), app.config().dislikes, app.config().dearrow);
    App::bg(
        move || {
            let d = video::details(&id).map_err(|e| e.to_string());
            let votes = if want_dislikes { crate::extras::dislikes(&id).ok() } else { None };
            let branded = if want_dearrow { crate::extras::dearrow(&id).ok().and_then(|b| b.title) } else { None };
            (d, votes, branded)
        },
        glib::clone!(
            #[weak]
            app,
            #[weak]
            rest,
            #[weak]
            title,
            #[weak]
            wt,
            #[weak]
            page,
            move |(d, votes, branded): (Result<video::Details, String>, Option<crate::extras::Votes>, Option<String>)| {
                while let Some(c) = rest.first_child() {
                    rest.remove(&c);
                }
                match d {
                    Ok(d) => {
                        if let Some(b) = branded {
                            title.set_label(&b);
                            title.set_tooltip_text(Some(&format!("Original title: {}", d.title)));
                        } else {
                            title.set_label(&d.title);
                        }
                        wt.set_title(&d.title);
                        page.set_title(&d.title); // the tab's label
                        fill(&app, &rest, &d, votes);
                    }
                    Err(e) => {
                        rest.append(
                            &adw::StatusPage::builder()
                                .icon_name("dialog-warning-symbolic")
                                .title("Couldn't load the details")
                                .description(glib::markup_escape_text(&e))
                                .build(),
                        );
                        app.problem(crate::report::Code::guess(&e), &format!("Video page: {e}"));
                    }
                }
            }
        ),
    );
    page
}

fn fill(app: &Rc<App>, rest: &gtk::Box, d: &video::Details, votes: Option<crate::extras::Votes>) {
    let item = Item {
        id: d.id.clone(),
        title: d.title.clone(),
        channel: d.channel.clone(),
        channel_id: d.channel_id.clone(),
        published: d.published,
        views: d.views,
        length: d.length,
        ..Default::default()
    };

    // Channel and subscribe.
    let ch = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    ch.append(&adw::Avatar::builder().size(40).text(&d.channel).show_initials(true).build());
    let names = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let name_btn = gtk::Button::builder().label(&d.channel).css_classes(["flat", "heading"]).halign(gtk::Align::Start).build();
    let (cid, cname) = (d.channel_id.clone(), d.channel.clone());
    name_btn.connect_clicked(glib::clone!(
        #[weak]
        app,
        move |_| app.open_channel(&cid, &cname)
    ));
    names.append(&name_btn);
    if let Some(s) = &d.subscribers {
        names.append(&gtk::Label::builder().label(s).xalign(0.0).margin_start(10).css_classes(["dim-label", "caption"]).build());
    }
    names.set_hexpand(true);
    ch.append(&names);
    let subscribed = db::open().and_then(|c| db::is_subscribed(&c, &d.channel_id)).unwrap_or(false);
    let sub = gtk::Button::builder().label(if subscribed { "Unsubscribe" } else { "Subscribe" }).valign(gtk::Align::Center).build();
    if !subscribed {
        sub.add_css_class("suggested-action");
    }
    let (cid, cname) = (d.channel_id.clone(), d.channel.clone());
    sub.connect_clicked(glib::clone!(
        #[weak]
        app,
        move |b| {
            let on = b.label().as_deref() == Some("Subscribe");
            if let Ok(c) = db::open() {
                let _ = db::set_subscribed(&c, &cid, &cname, on);
            }
            b.set_label(if on { "Unsubscribe" } else { "Subscribe" });
            if on {
                b.remove_css_class("suggested-action");
            } else {
                b.add_css_class("suggested-action");
            }
            app.toast(&if on { format!("Subscribed to {cname}") } else { format!("Unsubscribed from {cname}") });
            app.tabs().load_feed();
        }
    ));
    ch.append(&sub);
    rest.append(&ch);

    // Numbers.
    let mut stats = Vec::new();
    if let Some(v) = d.views.filter(|_| !app.config().hide_views) {
        stats.push(fmt::views(v));
    }
    if let Some(p) = d.published {
        stats.push(fmt::age(p));
    } else if let Some(t) = &d.published_text {
        stats.push(t.clone());
    }
    match (&votes, d.likes) {
        (Some(v), _) => stats.push(format!("👍 {}  👎 {} (Return YouTube Dislike)", short(v.likes), short(v.dislikes))),
        (None, Some(l)) => stats.push(format!("👍 {}", short(l))),
        _ => {}
    }
    if d.live {
        stats.push("live now".into());
    }
    if let Some(why) = &d.unplayable {
        stats.push(format!("YouTube says: {why}"));
    }
    rest.append(&gtk::Label::builder().label(stats.join(" · ")).wrap(true).xalign(0.0).css_classes(["dim-label"]).build());

    // Description.
    if !d.description.trim().is_empty() {
        let text = gtk::Label::builder()
            .label(&d.description)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .selectable(true)
            .build();
        let exp = gtk::Expander::builder().label("Description").child(&text).expanded(d.description.len() < 400).build();
        rest.append(&exp);
    }

    // Chapters: click to play from there.
    if !d.chapters.is_empty() {
        let g = adw::PreferencesGroup::builder().title(format!("Chapters ({})", d.chapters.len())).build();
        for c in &d.chapters {
            let row =
                adw::ActionRow::builder().title(glib::markup_escape_text(&c.title)).subtitle(fmt::clock(c.start)).activatable(true).build();
            row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
            let (item, at) = (item.clone(), c.start);
            row.connect_activated(glib::clone!(
                #[weak]
                app,
                move |_| app.play_from(&item, at)
            ));
            g.add(&row);
        }
        rest.append(&g);
    }

    // Comments.
    if let Some(token) = d.comments.clone() {
        rest.append(&comments_section(app, &d.id, token, d.comment_count.clone()));
    } else {
        rest.append(&gtk::Label::builder().label("Comments are turned off for this video.").xalign(0.0).css_classes(["dim-label"]).build());
    }

    // Related videos.
    if !d.related.is_empty() {
        let g = adw::PreferencesGroup::builder().title("Up next").build();
        for v in d.related.iter().take(20) {
            let it = Item::from(v.clone());
            let mut sub = vec![it.channel.clone()];
            if let Some(n) = it.views {
                sub.push(fmt::views(n));
            }
            if let Some(l) = it.length {
                sub.push(fmt::clock(l));
            }
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&it.title))
                .subtitle(glib::markup_escape_text(&sub.join(" · ")))
                .activatable(true)
                .build();
            let play = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .tooltip_text("Play")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let it2 = it.clone();
            play.connect_clicked(glib::clone!(
                #[weak]
                app,
                move |_| app.play(&it2)
            ));
            row.add_suffix(&play);
            row.connect_activated(glib::clone!(
                #[weak]
                app,
                move |_| app.open_video(&it)
            ));
            g.add(&row);
        }
        rest.append(&g);
    }
}

fn short(n: i64) -> String {
    fmt::views(n).trim_end_matches(" views").trim_end_matches(" view").to_owned()
}

fn comments_section(app: &Rc<App>, video_id: &str, _token: String, count: Option<String>) -> gtk::Box {
    let col = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let heading = gtk::Label::builder()
        .label(match count {
            Some(n) => format!("Comments ({n})"),
            None => "Comments".into(),
        })
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["title-4"])
        .build();
    top.append(&heading);
    let sort = gtk::DropDown::from_strings(&["Top", "Newest"]);
    top.append(&sort);
    col.append(&top);
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    col.append(&list);
    let more = gtk::Button::builder().label("Show more comments").halign(gtk::Align::Center).css_classes(["pill"]).visible(false).build();
    col.append(&more);

    let next: Rc<std::cell::RefCell<Option<String>>> = Rc::default();
    let load = {
        let (app, list, more, next) = (Rc::downgrade(app), list.downgrade(), more.downgrade(), next.clone());
        Rc::new(move |source: String, sort: video::Sort, clear: bool| {
            let (list2, more2, next2, app2) = (list.clone(), more.clone(), next.clone(), app.clone());
            if let Some(m) = more.upgrade() {
                m.set_sensitive(false);
            }
            App::bg(
                move || video::comments(&source, sort).map_err(|e| e.to_string()),
                move |r: Result<video::CommentPage, String>| {
                    let (Some(list), Some(more)) = (list2.upgrade(), more2.upgrade()) else { return };
                    more.set_sensitive(true);
                    if clear {
                        while let Some(c) = list.first_child() {
                            list.remove(&c);
                        }
                    }
                    match r {
                        Ok(p) => {
                            for c in &p.comments {
                                list.append(&comment_row(c, &app2));
                            }
                            more.set_visible(p.next.is_some());
                            *next2.borrow_mut() = p.next;
                        }
                        Err(e) => {
                            list.append(
                                &adw::ActionRow::builder().title("Couldn't load comments").subtitle(glib::markup_escape_text(&e)).build(),
                            );
                            more.set_visible(false);
                        }
                    }
                },
            );
        })
    };
    load(video_id.to_owned(), video::Sort::Top, true);
    let (load2, id) = (load.clone(), video_id.to_owned());
    sort.connect_selected_notify(move |s| load2(id.clone(), if s.selected() == 1 { video::Sort::Newest } else { video::Sort::Top }, true));
    more.connect_clicked(move |_| {
        if let Some(tok) = next.borrow().clone() {
            load(tok, video::Sort::Top, false);
        }
    });
    col
}

fn comment_row(c: &video::Comment, app: &std::rc::Weak<App>) -> gtk::Widget {
    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    let mut head = vec![c.author.clone()];
    if !c.published.is_empty() {
        head.push(c.published.clone());
    }
    if !c.likes_text.is_empty() && c.likes_text != "0" {
        head.push(format!("👍 {}", c.likes_text));
    }
    if c.pinned {
        head.push("📌 pinned".into());
    }
    if c.hearted {
        head.push("❤ by the creator".into());
    }
    b.append(&gtk::Label::builder().label(head.join(" · ")).xalign(0.0).wrap(true).css_classes(["caption-heading"]).build());
    b.append(
        &gtk::Label::builder().label(&c.text).xalign(0.0).wrap(true).wrap_mode(gtk::pango::WrapMode::WordChar).selectable(true).build(),
    );
    if let (Some(tok), n) = (c.replies.clone(), c.reply_count) {
        let btn = gtk::Button::builder()
            .label(if n == 1 { "1 reply".to_owned() } else { format!("{n} replies") })
            .halign(gtk::Align::Start)
            .css_classes(["flat", "caption"])
            .build();
        let replies = gtk::Box::builder().orientation(gtk::Orientation::Vertical).margin_start(18).build();
        let app = app.clone();
        btn.connect_clicked(glib::clone!(
            #[weak]
            replies,
            move |btn| {
                btn.set_sensitive(false);
                let (tok, app) = (tok.clone(), app.clone());
                App::bg(
                    move || video::replies(&tok).map_err(|e| e.to_string()),
                    glib::clone!(
                        #[weak]
                        replies,
                        #[weak]
                        btn,
                        move |r: Result<video::CommentPage, String>| {
                            btn.set_visible(false);
                            match r {
                                Ok(p) => {
                                    for c in &p.comments {
                                        replies.append(&comment_row(c, &app));
                                    }
                                }
                                Err(e) => replies.append(&gtk::Label::new(Some(&format!("Couldn't load replies: {e}")))),
                            }
                        }
                    ),
                );
            }
        ));
        b.append(&btn);
        b.append(&replies);
    }
    b.upcast()
}
