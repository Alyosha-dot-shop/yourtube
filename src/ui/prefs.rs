//! Preferences: For You and AI, appearance, profiles, playback.
//! Appearance changes show up immediately; everything is saved when the window closes.

use super::{App, theme};
use crate::config::Config;
use crate::db;
use crate::foryou::ai;
use adw::prelude::*;
use gtk::{gdk, glib};
use std::cell::RefCell;
use std::rc::Rc;

/// (label, base URL). Local ones first: they're the ones we recommend.
const AI_PRESETS: &[(&str, &str)] = &[
    ("Ollama (on this computer)", "http://127.0.0.1:11434/v1"),
    ("LM Studio (on this computer)", "http://127.0.0.1:1234/v1"),
    ("llama.cpp server (on this computer)", "http://127.0.0.1:8080/v1"),
    ("OpenAI (cloud)", "https://api.openai.com/v1"),
    ("Anthropic Claude (cloud)", "https://api.anthropic.com/v1"),
    ("OpenRouter (cloud)", "https://openrouter.ai/api/v1"),
    ("Other", ""),
];

const QUALITIES: &[u32] = &[480, 720, 1080, 1440, 2160];
const SCHEMES: &[(&str, &str)] = &[("system", "Follow the system"), ("light", "Light"), ("dark", "Dark")];

const LOCAL_MODELS: &str = "We recommend a model on your own computer: then nothing leaves it. Small models are plenty for this — \
they only pick a few searches and sort about 40 titles.\n\
• 1–4B models (Qwen3 4B, Gemma 3 4B, Llama 3.2 3B): about 3 GB of memory, fine on any recent laptop's CPU; a refresh takes under a minute.\n\
• 7–8B models (Qwen3 8B, Llama 3.1 8B): about 6 GB, nicer picks; best with 16 GB of RAM or a graphics card with 6 GB+.\n\
Easiest start: install Ollama, run “ollama pull qwen3:4b”, then choose Ollama below and type qwen3:4b.";

pub fn show(app: &Rc<App>) {
    let start = app.config();
    // The settings being edited; appearance changes apply live.
    let edit = Rc::new(RefCell::new(start.clone()));
    let dialog = adw::PreferencesDialog::builder().title("Preferences").search_enabled(true).build();

    dialog.add(&for_you_page(app, &dialog, &edit));
    dialog.add(&appearance_page(app, &edit));
    dialog.add(&super::shortcuts::prefs_page(app, &edit));
    dialog.add(&profiles_page(app, &dialog));
    dialog.add(&playback_page(&edit));
    dialog.add(&privacy_page(&edit));

    dialog.connect_closed(glib::clone!(
        #[weak]
        app,
        move |_| {
            let new = edit.borrow().clone();
            if new.ai_enabled && new.ai().is_none() {
                app.toast("AI stays off until it has an http(s) address and a model");
            }
            if new != start {
                // Live previews already touched the running config; compare against how it was.
                *app.config.borrow_mut() = start.clone();
                app.set_config(new);
            }
        }
    ));
    dialog.present(app.window.get());
}

/// Live-apply an appearance change.
fn live(app: &Rc<App>, edit: &Rc<RefCell<Config>>, f: impl FnOnce(&mut Config)) {
    f(&mut edit.borrow_mut());
    *app.config.borrow_mut() = edit.borrow().clone();
    app.apply_appearance();
}

