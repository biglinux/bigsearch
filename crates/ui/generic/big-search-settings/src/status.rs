// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! How the search is doing, in the words of somebody who is not going to read
//! the word "index".
//!
//! Everything here comes from asking the service, every couple of seconds, and
//! nothing is assumed: a service that is not answering is reported as switched
//! off with a button to switch it on, never as "0 files", which reads as an
//! empty disk.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{POLL_EVERY, tr};

/// The card at the top of the window.
#[derive(Clone)]
pub(crate) struct StatusCard {
    group: adw::PreferencesGroup,
    headline: gtk::Label,
    detail: gtk::Label,
    pause: gtk::Button,
    rebuild: gtk::Button,
    turn_on: gtk::Button,
    /// True while a restart is in flight, so an answer that arrives from the
    /// service that is going away does not overwrite "Applying…".
    applying: Rc<Cell<bool>>,
    /// Whether the service said it is paused, which is what the button means.
    paused: Rc<Cell<bool>>,
    poll: Rc<RefCell<Option<glib::SourceId>>>,
}

impl StatusCard {
    pub(crate) fn new() -> Self {
        let headline = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["title-4"])
            .label(tr("Checking…"))
            .build();
        let detail = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        let pause = gtk::Button::with_label(&tr("Pause"));
        let rebuild = gtk::Button::with_label(&tr("Build the list again"));
        let turn_on = gtk::Button::with_label(&tr("Turn the search on"));
        turn_on.add_css_class("suggested-action");
        turn_on.set_visible(false);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&turn_on);
        buttons.append(&rebuild);
        buttons.append(&pause);

        let words = gtk::Box::new(gtk::Orientation::Vertical, 4);
        words.append(&headline);
        words.append(&detail);
        words.append(&buttons);
        words.set_margin_top(6);
        words.set_margin_bottom(6);

        // No title: the window is already called "System search", and a heading
        // repeating the title above it is furniture.
        let group = adw::PreferencesGroup::new();
        group.add(&words);

        let card = Self {
            group,
            headline,
            detail,
            pause,
            rebuild,
            turn_on,
            applying: Rc::new(Cell::new(false)),
            paused: Rc::new(Cell::new(false)),
            poll: Rc::new(RefCell::new(None)),
        };
        card.connect_buttons();
        card
    }

    pub(crate) fn group(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    fn connect_buttons(&self) {
        {
            let paused = Rc::clone(&self.paused);
            let card = self.clone();
            self.pause.connect_clicked(move |_| {
                let resume = paused.get();
                let card = card.clone();
                ask(
                    move |client| {
                        if resume {
                            client.resume()
                        } else {
                            client.pause()
                        }
                    },
                    move |_| card.refresh(),
                );
            });
        }
        {
            let card = self.clone();
            self.rebuild.connect_clicked(move |button| {
                card.confirm_rebuild(button);
            });
        }
        {
            let card = self.clone();
            self.turn_on.connect_clicked(move |_| {
                if let Err(error) = crate::systemctl(&["enable", "--now", "big-search.service"]) {
                    log::warn!("the search service could not be started: {error}");
                }
                card.headline.set_label(&tr("Starting…"));
            });
        }
    }

    /// Building the list again is the one action here that costs real time, so
    /// it is the one that asks first.
    fn confirm_rebuild(&self, anchor: &gtk::Button) {
        let dialog = adw::AlertDialog::new(
            Some(&tr("Build the list of files again?")),
            Some(&tr(
                "The computer will read every catalogued folder from the start. Searching keeps working while it does, but results may be missing until it finishes.",
            )),
        );
        dialog.add_response("cancel", &tr("Cancel"));
        dialog.add_response("rebuild", &tr("Build again"));
        dialog.set_response_appearance("rebuild", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));
        let card = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "rebuild" {
                return;
            }
            let card = card.clone();
            ask(|client| client.rebuild(), move |_| card.refresh());
        });
        dialog.present(Some(anchor));
    }

    /// A change is written and the service has not been restarted yet.
    pub(crate) fn show_pending_restart(&self) {
        self.detail.set_label(&tr(
            "Your change is saved. It starts working when you close this window.",
        ));
    }

    pub(crate) fn show_applying(&self) {
        self.applying.set(true);
        self.headline.set_label(&tr("Applying…"));
        self.detail.set_label("");
    }

    /// The service refused, or is not there to be asked.
    pub(crate) fn show_forget_failed(&self) {
        self.detail.set_label(&tr(
            "Nothing could be forgotten: the search is not running. Turn it on and try again.",
        ));
    }

    pub(crate) fn show_write_failed(&self) {
        self.detail.set_label(&tr(
            "Your change could not be saved. Check the space left on the disk.",
        ));
    }

    /// Ask the service how it is doing, now and every couple of seconds.
    pub(crate) fn start_polling(&self) {
        // Never two timers: the page starts this every time it comes back on
        // screen, and a second one would ask twice as often for ever.
        self.stop_polling();
        self.refresh();
        let card = self.clone();
        let source = glib::timeout_add_local(POLL_EVERY, move || {
            card.refresh();
            glib::ControlFlow::Continue
        });
        *self.poll.borrow_mut() = Some(source);
    }

    /// Stop asking. Called when the window closes: a timer left running keeps
    /// waking the process up to ask about a window nobody is looking at.
    pub(crate) fn stop_polling(&self) {
        if let Some(source) = self.poll.borrow_mut().take() {
            source.remove();
        }
    }

    fn refresh(&self) {
        let card = self.clone();
        ask(
            |client| client.status(),
            move |answer| match answer {
                Ok(status) => card.show(&status),
                Err(_) => card.show_off(),
            },
        );
    }

    fn show(&self, status: &big_indexd_client::Status) {
        self.applying.set(false);
        self.paused.set(status.paused);
        self.turn_on.set_visible(false);
        self.pause.set_visible(true);
        self.rebuild.set_visible(true);
        self.pause.set_label(&if status.paused {
            tr("Resume")
        } else {
            tr("Pause")
        });

        self.headline.set_label(&if status.paused {
            tr("Paused")
        } else {
            tr("Working")
        });

        let mut lines = vec![
            tr("{count} files the computer can find").replace("{count}", &grouped(status.indexed)),
        ];
        if status.pending_content > 0 {
            lines.push(
                tr("Still reading what is inside {count} of them")
                    .replace("{count}", &grouped(status.pending_content)),
            );
        }
        let bytes = status.name_index_bytes + status.content_index_bytes;
        if bytes > 0 {
            lines.push(tr("Takes up {size} on the disk").replace("{size}", &human_size(bytes)));
        }
        self.detail.set_label(&lines.join("\n"));
    }

    /// Nobody answered. **Not** the same as an empty index: saying "0 files"
    /// here would tell the person their disk is empty.
    fn show_off(&self) {
        if self.applying.get() {
            return;
        }
        self.headline.set_label(&tr("The search is switched off"));
        self.detail.set_label(&tr(
            "Nothing is being catalogued, so searching reads the folders one by one and takes longer.",
        ));
        self.turn_on.set_visible(true);
        self.pause.set_visible(false);
        self.rebuild.set_visible(false);
    }
}

