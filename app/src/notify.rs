//! Cross-platform desktop notifications via `gio::Notification`.
//!
//! GTK routes notifications through the OS-native delivery mechanism:
//!   - Linux  — D-Bus / libnotify (GNOME Shell, KDE, etc.)
//!   - macOS  — NSUserNotificationCenter / UNUserNotificationCenter
//!   - Windows — WinRT Toast (via GLib Windows backend)
//!
//! Interactive notifications (consent request, incoming call) carry action
//! buttons backed by `gio::SimpleAction`s registered on the GIO application.
//! Those actions capture a `relm4::Sender<AppMsg>` and fire the right message.

use gio::prelude::*;
use relm4::AsyncComponentSender;

use crate::app::AppModel;
use crate::app::AppMsg;

/// Register the four interactive application actions used by notification buttons.
///
/// Call once from `AppModel::init()`.  Each action captures a clone of `sender`
/// so it can dispatch the corresponding `AppMsg` when the user clicks a button.
pub fn register_actions(sender: &AsyncComponentSender<AppModel>) {
    let app = relm4::main_application();

    macro_rules! register {
        ($name:expr, $msg:expr) => {{
            let action = gio::SimpleAction::new($name, None);
            let s = sender.clone();
            action.connect_activate(move |_, _| s.input($msg));
            app.add_action(&action);
        }};
    }

    register!("accept-consent", AppMsg::AcceptConsent);
    register!("reject-consent", AppMsg::RejectConsent);
    register!("accept-call",    AppMsg::AcceptCall);
    register!("reject-call",    AppMsg::RejectCall);
}

/// Send a desktop notification.
///
/// `id` is used for deduplication — a second `send` with the same `id`
/// replaces the previous notification rather than stacking a new one.
///
/// `buttons` is a slice of `(label, "app.<action-name>")` pairs.  Pass an
/// empty slice for a plain fire-and-forget notification.
pub fn send(id: &str, title: &str, body: &str, icon_name: &str, buttons: &[(&str, &str)]) {
    let notif = gio::Notification::new(title);
    notif.set_body(Some(body));
    notif.set_icon(&gio::ThemedIcon::new(icon_name));
    for (label, action) in buttons {
        notif.add_button(label, action);
    }
    relm4::main_application().send_notification(Some(id), &notif);
}

/// Withdraw a previously-sent notification by id.
///
/// Safe to call when no notification with that id is currently displayed.
pub fn withdraw(id: &str) {
    relm4::main_application().withdraw_notification(id);
}
