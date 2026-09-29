//! The versions store against a real filesystem.
//!
//! This is the one test that exercises `FICLONE` itself, so it needs a disk that
//! can share blocks. **On a disk that cannot — ext4, tmpfs, a container's
//! overlay — it declares itself unsupported and passes**, because the suite has
//! to be green on every machine this ships from, and the feature's whole answer
//! there is "this computer keeps no versions".
//!
//! It runs in its own process, which is what lets it point `HOME` and
//! `XDG_DATA_HOME` at a scratch directory: the service resolves both once into
//! statics, so a unit test sharing the process could not.
#![cfg(not(miri))]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "integration test owns its own scratch directory"
)]

use std::path::{Path, PathBuf};

use big_search::history::{History, Reason, Support, Unavailable};

/// A scratch home on the same filesystem as the build directory.
///
/// Not under `/tmp`: that is usually `tmpfs`, which cannot share blocks, and the
/// test would skip itself on the very machine it was meant to cover. And no
/// hidden component anywhere in the path — the scanner excludes those, so the
/// documents would not be eligible.
fn scratch_home() -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("history-store");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("Documentos")).expect("scratch home");
    std::fs::create_dir_all(root.join("data")).expect("scratch data");
    // Canonical, because the service canonicalises the folders it catalogues and
    // the paths it hears about from the kernel are canonical too. A build
    // directory reached through a symlink would not match either.
    std::fs::canonicalize(&root).expect("canonical scratch home")
}

fn write_by_replacing(path: &Path, contents: &str) {
    // The kernel stamps a file with a clock that only moves every few
    // milliseconds, so two writes in a row here would carry the *same*
    // modification time and the second one would look like no change at all.
    // A person saving a document cannot hit that window; a test can.
    std::thread::sleep(std::time::Duration::from_millis(20));
    // What every document editor does: write a temporary file, then rename it
    // over the original. The inode changes; the path does not.
    let temporary = path.with_extension("tmp-save");
    std::fs::write(&temporary, contents).expect("write temporary");
    std::fs::rename(&temporary, path).expect("rename over the original");
}