fn for_you_page(app: &Rc<App>, dialog: &adw::PreferencesDialog, edit: &Rc<RefCell<Config>>) -> adw::PreferencesPage {
    let c = edit.borrow().clone();
    let page = adw::PreferencesPage::builder().title("For You").icon_name("starred-symbolic").build();

    let source = adw::PreferencesGroup::builder()
        .title("Recommendations")
        .description("For You is made on this computer from what you search and watch in YourTube. Nothing leaves it unless you turn on AI with a cloud service.")
        .build();
    let external = adw::SwitchRow::builder()
        .title("Use your own recommender service")
        .subtitle("Something you run that speaks the For You API (see the README)")
        .active(c.recommender.is_some())
        .build();
    let external_url = adw::EntryRow::builder().title("Recommender address").text(c.recommender.clone().unwrap_or_default()).build();
    external.bind_property("active", &external_url, "sensitive").sync_create().build();
    let sync_external = glib::clone!(
        #[strong]
        edit,
        #[weak]
        external,
        #[weak]
        external_url,
        move || {
            let url = external_url.text().trim().to_owned();
            edit.borrow_mut().recommender =
                (external.is_active() && (url.starts_with("http://") || url.starts_with("https://"))).then_some(url);
        }
    );
    let s1 = sync_external.clone();
    external.connect_active_notify(move |_| s1());
    external_url.connect_changed(move |_| sync_external());
    source.add(&external);
    source.add(&external_url);
    page.add(&source);

    let ai_group = adw::PreferencesGroup::builder()
        .title("AI help (optional)")
        .description(
            "The AI suggests new things to explore and picks the best videos, with a reason for each. When on, each For You \
             refresh sends your recent searches, titles and channels of videos you watched lately, and titles of candidate \
             videos to the service below.",
        )
        .build();
    let ai_on = adw::SwitchRow::builder().title("Use AI for For You").active(c.ai_enabled).build();
    let names: Vec<&str> = AI_PRESETS.iter().map(|(n, _)| *n).collect();
    let preset = adw::ComboRow::builder()
        .title("Service")
        .model(&gtk::StringList::new(&names))
        .selected(AI_PRESETS.iter().position(|(_, u)| !u.is_empty() && *u == c.ai_url).unwrap_or(if c.ai_url.is_empty() {
            0
        } else {
            AI_PRESETS.len() - 1
        }) as u32)
        .build();
    let url =
        adw::EntryRow::builder().title("Address (ends in /v1)").text(if c.ai_url.is_empty() { AI_PRESETS[0].1 } else { &c.ai_url }).build();
    let model = adw::EntryRow::builder().title("Model (e.g. qwen3:4b, llama3.2)").text(&c.ai_model).build();
    let key = adw::PasswordEntryRow::builder().title("API key (cloud services only)").text(&c.ai_api_key).build();
    let test = adw::ButtonRow::builder().title("Test connection").build();
    let cloud_note = gtk::Label::builder()
        .label("Heads-up: this is a cloud service, so your recent searches and watched titles leave your computer when For You refreshes.")
        .wrap(true)
        .xalign(0.0)
        .css_classes(["caption", "warning"])
        .margin_top(6)
        .build();
    let update_cloud = glib::clone!(
        #[weak]
        url,
        #[weak]
        cloud_note,
        move || {
            let u = url.text();
            let local = u.contains("://127.") || u.contains("://localhost") || u.contains("://[::1]");
            cloud_note.set_visible(!u.is_empty() && !local);
        }
    );
    update_cloud();
    preset.connect_selected_notify(glib::clone!(
        #[weak]
        url,
        move |p| {
            if let Some((_, u)) = AI_PRESETS.get(p.selected() as usize).filter(|(_, u)| !u.is_empty()) {
                url.set_text(u);
            }
        }
    ));
    let sync_ai = glib::clone!(
        #[strong]
        edit,
        #[weak]
        ai_on,
        #[weak]
        url,
        #[weak]
        model,
        #[weak]
        key,
        move || {
            let mut e = edit.borrow_mut();
            e.ai_enabled = ai_on.is_active();
            e.ai_url = url.text().trim().to_owned();
            e.ai_model = model.text().trim().to_owned();
            e.ai_api_key = key.text().trim().to_owned();
        }
    );
    for row in [&url, &model] {
        let (s, u) = (sync_ai.clone(), update_cloud.clone());
        row.connect_changed(move |_| {
            s();
            u();
        });
    }
    let s = sync_ai.clone();
    key.connect_changed(move |_| s());
    ai_on.connect_active_notify(move |_| sync_ai());
    for w in [preset.upcast_ref::<gtk::Widget>(), url.upcast_ref(), model.upcast_ref(), key.upcast_ref(), test.upcast_ref()] {
        ai_on.bind_property("active", w, "sensitive").sync_create().build();
    }
    test.connect_activated(glib::clone!(
        #[weak]
        dialog,
        #[strong]
        edit,
        move |row| {
            let Some(cfg) = Config { ai_enabled: true, ..edit.borrow().clone() }.ai() else {
                dialog.add_toast(adw::Toast::new("Fill in an http(s) address and a model first"));
                return;
            };
            row.set_sensitive(false);
            let row = row.clone();
            App::bg(
                move || ai::test(&cfg).map_err(|e| e.to_string()),
                move |r: Result<String, String>| {
                    row.set_sensitive(true);
                    let msg = match r {
                        Ok(m) => format!("Works: {m}"),
                        Err(e) => format!("Didn't work: {e}"),
                    };
                    dialog.add_toast(adw::Toast::builder().title(glib::markup_escape_text(&msg)).timeout(6).build());
                },
            );
        }
    ));
    for w in
        [ai_on.upcast_ref::<gtk::Widget>(), preset.upcast_ref(), url.upcast_ref(), model.upcast_ref(), key.upcast_ref(), test.upcast_ref()]
    {
        ai_group.add(w);
    }
    ai_group.add(&cloud_note);
    page.add(&ai_group);

    let local = adw::PreferencesGroup::builder().title("Which model?").build();
    local.add(&gtk::Label::builder().label(LOCAL_MODELS).wrap(true).xalign(0.0).selectable(true).css_classes(["dim-label"]).build());
    page.add(&local);

    // Hidden channels.
    let hidden = adw::PreferencesGroup::builder()
        .title("Hidden channels")
        .description("Channels you chose not to see. They're left out of For You, search and related videos.")
        .build();
    let blocked = db::open().and_then(|c| db::blocked_channels(&c)).unwrap_or_default();
    if blocked.is_empty() {
        hidden.add(
            &adw::ActionRow::builder()
                .title("None yet — right-click a video → “Don't show this channel”, or press B")
                .sensitive(false)
                .build(),
        );
    }
    for (id, name) in blocked {
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(if name.is_empty() { &id } else { &name })).build();
        let unhide = gtk::Button::builder().label("Unhide").valign(gtk::Align::Center).build();
        unhide.connect_clicked(glib::clone!(
            #[weak]
            app,
            #[weak]
            row,
            move |_| {
                app.unblock_channel(&id, &name, false);
                row.set_sensitive(false);
            }
        ));
        row.add_suffix(&unhide);
        hidden.add(&row);
    }
    page.add(&hidden);
    page
}

