// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! Keeping earlier versions of documents — the switch, and the honest sentence
//! when this computer cannot.
//!
//! The service finds out for itself whether the disk can share blocks, and says
//! why when it cannot: ext4 and FAT cannot, a network folder cannot, a live
//! session keeps nothing anyway. Where that is the answer, **the switch is not
//! shown at all** — a control that changes nothing is a control that lies — and
//! the reason takes its place.

use std::rc::Rc;

use adw::prelude::*;
use big_indexd_client::Unavailable;
use gtk::{gio, glib};

use crate::{SearchSettings, msgid, tr};

/// The heading of this group, and what the settings search calls it.
pub(crate) const TITLE: &str = msgid("Earlier versions of your documents");

pub(crate) fn group(settings: &Rc<SearchSettings>) -> adw::PreferencesGroup {
    let config = big_search_config::read();
    let group = adw::PreferencesGroup::builder().title(tr(TITLE)).build();

    let keep = adw::SwitchRow::builder()
        .title(tr("Keep earlier versions"))
        .subtitle(
            tr("Keeps the last {count} versions of your documents for {days} days, so you can go back.")
                .replace("{count}", &config.history.keep_versions.to_string())
                .replace("{days}", &config.history.keep_days.to_string()),
        )
        .active(config.history.enabled)
        .build();
    {
        let settings = Rc::clone(settings);
        keep.connect_active_notify(move |row| {
            settings.set_history("enabled", if row.is_active() { "true" } else { "false" });
        });
    }
    group.add(&keep);

    let skip_git = adw::SwitchRow::builder()
        .title(tr("Skip folders managed by git"))
        .subtitle(tr(
            "Git already keeps the history of these folders, and switching branches would rewrite many documents at once.",
        ))
        .active(config.history.skip_git_repositories)
        .build();
    let gitignore = adw::SwitchRow::builder()
        .title(tr("Follow the folder's .gitignore"))
        .subtitle(tr(
            "In git folders, skip the files git itself ignores, such as build output.",
        ))
        .active(config.history.respect_gitignore)
        .build();
    // The `.gitignore` only decides anything when git folders are kept at all.
    keep.bind_property("active", &skip_git, "sensitive")
        .sync_create()
        .build();
    let sync_gitignore = {
        let (keep, skip_git, gitignore) = (
            keep.downgrade(),
            skip_git.downgrade(),
            gitignore.downgrade(),
        );
        move || {
            if let (Some(keep), Some(skip_git), Some(gitignore)) =
                (keep.upgrade(), skip_git.upgrade(), gitignore.upgrade())
            {
                gitignore.set_sensitive(keep.is_active() && !skip_git.is_active());
            }
        }
    };
    sync_gitignore();
    {
        let sync_gitignore = sync_gitignore.clone();
        keep.connect_active_notify(move |_| sync_gitignore());
    }
    {
        let settings = Rc::clone(settings);
        skip_git.connect_active_notify(move |row| {
            settings.set_history(
                "skip_git_repositories",
                if row.is_active() { "true" } else { "false" },
            );
            sync_gitignore();
        });
    }
    {
        let settings = Rc::clone(settings);
        gitignore.connect_active_notify(move |row| {
            settings.set_history(
                "respect_gitignore",
                if row.is_active() { "true" } else { "false" },
            );
        });
    }
    group.add(&skip_git);
    group.add(&gitignore);

    let forget = gtk::Button::builder()
        .label(tr("Erase the kept versions"))
        .halign(gtk::Align::End)
        .margin_top(6)
        .css_classes(["destructive-action"])
        .build();
    {
        let card = settings.status.clone();
        forget.connect_clicked(move |button| confirm_erase(button, &card));
    }
    group.add(&forget);

    // What the service says about itself replaces the switch when it cannot
    // work here. Asked after the rows are built, because the answer comes from
    // another process and must not hold the window open.
    ask_whether_this_computer_can(
        &group,
        [
            keep.upcast(),
            skip_git.upcast(),
            gitignore.upcast(),
            forget.upcast(),
        ],
    );
    group
}

/// Ask the service whether versions are kept here, and say so.
///
/// The question is asked about a file that certainly has no versions, because
/// the answer carries the reason whatever the file is.
fn ask_whether_this_computer_can(group: &adw::PreferencesGroup, controls: [gtk::Widget; 4]) {
    let group = group.clone();
    glib::spawn_future_local(async move {
        let answer = gio::spawn_blocking(|| {
            big_indexd_client::Client::connect_default()
                .and_then(|client| client.versions("/").ok())
        })
        .await
        .ok()
        .flatten();
        let Some(answer) = answer else {
            // The service is not running, which the card above already says. The
            // switch stays: turning it on is what the person came here to do.
            return;
        };
        // Exhaustive on purpose: a reason added to the service has to be given a
        // sentence here, not swallowed by a catch-all that says nothing.
        let explanation = match answer.unavailable {
            None | Some(Unavailable::Disabled) => None,
            Some(Unavailable::NoReflink) => Some(tr(
                "This computer's disk cannot keep earlier versions. Nothing is being kept.",
            )),
            Some(Unavailable::OldKernel) => Some(tr(
                "This system is too old to keep earlier versions. Nothing is being kept.",
            )),
            Some(Unavailable::LiveSession) => Some(tr(
                "This is a trial session, so nothing is kept after you turn the computer off.",
            )),
            Some(Unavailable::Unknown) => {
                Some(tr("Earlier versions are not kept on this computer."))
            }
        };
        if let Some(explanation) = explanation {
            group.set_description(Some(&explanation));
            for control in &controls {
                control.set_visible(false);
            }
        } else if answer.paused_for_space {
            // A different thing from "off": it is on, and waiting for room.
            group.set_description(Some(&tr(
                "There is little free disk space, so new versions are not being kept for now.",
            )));
        }
    });
}

fn confirm_erase(anchor: &gtk::Button, status: &crate::status::StatusCard) {
    let dialog = adw::AlertDialog::new(
        Some(&tr("Erase every kept version?")),
        Some(&tr(
            "Your documents are untouched. Only the earlier versions of them are erased, and they cannot be brought back.",
        )),
    );
    dialog.add_response("cancel", &tr("Cancel"));
    dialog.add_response("erase", &tr("Erase"));
    dialog.set_response_appearance("erase", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    let status = status.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "erase" {
            return;
        }
        let status = status.clone();
        glib::spawn_future_local(async move {
            let erased = gio::spawn_blocking(|| {
                big_indexd_client::Client::connect_default()
                    .is_some_and(|client| client.clear_history().is_ok())
            })
            .await
            .unwrap_or(false);
            if !erased {
                status.show_forget_failed();
            }
        });
    });
    dialog.present(Some(anchor));
}