#[test]
fn a_version_survives_the_save_that_replaced_it() {
    let home = scratch_home();
    // SAFETY: this test binary is its own process and sets these once, before
    // anything reads them.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_DATA_HOME", home.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
    }

    let document = home.join("Documentos/contrato.odt");
    std::fs::write(&document, "versão A").expect("write the document");

    let mut history = History::open(&big_search_config::FileHistory::default());
    // `Saved`, not `Baseline`: a document created while the service is watching
    // earns its first version from the watcher, and that is the path that broke
    // once — the state it started from has to survive thinning either way.
    let kept = history
        .save_version(&document, Reason::Saved)
        .expect("save the first version");

    if let Support::Unavailable(reason) = history.support() {
        assert!(
            // A container's root is an overlay, which is also how a live
            // session looks; the service keeps no versions in either.
            matches!(
                reason,
                Unavailable::NoReflink | Unavailable::OldKernel | Unavailable::LiveSession
            ),
            "unexpected reason: {}",
            reason.as_str()
        );
        eprintln!(
            "this filesystem keeps no versions ({}); test declared unsupported",
            reason.as_str()
        );
        return;
    }
    assert!(kept, "the first version was not kept");

    write_by_replacing(&document, "versão B");
    assert!(
        history
            .save_version(&document, Reason::Saved)
            .expect("save the second version"),
        "the save that replaced the document did not earn a version"
    );

    // One, not two: the state the document is in right now is not an *earlier*
    // version of it, so it is left out of the list.
    let versions = history.versions(&document).expect("read the versions");
    assert_eq!(versions.len(), 1, "{versions:?}");
    let oldest = versions.last().expect("one earlier version");
    assert_eq!(
        std::fs::read_to_string(&oldest.object).expect("read the kept version"),
        "versão A",
        "the kept version followed the document instead of preserving it"
    );
    assert_eq!(
        std::fs::read_to_string(&document).expect("read the document"),
        "versão B"
    );
    // A version nobody can save over.
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&oldest.object)
            .expect("stat")
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o400, "a version must not be writable");

    // Saving nothing new keeps nothing new.
    assert!(
        !history
            .save_version(&document, Reason::Saved)
            .expect("no change"),
        "an unchanged document earned a second version"
    );

    // Renaming carries the history with it, instead of starting again.
    let renamed = home.join("Documentos/contrato assinado.odt");
    std::fs::rename(&document, &renamed).expect("rename");
    // What the watcher does when the kernel hands it both names at once. The
    // save that follows replaces the inode, so this pair is the only thing that
    // still connects the document to its own past.
    history
        .note_rename(&document, &renamed)
        .expect("note the rename");
    write_by_replacing(&renamed, "versão C");
    history
        .save_version(&renamed, Reason::Saved)
        .expect("save after the rename");
    let after_rename = history.versions(&renamed).expect("versions");
    assert!(
        !after_rename.is_empty(),
        "the history did not follow the renamed document: {after_rename:?}"
    );
    // The state it started from is still there, under the new name. Two saves
    // inside the same hour share one slot, so the middle one is gone — that is
    // the thinning working, and the first version surviving it is the promise.
    assert_eq!(after_rename.len(), 1, "{after_rename:?}");
    let first = after_rename.last().expect("the oldest version");
    assert_eq!(
        std::fs::read_to_string(&first.object).expect("read"),
        "versão A"
    );

    // The state a document was in right before somebody went back to an older
    // version survives the next save, minutes later, in the same hour slot.
    // The dialog that asks promises exactly that, and thinning used to break it.
    write_by_replacing(&renamed, "versão D");
    history
        .save_version(&renamed, Reason::Saved)
        .expect("the watcher keeps this state first");
    // The file manager asks for the same state again, right before writing an
    // older version over it. Nothing new to clone — and that row is the undo.
    history
        .save_version(&renamed, Reason::BeforeRestore)
        .expect("keep the state before going back");
    write_by_replacing(&renamed, "versão A");
    history
        .save_version(&renamed, Reason::Saved)
        .expect("save after going back");
    let undo = history
        .versions(&renamed)
        .expect("versions")
        .into_iter()
        .find(|version| version.reason == Reason::BeforeRestore)
        .expect("the undo version was thinned away");
    assert_eq!(
        std::fs::read_to_string(&undo.object).expect("read"),
        "versão D"
    );

    // A file manager deletes for good by renaming to a hidden name and then
    // unlinking. The history must not follow it there: filed under a name
    // nobody can type, the request to forget it would never find it, and the
    // content of a document somebody destroyed would sit here for a month.
    let on_its_way_out = home.join("Documentos/.bigfiles-delete-copy-9f2c.odt");
    history
        .note_rename(&renamed, &on_its_way_out)
        .expect("note the rename");
    assert!(
        !history.versions(&renamed).expect("versions").is_empty(),
        "the history followed the document out of reach"
    );

    // And forgetting takes the objects with it, not just the rows.
    let objects: Vec<PathBuf> = history
        .versions(&renamed)
        .expect("versions")
        .into_iter()
        .map(|version| version.object)
        .collect();
    history.forget(&renamed).expect("forget");
    assert!(history.versions(&renamed).expect("versions").is_empty());
    for object in objects {
        assert!(!object.exists(), "{} was left behind", object.display());
    }
    // And what is not a document never reaches the store at all. In the same
    // test because the service resolves the home folder once per process: two
    // tests with two scratch homes would race for that one answer.
    for name in ["filme.mkv", "senhas.txt", "cofre.kdbx", "vazio.odt"] {
        let path = home.join("Documentos").join(name);
        std::fs::write(&path, if name == "vazio.odt" { "" } else { "conteúdo" }).expect("write");
        assert!(
            !history.save_version(&path, Reason::Saved).expect("save"),
            "{name} should never be kept"
        );
    }
}