fn appearance_page(app: &Rc<App>, edit: &Rc<RefCell<Config>>) -> adw::PreferencesPage {
    let c = edit.borrow().clone();
    let page = adw::PreferencesPage::builder().title("Appearance").icon_name("applications-graphics-symbolic").build();
    let look = adw::PreferencesGroup::builder().title("Look").build();

    let scheme_names: Vec<&str> = SCHEMES.iter().map(|(_, l)| *l).collect();
    let scheme = adw::ComboRow::builder()
        .title("Style")
        .model(&gtk::StringList::new(&scheme_names))
        .selected(SCHEMES.iter().position(|(k, _)| *k == c.color_scheme).unwrap_or(0) as u32)
        .build();
    scheme.connect_selected_notify(glib::clone!(
        #[weak]
        app,
        #[strong]
        edit,
        move |r| live(&app, &edit, |c| c.color_scheme = SCHEMES[r.selected() as usize].0.to_owned())
    ));

    let theme_names: Vec<&str> = theme::THEMES.iter().map(|(n, _)| *n).collect();
    let theme_row = adw::ComboRow::builder()
        .title("Theme")
        .subtitle("Midnight and Forest are always dark, Paper always light")
        .model(&gtk::StringList::new(&theme_names))
        .selected(theme::THEMES.iter().position(|(n, _)| *n == c.theme).unwrap_or(0) as u32)
        .build();
    theme_row.connect_selected_notify(glib::clone!(
        #[weak]
        app,
        #[strong]
        edit,
        move |r| live(&app, &edit, |c| c.theme = theme::THEMES[r.selected() as usize].0.to_owned())
    ));

    let accent_row = adw::ActionRow::builder().title("Accent colour").subtitle("Overrides the theme's").build();
    let color =
        gtk::ColorDialogButton::builder().dialog(&gtk::ColorDialog::builder().with_alpha(false).build()).valign(gtk::Align::Center).build();
    if let Some(rgba) = c.accent.as_deref().and_then(|h| gdk::RGBA::parse(h).ok()) {
        color.set_rgba(&rgba);
    }
    let reset = gtk::Button::builder()
        .icon_name("edit-undo-symbolic")
        .tooltip_text("Use the theme's accent")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    color.connect_rgba_notify(glib::clone!(
        #[weak]
        app,
        #[strong]
        edit,
        move |b| {
            let c = b.rgba();
            let hex = format!("#{:02x}{:02x}{:02x}", (c.red() * 255.0) as u8, (c.green() * 255.0) as u8, (c.blue() * 255.0) as u8);
            live(&app, &edit, |c| c.accent = Some(hex));
        }
    ));
    reset.connect_clicked(glib::clone!(
        #[weak]
        app,
        #[strong]
        edit,
        move |_| live(&app, &edit, |c| c.accent = None)
    ));
    accent_row.add_suffix(&reset);
    accent_row.add_suffix(&color);

    let radius = adw::SpinRow::builder()
        .title("Corner rounding")
        .adjustment(&gtk::Adjustment::new(c.corner_radius as f64, 0.0, 24.0, 1.0, 4.0, 0.0))
        .build();
    radius.connect_value_notify(glib::clone!(
        #[weak]
        app,
        #[strong]
        edit,
        move |r| live(&app, &edit, |c| c.corner_radius = r.value() as u32)
    ));

    let size_names: Vec<&str> = theme::CARD_SIZES.iter().map(|(n, _)| *n).collect();
    let size = adw::ComboRow::builder()
        .title("Video size")
        .model(&gtk::StringList::new(&size_names))
        .selected(theme::CARD_SIZES.iter().position(|(n, _)| *n == c.card_size).unwrap_or(1) as u32)
        .build();
    size.connect_selected_notify(glib::clone!(
        #[weak]
        app,
        #[strong]
        edit,
        move |r| live(&app, &edit, |c| c.card_size = theme::CARD_SIZES[r.selected() as usize].0.to_owned())
    ));

    let anim = adw::SwitchRow::builder()
        .title("Animations")
        .subtitle("Fades, page transitions and the spinning refresh icon")
        .active(c.animations)
        .build();
    anim.connect_active_notify(glib::clone!(
        #[weak]
        app,
        #[strong]
        edit,
        move |r| live(&app, &edit, |c| c.animations = r.is_active())
    ));

    for w in [
        scheme.upcast_ref::<gtk::Widget>(),
        theme_row.upcast_ref(),
        accent_row.upcast_ref(),
        radius.upcast_ref(),
        size.upcast_ref(),
        anim.upcast_ref(),
    ] {
        look.add(w);
    }
    page.add(&look);

    let own = adw::PreferencesGroup::builder()
        .title("Your own style")
        .description(format!(
            "Everything above is CSS, and you can add your own on top. It reloads the moment you save.\n{}",
            theme::user_css_path().display()
        ))
        .build();
    let open = adw::ButtonRow::builder().title("Edit your CSS…").end_icon_name("document-edit-symbolic").build();
    open.connect_activated(glib::clone!(
        #[weak]
        app,
        move |_| match theme::ensure_user_css() {
            Ok(p) =>
                gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(p))).launch(app.window.get(), gtk::gio::Cancellable::NONE, |_| {}),
            Err(e) => app.toast(&format!("Couldn't create the CSS file: {e}")),
        }
    ));
    own.add(&open);
    page.add(&own);
    page
}

