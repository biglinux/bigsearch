// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! Remembering where files came from — and the one switch that is off until
//! somebody asks for it.
//!
//! The file manager can say "you downloaded this from that site in March", which
//! is often the only thing that explains why a file is on the machine at all.
//! Two of the places that answer are the person's own browsers, and reading
//! those is a different kind of permission from cataloguing their files:
//! **nobody expects a file manager to have read the browser's download list**,
//! so that one starts switched off and says exactly what it reads.
//!
//! What the screen promises about the network is not a promise: the service runs
//! under `RestrictAddressFamilies=AF_UNIX`, so the kernel refuses it a network
//! socket at all.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{SearchSettings, msgid, tr};

/// The heading of this group, and what the settings search calls it.
pub(crate) const TITLE: &str = msgid("Where your files came from");

pub(crate) fn group(settings: &Rc<SearchSettings>) -> adw::PreferencesGroup {
    let config = big_search_config::read();
    let group = adw::PreferencesGroup::builder().title(tr(TITLE)).build();

    let remember = adw::SwitchRow::builder()
        .title(tr("Remember where files came from"))
        .subtitle(tr(
            "Lets the file manager say how a file got here. Stays on this computer; nothing is sent anywhere.",
        ))
        .active(config.origin.enabled)
        .build();
    {
        let settings = Rc::clone(settings);
        remember.connect_active_notify(move |row| {
            settings.set_origin("enabled", if row.is_active() { "true" } else { "false" });
        });
    }
    group.add(&remember);

    let browsers = adw::SwitchRow::builder()
        .title(tr("Look at the browser's download list"))
        .subtitle(browser_subtitle())
        .active(config.origin.browser_history)
        .build();
    browsers.set_sensitive(config.origin.enabled);
    {
        let settings = Rc::clone(settings);
        browsers.connect_active_notify(move |row| {
            settings.set_origin(
                "browser_history",
                if row.is_active() { "true" } else { "false" },
            );
        });
    }
    // Without the first switch there is nothing to feed, so the second one
    // cannot be on: a switch that changes nothing is a switch that lies.
    {
        let browsers = browsers.clone();
        remember.connect_active_notify(move |row| browsers.set_sensitive(row.is_active()));
    }
    group.add(&browsers);

    let forget = gtk::Button::builder()
        .label(tr("Forget everything remembered"))
        .halign(gtk::Align::End)
        .margin_top(6)
        .css_classes(["destructive-action"])
        .build();
    {
        let card = settings.status.clone();
        forget.connect_clicked(move |button| confirm_forget(button, &card));
    }
    group.add(&forget);

    group
}

/// What the download-list switch reads, and which browsers were found.
///
/// The list of browsers is said rather than asked about: a person does not know
/// which browser wrote what, and one switch per browser would be seven decisions
/// nobody can make. Naming what was found gives them the fact without the
/// decision.
fn browser_subtitle() -> String {
    let found = installed_browsers();
    let promise = tr(
        "Reads only the list of downloads, to say which site a file came from. It never reads the pages you visited.",
    );
    if found.is_empty() {
        return promise;
    }
    format!(
        "{promise}\n{}",
        tr("Found here: {browsers}").replace("{browsers}", &found.join(", "))
    )
}

/// The Chromium-family browsers whose profile folder exists.
///
/// Only the folder is looked at, never opened: this is for the sentence on the
/// screen, and the reading itself happens in the service and only once the
/// switch is on.
fn installed_browsers() -> Vec<String> {
    const BROWSERS: [(&str, &str); 6] = [
        ("google-chrome", "Google Chrome"),
        ("chromium", "Chromium"),
        ("BraveSoftware/Brave-Browser", "Brave"),
        ("microsoft-edge", "Microsoft Edge"),
        ("opera", "Opera"),
        ("vivaldi", "Vivaldi"),
    ];
    let config_home = big_search_config::home_dir().join(".config");
    BROWSERS
        .iter()
        .filter(|(folder, _)| config_home.join(folder).is_dir())
        .map(|(_, name)| (*name).to_owned())
        .collect()
}

fn confirm_forget(anchor: &gtk::Button, status: &crate::status::StatusCard) {
    let dialog = adw::AlertDialog::new(
        Some(&tr("Forget where every file came from?")),
        Some(&tr(
            "The files themselves are untouched. Only what was remembered about how they got here is erased, and it cannot be brought back.",
        )),
    );
    dialog.add_response("cancel", &tr("Cancel"));
    dialog.add_response("forget", &tr("Forget"));
    dialog.set_response_appearance("forget", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    let status = status.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "forget" {
            return;
        }
        let status = status.clone();
        glib::spawn_future_local(async move {
            let cleared = gio::spawn_blocking(|| {
                big_indexd_client::Client::connect_default()
                    .is_some_and(|client| client.clear_origin().is_ok())
            })
            .await
            .unwrap_or(false);
            if !cleared {
                status.show_forget_failed();
            }
        });
    });
    dialog.present(Some(anchor));
}