/// Ask the service something on a worker thread and answer on the main loop.
///
/// The socket blocks; a round trip on the main loop is a frozen window for as
/// long as the service takes.
fn ask<T: Send + 'static>(
    question: impl FnOnce(&big_indexd_client::Client) -> Result<T, big_indexd_client::ClientError>
    + Send
    + 'static,
    answer: impl FnOnce(Result<T, big_indexd_client::ClientError>) + 'static,
) {
    glib::spawn_future_local(async move {
        let outcome = gio::spawn_blocking(move || {
            let Some(client) = big_indexd_client::Client::connect_default() else {
                return Err(big_indexd_client::ClientError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no search service",
                )));
            };
            question(&client)
        })
        .await;
        match outcome {
            Ok(outcome) => answer(outcome),
            Err(_) => answer(Err(big_indexd_client::ClientError::Io(
                std::io::Error::other("the worker stopped"),
            ))),
        }
    });
}

/// A count with its thousands separated, because six digits in a row is not a
/// number anybody reads.
fn grouped(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push('.');
        }
        out.push(digit);
    }
    out
}

/// A size in the units people use for disks.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit + 1 < UNITS.len() {
        size /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_count_is_grouped_the_way_it_is_read() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1.000");
        assert_eq!(grouped(722_431), "722.431");
        assert_eq!(grouped(1_234_567), "1.234.567");
    }

    #[test]
    fn a_size_is_written_in_disk_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1_500), "1.5 KB");
        assert_eq!(human_size(1_200_000_000), "1.2 GB");
    }
}