fn profiles_page(app: &Rc<App>, dialog: &adw::PreferencesDialog) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder().title("Profiles").icon_name("system-users-symbolic").build();
    let group = adw::PreferencesGroup::builder()
        .title("Profiles")
        .description("Each profile has its own subscriptions, history, playlists and For You — handy when people share a computer.")
        .build();
    let current = db::profile_name();
    for name in db::profiles() {
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(&name)).build();
        row.add_prefix(&adw::Avatar::builder().size(32).text(&name).show_initials(true).build());
        if name == current {
            row.set_subtitle("In use");
        } else {
            let use_btn = gtk::Button::builder().label("Switch").valign(gtk::Align::Center).build();
            use_btn.connect_clicked(glib::clone!(
                #[weak]
                app,
                #[weak]
                dialog,
                #[strong]
                name,
                move |_| {
                    dialog.close();
                    app.switch_profile(&name);
                }
            ));
            row.add_suffix(&use_btn);
        }
        if name != db::DEFAULT_PROFILE && name != current {
            let rename = gtk::Button::builder()
                .icon_name("document-edit-symbolic")
                .tooltip_text("Rename")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            rename.connect_clicked(glib::clone!(
                #[weak]
                app,
                #[weak]
                dialog,
                #[strong]
                name,
                move |_| {
                    dialog.close();
                    let old = name.clone();
                    super::pages::name_dialog(&app, "Rename profile", &name, "Rename", move |app, new| {
                        match db::rename_profile(&old, &new) {
                            Ok(_) => {
                                app.rebuild_profile_menu();
                                app.toast(&format!("Renamed to {new}"));
                            }
                            Err(e) => app.toast(&e.to_string()),
                        }
                    });
                }
            ));
            row.add_suffix(&rename);
            let del = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Delete profile")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            del.connect_clicked(glib::clone!(
                #[weak]
                app,
                #[weak]
                dialog,
                #[weak]
                row,
                #[strong]
                name,
                move |_| {
                    let d = adw::AlertDialog::new(
                        Some(&format!("Delete {name}?")),
                        Some("Its subscriptions, history, playlists and For You are deleted for good."),
                    );
                    d.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
                    d.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
                    let name = name.clone();
                    d.connect_response(
                        Some("delete"),
                        glib::clone!(
                            #[weak]
                            app,
                            #[weak]
                            row,
                            move |_, _| match db::delete_profile(&name) {
                                Ok(()) => {
                                    row.set_visible(false);
                                    app.rebuild_profile_menu();
                                }
                                Err(e) => app.toast(&e.to_string()),
                            }
                        ),
                    );
                    d.present(Some(&dialog));
                }
            ));
            row.add_suffix(&del);
        }
        group.add(&row);
    }
    let add = adw::ButtonRow::builder().title("New profile…").start_icon_name("list-add-symbolic").build();
    add.connect_activated(glib::clone!(
        #[weak]
        app,
        #[weak]
        dialog,
        move |_| {
            dialog.close();
            super::pages::new_profile_dialog(&app);
        }
    ));
    group.add(&add);
    page.add(&group);

    let incog = adw::PreferencesGroup::builder()
        .title("Incognito")
        .description(
            "Incognito (Ctrl+Shift+N, or the mask button) saves nothing about what you watch or search: no history, no resume \
             points, no For You clicks. It lasts until you turn it off or close YourTube. Thumbnails are still cached.",
        )
        .build();
    page.add(&incog);
    page
}

