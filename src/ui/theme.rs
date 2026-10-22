//! Appearance: light/dark, colour themes, accent, corner rounding, card size,
//! animations, and your own CSS on top of all of it.
//!
//! Themes are just CSS that sets libadwaita's colour variables, so anything a
//! theme does you can also do (or undo) in `~/.config/yourtube/style.css`,
//! which reloads as soon as you save it.

use crate::config::Config;
use adw::prelude::*;
use gtk::{gdk, gio, glib};

/// (name, CSS). Each sets libadwaita colour variables; "System" sets nothing.
pub const THEMES: &[(&str, &str)] = &[
    ("System", ""),
    ("YourTube", ":root { --accent-bg-color: #ec4a36; --accent-color: #ff6b4f; }"),
    (
        "Midnight",
        ":root { --window-bg-color: #0f1320; --view-bg-color: #0b0f1a; --headerbar-bg-color: #141a2b;
                 --card-bg-color: rgba(255,255,255,0.05); --popover-bg-color: #1a2135; --dialog-bg-color: #1a2135;
                 --window-fg-color: #e6e9f2; --view-fg-color: #e6e9f2; --headerbar-fg-color: #e6e9f2;
                 --popover-fg-color: #e6e9f2; --dialog-fg-color: #e6e9f2;
                 --accent-bg-color: #6c7cff; --accent-color: #9aa5ff; }",
    ),
    (
        "Paper",
        ":root { --window-bg-color: #f6f1e7; --view-bg-color: #fbf7ef; --headerbar-bg-color: #efe7d8;
                 --card-bg-color: rgba(0,0,0,0.04); --popover-bg-color: #fbf7ef; --dialog-bg-color: #fbf7ef;
                 --window-fg-color: #2b2722; --view-fg-color: #2b2722; --headerbar-fg-color: #2b2722;
                 --popover-fg-color: #2b2722; --dialog-fg-color: #2b2722;
                 --accent-bg-color: #b5532b; --accent-color: #9a4321; }",
    ),
    (
        "Forest",
        ":root { --window-bg-color: #131a16; --view-bg-color: #0f1512; --headerbar-bg-color: #18211b;
                 --card-bg-color: rgba(255,255,255,0.05); --popover-bg-color: #1c2620; --dialog-bg-color: #1c2620;
                 --window-fg-color: #e3ece5; --view-fg-color: #e3ece5; --headerbar-fg-color: #e3ece5;
                 --popover-fg-color: #e3ece5; --dialog-fg-color: #e3ece5;
                 --accent-bg-color: #3f9b62; --accent-color: #6fcf8e; }",
    ),
    ("Grape", ":root { --accent-bg-color: #8f5bd8; --accent-color: #b48cf2; }"),
    ("Ocean", ":root { --accent-bg-color: #1c8ad6; --accent-color: #4fb0f0; }"),
    ("Mono", ":root { --accent-bg-color: #6b6b73; --accent-color: #9a9aa3; }"),
];

/// Themes that only look right in one colour scheme.
fn forced_scheme(theme: &str) -> Option<adw::ColorScheme> {
    match theme {
        "Midnight" | "Forest" => Some(adw::ColorScheme::ForceDark),
        "Paper" => Some(adw::ColorScheme::ForceLight),
        _ => None,
    }
}

pub const CARD_SIZES: &[(&str, i32)] = &[("Small", 220), ("Medium", 280), ("Large", 360)];

pub struct Theme {
    provider: gtk::CssProvider,
    user: gtk::CssProvider,
    _monitor: Option<gio::FileMonitor>,
}

