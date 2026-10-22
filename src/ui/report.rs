//! "Report a problem" (Ctrl+Shift+B): screenshot + what happened + error codes → a GitHub issue.

use super::App;
use crate::report;
use adw::prelude::*;
use gtk::{gdk, glib};
use std::rc::Rc;

/// Where screenshots and reports are kept until you send them.
fn reports_dir() -> std::path::PathBuf {
    dirs::state_dir().or_else(dirs::data_local_dir).unwrap_or_else(std::env::temp_dir).join("yourtube").join("reports")
}

/// A picture of the window exactly as it looks right now.
pub fn screenshot(widget: &gtk::Widget) -> Option<gdk::Texture> {
    let (w, h) = (widget.width(), widget.height());
    if w <= 0 || h <= 0 {
        return None;
    }
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snap = gtk::Snapshot::new();
    paintable.snapshot(&snap, w as f64, h as f64);
    let node = snap.to_node()?;
    Some(widget.native()?.renderer()?.render_texture(&node, None))
}

pub fn show(app: &Rc<App>) {
    let Some(window) = app.window.get() else { return };
    // Take the picture before the dialog covers anything.
    let shot = screenshot(window.upcast_ref());
    let page = match app.nav().visible_page().and_then(|p| p.tag()).as_deref() {
        Some("main") => app.stack.visible_child_name().map(|s| s.to_string()).unwrap_or_default(),
        _ => app.nav().visible_page().map(|p| p.title().to_string()).unwrap_or_default(),
    };
    let problems = report::recent();
    let code = problems.last().map_or("none yet", |p| p.code.id());

    let d = adw::AlertDialog::new(
        Some("Report a problem"),
        Some(&format!(
            "This opens a GitHub issue with what happened, the error codes ({code}) and your system details. \
             No history, searches, keys or passwords are included, and you see it all before sending."
        )),
    );
    let col = gtk::Box::new(gtk::Orientation::Vertical, 10);
    let what = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .height_request(90)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();
    what.buffer().set_text("");
    let frame = gtk::Frame::builder().child(&what).build();
    col.append(&gtk::Label::builder().label("What happened? What did you expect?").xalign(0.0).css_classes(["heading"]).build());
    col.append(&frame);
    let attach =
        gtk::CheckButton::builder().label("Include a screenshot of the window").active(shot.is_some()).sensitive(shot.is_some()).build();
    if let Some(t) = &shot {
        let pic = gtk::Picture::for_paintable(t);
        pic.set_can_shrink(true);
        pic.set_height_request(140);
        pic.set_content_fit(gtk::ContentFit::Contain);
        col.append(&pic);
        col.append(&attach);
        col.append(
            &gtk::Label::builder()
                .label("Check the screenshot doesn't show anything you'd rather keep private.")
                .wrap(true)
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }
    d.set_extra_child(Some(&col));
    d.add_responses(&[("cancel", "Cancel"), ("copy", "Copy report"), ("github", "Open GitHub issue")]);
    d.set_response_appearance("github", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("github"));
    d.connect_response(
        None,
        glib::clone!(
            #[weak]
            app,
            move |_, resp| {
                if resp == "cancel" {
                    return;
                }
                let buf = what.buffer();
                let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string();
                let dir = reports_dir();
                let _ = std::fs::create_dir_all(&dir);
                let stamp = crate::export::rfc3339(crate::db::now()).replace(':', "-");
                let mut shot_path = None;
                if let (Some(t), true) = (&shot, attach.is_active()) {
                    let p = dir.join(format!("yourtube-{stamp}.png"));
                    if t.save_to_png(&p).is_ok() {
                        shot_path = Some(p);
                    }
                }
                let toolkit = format!(
                    "GTK {}.{}.{}, libadwaita {}.{}.{}",
                    gtk::major_version(),
                    gtk::minor_version(),
                    gtk::micro_version(),
                    adw::major_version(),
                    adw::minor_version(),
                    adw::micro_version()
                );
                let (title, body) = report::build(&text, &page, &toolkit, shot_path.as_ref().and_then(|p| p.to_str()));
                let _ = std::fs::write(dir.join(format!("yourtube-{stamp}.md")), format!("# {title}\n\n{body}"));
                if let Some(w) = app.window.get() {
                    w.clipboard().set_text(&format!("{title}\n\n{body}"));
                }
                if resp == "github" {
                    gtk::UriLauncher::new(&report::issue_url(&title, &body)).launch(app.window.get(), gtk::gio::Cancellable::NONE, |_| {});
                    if let Some(p) = &shot_path {
                        // GitHub can't take files through a link, so show the screenshot to drag in.
                        gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(p))).open_containing_folder(
                            app.window.get(),
                            gtk::gio::Cancellable::NONE,
                            |_| {},
                        );
                        app.toast("Issue opened, report copied. Drag the screenshot from the folder into the issue.");
                    } else {
                        app.toast("Issue opened in your browser; the full report is also on your clipboard");
                    }
                } else {
                    app.toast("Report copied to the clipboard");
                }
            }
        ),
    );
    d.present(Some(window));
}