/// A switch bound to one setting.
fn switch(edit: &Rc<RefCell<Config>>, title: &str, subtitle: &str, field: fn(&mut Config) -> &mut bool) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder().title(title).subtitle(subtitle).active(*field(&mut edit.borrow_mut())).build();
    let edit = edit.clone();
    row.connect_active_notify(move |r| *field(&mut edit.borrow_mut()) = r.is_active());
    row
}

/// A text field bound to one setting.
fn entry(edit: &Rc<RefCell<Config>>, title: &str, field: fn(&mut Config) -> &mut String) -> adw::EntryRow {
    let row = adw::EntryRow::builder().title(title).text(field(&mut edit.borrow_mut()).as_str()).build();
    let edit = edit.clone();
    row.connect_changed(move |r| *field(&mut edit.borrow_mut()) = r.text().trim().to_owned());
    row
}

fn playback_page(edit: &Rc<RefCell<Config>>) -> adw::PreferencesPage {
    let c = edit.borrow().clone();
    let page = adw::PreferencesPage::builder().title("Playback").icon_name("media-playback-start-symbolic").build();
    let group = adw::PreferencesGroup::builder()
        .title("Video")
        .description(
            "Videos play in mpv, through yt-dlp. mpv's own keys work in its window (space, arrows, f, j for subtitles, [ ] for speed…).",
        )
        .build();
    let labels: Vec<String> = QUALITIES.iter().map(|q| format!("Up to {q}p")).collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let quality = adw::ComboRow::builder()
        .title("Quality")
        .model(&gtk::StringList::new(&refs))
        .selected(QUALITIES.iter().position(|q| *q >= c.max_height).unwrap_or(2) as u32)
        .build();
    quality.connect_selected_notify(glib::clone!(
        #[strong]
        edit,
        move |r| edit.borrow_mut().max_height = QUALITIES.get(r.selected() as usize).copied().unwrap_or(1080)
    ));
    group.add(&quality);
    let speed = adw::SpinRow::builder()
        .title("Starting speed")
        .digits(2)
        .adjustment(&gtk::Adjustment::new(c.speed, 0.25, 4.0, 0.25, 0.5, 0.0))
        .build();
    speed.connect_value_notify(glib::clone!(
        #[strong]
        edit,
        move |r| edit.borrow_mut().speed = r.value()
    ));
    group.add(&speed);
    group.add(&switch(edit, "Autoplay", "Play the next video of a playlist or queue on its own", |c| &mut c.autoplay));
    group.add(&entry(edit, "Subtitle languages (e.g. en,de — empty for none)", |c| &mut c.subtitles));
    page.add(&group);

    let sb = adw::PreferencesGroup::builder()
        .title("SponsorBlock")
        .description("Skips sponsors and other segments, marked by the community at sponsor.ajay.app. YourTube only sends the first characters of a hash of the video's id, so they can't see what you watch.")
        .build();
    let sb_on = switch(edit, "Use SponsorBlock", "", |c| &mut c.sponsorblock);
    sb.add(&sb_on);
    let modes = ["Skip", "Ask", "Off"];
    for (id, label, _) in crate::config::SPONSORBLOCK_DEFAULTS {
        let cur = c.sponsorblock_categories.get(*id).map(String::as_str).unwrap_or("off");
        let row = adw::ComboRow::builder()
            .title(*label)
            .model(&gtk::StringList::new(&modes))
            .selected(["skip", "ask", "off"].iter().position(|m| *m == cur).unwrap_or(2) as u32)
            .build();
        let (edit2, id) = (edit.clone(), id.to_string());
        row.connect_selected_notify(move |r| {
            edit2.borrow_mut().sponsorblock_categories.insert(id.clone(), ["skip", "ask", "off"][r.selected() as usize].to_owned());
        });
        sb_on.bind_property("active", &row, "sensitive").sync_create().build();
        sb.add(&row);
    }
    page.add(&sb);

    let extras = adw::PreferencesGroup::builder().title("Community extras").build();
    extras.add(&switch(edit, "DeArrow", "Less clickbaity titles and thumbnails, picked by the community (dearrow.ajay.app)", |c| {
        &mut c.dearrow
    }));
    extras.add(&switch(
        edit,
        "Show dislikes",
        "Estimated dislike counts from Return YouTube Dislike (asks their server about each video you open)",
        |c| &mut c.dislikes,
    ));
    page.add(&extras);

    let dl = adw::PreferencesGroup::builder()
        .title("Downloads")
        .description("Saved with yt-dlp; downloaded videos play from your disk, even offline.")
        .build();
    let folder = adw::ActionRow::builder()
        .title("Folder")
        .subtitle(c.download_dir.clone().unwrap_or_else(|| dirs::video_dir().unwrap_or_default().join("YourTube").display().to_string()))
        .activatable(true)
        .build();
    folder.add_suffix(&gtk::Image::from_icon_name("folder-open-symbolic"));
    folder.connect_activated(glib::clone!(
        #[strong]
        edit,
        move |row| {
            let row = row.clone();
            let edit = edit.clone();
            gtk::FileDialog::builder().title("Save downloads to…").build().select_folder(
                row.root().and_downcast_ref::<gtk::Window>(),
                gtk::gio::Cancellable::NONE,
                move |r| {
                    if let Some(p) = r.ok().and_then(|f| f.path()) {
                        row.set_subtitle(&p.display().to_string());
                        edit.borrow_mut().download_dir = Some(p.display().to_string());
                    }
                },
            );
        }
    ));
    dl.add(&folder);
    let dlq = adw::ComboRow::builder()
        .title("Download quality")
        .model(&gtk::StringList::new(&refs))
        .selected(QUALITIES.iter().position(|q| *q >= c.download_height).unwrap_or(2) as u32)
        .build();
    dlq.connect_selected_notify(glib::clone!(
        #[strong]
        edit,
        move |r| edit.borrow_mut().download_height = QUALITIES.get(r.selected() as usize).copied().unwrap_or(1080)
    ));
    dl.add(&dlq);
    dl.add(&switch(edit, "Cut sponsors out of downloads", "Uses your SponsorBlock “Skip” categories", |c| {
        &mut c.download_cut_sponsors
    }));
    page.add(&dl);

    let net = adw::PreferencesGroup::builder()
        .title("Network")
        .description("Send everything YourTube, yt-dlp and mpv fetch through a proxy, for example Tor at socks5h://127.0.0.1:9050.")
        .build();
    let proxy = adw::EntryRow::builder().title("Proxy (empty for none)").text(c.proxy.clone().unwrap_or_default()).build();
    proxy.connect_changed(glib::clone!(
        #[strong]
        edit,
        move |r| {
            let v = r.text().trim().to_owned();
            edit.borrow_mut().proxy = v.contains("://").then_some(v);
        }
    ));
    net.add(&proxy);
    page.add(&net);
    page
}