impl Theme {
    pub fn install(display: &gdk::Display) -> Self {
        let provider = gtk::CssProvider::new();
        let user = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
        // Your own CSS wins over everything the app sets.
        gtk::style_context_add_provider_for_display(display, &user, gtk::STYLE_PROVIDER_PRIORITY_USER);
        let file = gio::File::for_path(user_css_path());
        let monitor = file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE).ok();
        if let Some(m) = &monitor {
            m.connect_changed(glib::clone!(
                #[weak]
                user,
                move |_, _, _, _| load_user_css(&user)
            ));
        }
        load_user_css(&user);
        Theme { provider, user, _monitor: monitor }
    }

    /// Apply the appearance settings (call again whenever they change).
    pub fn apply(&self, c: &Config, window: &impl IsA<gtk::Widget>) {
        let mut css = THEMES.iter().find(|(n, _)| *n == c.theme).map(|(_, css)| css.to_string()).unwrap_or_default();
        if let Some(hex) = c.accent.as_deref().filter(|h| valid_hex(h)) {
            css += &format!(":root {{ --accent-bg-color: {hex}; --accent-color: {hex}; }}");
        }
        css += &format!(
            ".yt-thumb {{ border-radius: {r}px; }} .yt-grid > child {{ border-radius: {r2}px; }}",
            r = c.corner_radius,
            r2 = c.corner_radius + 2
        );
        self.provider.load_from_string(&css);
        let scheme = forced_scheme(&c.theme).unwrap_or(match c.color_scheme.as_str() {
            "light" => adw::ColorScheme::ForceLight,
            "dark" => adw::ColorScheme::ForceDark,
            _ => adw::ColorScheme::Default,
        });
        adw::StyleManager::default().set_color_scheme(scheme);
        if c.animations {
            window.remove_css_class("yt-calm");
        } else {
            window.add_css_class("yt-calm");
        }
        let _ = &self.user;
    }
}

pub fn valid_hex(s: &str) -> bool {
    let s = s.trim();
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

pub fn user_css_path() -> std::path::PathBuf {
    crate::config::path().with_file_name("style.css")
}

fn load_user_css(p: &gtk::CssProvider) {
    p.load_from_string(&std::fs::read_to_string(user_css_path()).unwrap_or_default());
}

/// Create the custom CSS file with a few commented examples if it doesn't exist yet.
pub fn ensure_user_css() -> std::io::Result<std::path::PathBuf> {
    let p = user_css_path();
    if !p.exists() {
        std::fs::create_dir_all(p.parent().expect("config dir"))?;
        std::fs::write(&p, USER_CSS_TEMPLATE)?;
    }
    Ok(p)
}

const USER_CSS_TEMPLATE: &str = r#"/* Your own YourTube style. Saved changes show up right away.
 *
 * Colours use libadwaita's variables:
 * https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/css-variables.html
 *
 * Some ideas (remove the comment marks to try them):
 */

/* :root { --accent-bg-color: #ff6b6b; --accent-color: #ff8787; } */
/* :root { --window-bg-color: #101010; --view-bg-color: #0a0a0a; } */

/* Bigger titles on the cards */
/* .yt-grid .heading { font-size: 1.1em; } */

/* Square thumbnails corners */
/* .yt-thumb { border-radius: 0; } */

/* The "why this is here" line under For You picks */
/* .yt-note { color: #e5a50a; font-style: italic; } */
"#;

/// A few bundled symbolic icons the icon theme doesn't have.
pub fn install_icons(display: &gdk::Display) {
    let dir = crate::db::cache_dir().join("icons");
    let actions = dir.join("hicolor/scalable/actions");
    let _ = std::fs::create_dir_all(&actions);
    let apps = dir.join("hicolor/scalable/apps");
    let _ = std::fs::create_dir_all(&apps);
    for (sub, name, svg) in [
        (&actions, "yt-incognito-symbolic.svg", INCOGNITO_ICON),
        (&apps, "io.github.yourtube.YourTube.svg", include_str!("../../data/icons/io.github.yourtube.YourTube.svg")),
        (&apps, "io.github.yourtube.YourTube-symbolic.svg", include_str!("../../data/icons/io.github.yourtube.YourTube-symbolic.svg")),
    ] {
        let p = sub.join(name);
        if std::fs::read_to_string(&p).ok().as_deref() != Some(svg) {
            let _ = std::fs::write(&p, svg);
        }
    }
    gtk::IconTheme::for_display(display).add_search_path(&dir);
}

const INCOGNITO_ICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">
  <path fill="#2e3436" d="M5 1.5h6l1.6 4H3.4z"/>
  <rect fill="#2e3436" x="1" y="6" width="14" height="1.6" rx="0.8"/>
  <path fill="#2e3436" fill-rule="evenodd" d="M4.5 9a2.6 2.6 0 1 1 0 5.2a2.6 2.6 0 0 1 0-5.2zm0 1.3a1.3 1.3 0 1 0 0 2.6a1.3 1.3 0 0 0 0-2.6zM11.5 9a2.6 2.6 0 1 1 0 5.2a2.6 2.6 0 0 1 0-5.2zm0 1.3a1.3 1.3 0 1 0 0 2.6a1.3 1.3 0 0 0 0-2.6z"/>
  <rect fill="#2e3436" x="6.8" y="10.9" width="2.4" height="1" rx="0.5"/>
</svg>
"##;
