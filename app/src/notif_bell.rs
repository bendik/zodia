//! Sidebar notification bell — visible only when there are unread messages.
//!
//! Pure-display `SimpleComponent`: parent pushes the unread total + a
//! pre-formatted summary of which stargazers have unread messages, and the
//! bell handles its own visibility + popover content.

use libadwaita::gtk;
use libadwaita::gtk::prelude::*;
use relm4::prelude::*;
use relm4::Component;

// ── public entry point ────────────────────────────────────────────────────────

/// Spawn the bell, return its `MenuButton` root + an input sender for updates.
pub fn launch() -> (gtk::MenuButton, relm4::Sender<NotifBellMsg>) {
    use relm4::ComponentController;
    let mut ctl = <NotifBell as Component>::builder()
        .launch(())
        .detach();
    let widget = ctl.widget().clone();
    let sender = ctl.sender().clone();
    ctl.detach_runtime();
    (widget, sender)
}

// ── messages ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum NotifBellMsg {
    /// Set the popover summary text and visibility based on `total_unread`.
    Set { summary: String, total_unread: usize },
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct NotifBell {
    summary:      String,
    total_unread: usize,
}

pub struct NotifBellWidgets {
    btn:   gtk::MenuButton,
    label: gtk::Label,
}

impl SimpleComponent for NotifBell {
    type Init    = ();
    type Input   = NotifBellMsg;
    type Output  = ();
    type Root    = gtk::MenuButton;
    type Widgets = NotifBellWidgets;

    fn init_root() -> Self::Root {
        gtk::MenuButton::new()
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        _sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        relm4::view! {
            label = gtk::Label {
                set_margin_top: 8,
                set_margin_bottom: 8,
                set_margin_start: 12,
                set_margin_end: 12,
            }
        }
        relm4::view! {
            popover = gtk::Popover {
                #[wrap(Some)]
                set_child = &label.clone() {},
            }
        }
        root.set_icon_name("notification-symbolic");
        root.set_tooltip_text(Some("Notifications"));
        root.set_visible(false);
        root.set_popover(Some(&popover));

        ComponentParts {
            model:   NotifBell { summary: String::new(), total_unread: 0 },
            widgets: NotifBellWidgets { btn: root, label },
        }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            NotifBellMsg::Set { summary, total_unread } => {
                self.summary      = summary;
                self.total_unread = total_unread;
            }
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: ComponentSender<Self>) {
        widgets.btn.set_visible(self.total_unread > 0);
        if self.total_unread > 0 {
            widgets.label.set_text(&self.summary);
        }
    }
}