/// Privacy, distractions and the web version.
fn privacy_page(edit: &Rc<RefCell<Config>>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder().title("Privacy").icon_name("security-high-symbolic").build();
    let g = adw::PreferencesGroup::builder()
        .title("What YourTube remembers")
        .description("All of it stays on this computer. Incognito (Ctrl+Shift+N) turns everything off for a while.")
        .build();
    g.add(&switch(edit, "Remember what I watch", "History and where you left off; For You learns from it", |c| &mut c.remember_history));
    g.add(&switch(edit, "Remember what I search", "For You leans on your searches most", |c| &mut c.remember_searches));
    page.add(&g);
    let d = adw::PreferencesGroup::builder().title("Fewer distractions").build();
    d.add(&switch(edit, "Hide view counts", "", |c| &mut c.hide_views));
    d.add(&switch(edit, "Hide live streams and premieres", "", |c| &mut c.hide_live));
    page.add(&d);
    let w = adw::PreferencesGroup::builder()
        .title("Web version")
        .description("Run “yourtube serve” on a computer that stays on (a Raspberry Pi, an old laptop) and open it from any phone or computer at home. Set a password if other people use your network.")
        .build();
    w.add(&entry(edit, "Address and port (0.0.0.0:8723 for the whole network)", |c| &mut c.web_address));
    let pw = adw::PasswordEntryRow::builder().title("Password").text(edit.borrow().web_password.as_str()).build();
    pw.connect_changed(glib::clone!(
        #[strong]
        edit,
        move |r| edit.borrow_mut().web_password = r.text().to_string()
    ));
    w.add(&pw);
    page.add(&w);
    page
}
