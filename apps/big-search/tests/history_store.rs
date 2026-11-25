//! The versions store against a real filesystem.
//!
//! This is the one test that exercises `FICLONE` itself, so it needs a disk that
//! can share blocks. On a disk that cannot (ext4, tmpfs, a container's
//! overlay) it declares itself unsupported and passes, because the suite has
//! to be green on every machine this ships from, and the feature's whole answer
//! there is "this computer keeps no versions".
//!
//! Its own process, so it can point `HOME` and `XDG_DATA_HOME` at a scratch
//! directory: the service resolves both once into statics.
#![cfg(not(miri))]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "integration test owns its own scratch directory"
)]

use std::path::{Path, PathBuf};

use big_search::history::{History, Reason, SharedHistory, Support, Unavailable, check_kept_paths};

/// A scratch home on the same filesystem as the build directory.
///
/// Not under `/tmp`: that is usually `tmpfs`, which cannot share blocks, and the
/// test would skip itself on the very machine it was meant to cover. And no
/// hidden component anywhere in the path: the scanner excludes those, so the
/// documents would not be eligible.
fn scratch_home() -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("history-store");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("Documentos")).expect("scratch home");
    std::fs::create_dir_all(root.join("data")).expect("scratch data");
    // Canonical, as the service's own paths are.
    std::fs::canonicalize(&root).expect("canonical scratch home")
}

fn write_by_replacing(path: &Path, contents: &str) {
    // The mtime clock is coarse; back-to-back writes would look unchanged.
    std::thread::sleep(std::time::Duration::from_millis(20));
    // What document editors do: write a temporary file, rename it over the
    // original. The inode changes; the path does not.
    let temporary = path.with_extension("tmp-save");
    std::fs::write(&temporary, contents).expect("write temporary");
    std::fs::rename(&temporary, path).expect("rename over the original");
}

#[test]
fn versions_survive_replacing_saves() {
    let home = scratch_home();
    let Some((mut history, document)) = open_with_a_first_version(&home) else {
        return;
    };
    replacing_save_keeps_the_earlier_state(&history, &document);
    let renamed = rename_carries_the_history(&history, &home, &document);
    state_before_a_restore_survives(&history, &renamed);
    hidden_rename_and_forget(&history, &home, &renamed);
    folder_rename_carries_the_history(&history, &home);
    forget_after_a_move_while_stopped(&history, &home);
    folder_moved_onto_a_vanished_one(&history, &home);
    folder_moved_while_stopped(&mut history, &home);
    rename_chain_while_stopped(&mut history, &home);
    created_where_a_document_left(&mut history, &home);
    parked_versions_forgotten(&mut history, &home);
    linked_folder_never_kept(&history, &home);
    non_documents_never_kept(&history, &home);
}

/// The service started again: its store opened anew and the paths it keeps
/// checked before anything it hears.
fn restarted() -> SharedHistory {
    let history = SharedHistory::new(History::open(&big_search_config::FileHistory::default()));
    check_kept_paths(&history).expect("check the kept paths");
    history
}

/// What the versions worker does with a save the watcher heard: the row at
/// the path settled at once, the version kept once the file is quiet.
fn heard_save(history: &SharedHistory, path: &Path) -> bool {
    let mut store = history.lock();
    store.note_change(path).expect("note the change");
    store.save_version(path, Reason::Saved).expect("save")
}

/// Open the store in the scratch home and keep a first version of a document.
/// `None` when this filesystem keeps no versions at all.
fn open_with_a_first_version(home: &Path) -> Option<(SharedHistory, PathBuf)> {
    // SAFETY: this test binary is its own process and sets these once, before
    // anything reads them.
    unsafe {
        std::env::set_var("HOME", home);
        std::env::set_var("XDG_DATA_HOME", home.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
    }

    let document = home.join("Documentos/contrato.odt");
    std::fs::write(&document, "versão A").expect("write the document");

    let history = SharedHistory::new(History::open(&big_search_config::FileHistory::default()));
    // `Saved`, not `Baseline`: a document created while the service is watching
    // earns its first version from the watcher, and the state it started from
    // has to survive thinning either way.
    let kept = heard_save(&history, &document);

    let support = history.lock().support();
    if let Support::Unavailable(reason) = support {
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
        return None;
    }
    assert!(kept, "the first version was not kept");
    Some((history, document))
}

fn replacing_save_keeps_the_earlier_state(history: &SharedHistory, document: &Path) {
    write_by_replacing(document, "versão B");
    assert!(
        heard_save(history, document),
        "the save that replaced the document did not earn a version"
    );

    // One, not two: the state the document is in right now is not an *earlier*
    // version of it, so it is left out of the list.
    let versions = history
        .lock()
        .versions(&[document])
        .expect("read the versions");
    assert_eq!(versions.len(), 1, "{versions:?}");
    let oldest = versions.last().expect("one earlier version");
    assert_eq!(
        std::fs::read_to_string(&oldest.object).expect("read the kept version"),
        "versão A",
        "the kept version followed the document instead of preserving it"
    );
    assert_eq!(
        std::fs::read_to_string(document).expect("read the document"),
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
        !heard_save(history, document),
        "an unchanged document earned a second version"
    );
}

fn rename_carries_the_history(history: &SharedHistory, home: &Path, document: &Path) -> PathBuf {
    // Renaming carries the history with it, instead of starting again.
    let renamed = home.join("Documentos/contrato assinado.odt");
    std::fs::rename(document, &renamed).expect("rename");
    // What the watcher does with a rename pair; the next save replaces the
    // inode, so this is the only link to the document's past.
    history
        .lock()
        .note_rename(document, &renamed)
        .expect("note the rename");
    write_by_replacing(&renamed, "versão C");
    heard_save(history, &renamed);
    let after_rename = history.lock().versions(&[&renamed]).expect("versions");
    assert!(
        !after_rename.is_empty(),
        "the history did not follow the renamed document: {after_rename:?}"
    );
    // The state it started from is still there, under the new name. Two saves
    // inside the same hour share one slot, so thinning drops the middle one and
    // keeps the first.
    assert_eq!(after_rename.len(), 1, "{after_rename:?}");
    let first = after_rename.last().expect("the oldest version");
    assert_eq!(
        std::fs::read_to_string(&first.object).expect("read"),
        "versão A"
    );
    renamed
}

fn state_before_a_restore_survives(history: &SharedHistory, renamed: &Path) {
    // The state a document was in right before somebody went back to an older
    // version survives the next save, minutes later, in the same hour slot.
    // The dialog that asks promises exactly that.
    write_by_replacing(renamed, "versão D");
    heard_save(history, renamed);
    // The file manager asks for the same state again, right before writing an
    // older version over it. Nothing new to clone, and that row is the undo.
    history
        .lock()
        .save_version(renamed, Reason::BeforeRestore)
        .expect("keep the state before going back");
    write_by_replacing(renamed, "versão A");
    heard_save(history, renamed);
    let undo = history
        .lock()
        .versions(&[renamed])
        .expect("versions")
        .into_iter()
        .find(|version| version.reason == Reason::BeforeRestore)
        .expect("the undo version was thinned away");
    assert_eq!(
        std::fs::read_to_string(&undo.object).expect("read"),
        "versão D"
    );
}

fn hidden_rename_and_forget(history: &SharedHistory, home: &Path, renamed: &Path) {
    // A file manager deletes by renaming to a hidden name, then unlinking. The
    // history must not follow, or a later "forget" could never find it.
    let on_its_way_out = home.join("Documentos/.bigfiles-delete-copy-9f2c.odt");
    history
        .lock()
        .note_rename(renamed, &on_its_way_out)
        .expect("note the rename");
    assert!(
        !history
            .lock()
            .versions(&[renamed])
            .expect("versions")
            .is_empty(),
        "the history followed the document out of reach"
    );

    // And forgetting takes the objects with it, not just the rows.
    let objects: Vec<PathBuf> = history
        .lock()
        .versions(&[renamed])
        .expect("versions")
        .into_iter()
        .map(|version| version.object)
        .collect();
    history.lock().forget(&[renamed], None).expect("forget");
    assert!(
        history
            .lock()
            .versions(&[renamed])
            .expect("versions")
            .is_empty()
    );
    for object in objects {
        assert!(!object.exists(), "{} was left behind", object.display());
    }
}

/// A document with one earlier version listed, written at `path`.
fn kept_twice(history: &SharedHistory, path: &Path) {
    std::fs::create_dir_all(path.parent().expect("a folder")).expect("create the folder");
    std::fs::write(path, "primeira").expect("write");
    heard_save(history, path);
    write_by_replacing(path, "segunda");
    heard_save(history, path);
    assert_eq!(history.lock().versions(&[path]).expect("versions").len(), 1);
}

/// Guards: privacy action: a folder renamed while the service runs takes the
/// history of every document in it, and of no folder that merely shares its
/// name's start, so the request to forget it through its new path finds it.
fn folder_rename_carries_the_history(history: &SharedHistory, home: &Path) {
    let docs = home.join("Documentos");
    let inside = docs.join("proj/a.odt");
    let beside = [docs.join("proj-a/b.odt"), docs.join("proj2/b.odt")];
    for document in std::iter::once(&inside).chain(&beside) {
        kept_twice(history, document);
    }
    std::fs::create_dir(docs.join("arquivo")).expect("create the archive");
    std::fs::rename(docs.join("proj"), docs.join("arquivo/proj")).expect("move the folder");
    history
        .lock()
        .note_rename(&docs.join("proj"), &docs.join("arquivo/proj"))
        .expect("note the rename");
    std::os::unix::fs::symlink(docs.join("arquivo/proj"), docs.join("proj")).expect("link");

    let moved = docs.join("arquivo/proj/a.odt");
    assert_eq!(
        history.lock().versions(&[&moved]).expect("versions").len(),
        1
    );
    assert!(
        history
            .lock()
            .versions(&[&inside])
            .expect("versions")
            .is_empty()
    );
    for document in &beside {
        assert_eq!(
            history
                .lock()
                .versions(&[document])
                .expect("versions")
                .len(),
            1
        );
    }
    history
        .lock()
        .forget(&[&moved], Some(&moved))
        .expect("forget");
    assert!(
        history
            .lock()
            .versions(&[&moved])
            .expect("versions")
            .is_empty()
    );
}

/// Guards: privacy action: forgetting a document through its path erases the
/// versions kept under the path it had before its folder moved while the
/// service was stopped.
fn forget_after_a_move_while_stopped(history: &SharedHistory, home: &Path) {
    let docs = home.join("Documentos");
    let before = docs.join("pasta/c.odt");
    kept_twice(history, &before);
    std::fs::rename(docs.join("pasta"), docs.join("arquivo/pasta")).expect("move the folder");
    let now_at = docs.join("arquivo/pasta/c.odt");
    history
        .lock()
        .forget(&[&now_at], Some(&now_at))
        .expect("forget");
    assert!(
        history
            .lock()
            .versions(&[&before])
            .expect("versions")
            .is_empty()
    );
}

/// Guards: persisted data: `rm -r velha; mv nova velha` offers the deleted
/// documents' versions neither as the moved one's nor as a new one's, and
/// forgetting their paths erases them.
fn folder_moved_onto_a_vanished_one(history: &SharedHistory, home: &Path) {
    let docs = home.join("Documentos");
    let vanished = docs.join("velha/e.odt");
    let unmatched = docs.join("velha/f.odt");
    let mut objects = Vec::new();
    for document in [&vanished, &unmatched] {
        kept_twice(history, document);
        objects.extend(listed_objects(history, document));
    }
    std::fs::remove_dir_all(docs.join("velha")).expect("delete the folder");
    kept_twice(history, &docs.join("nova/e.odt"));
    std::fs::rename(docs.join("nova"), docs.join("velha")).expect("move the folder");
    history
        .lock()
        .note_rename(&docs.join("nova"), &docs.join("velha"))
        .expect("note the rename");
    assert_eq!(
        history
            .lock()
            .versions(&[&vanished])
            .expect("versions")
            .len(),
        1
    );
    std::fs::write(&unmatched, "nova").expect("write");
    heard_save(history, &unmatched);
    assert!(
        history
            .lock()
            .versions(&[&unmatched])
            .expect("versions")
            .is_empty()
    );
    for document in [&vanished, &unmatched] {
        history
            .lock()
            .forget(&[document], Some(document))
            .expect("forget");
    }
    for object in objects {
        assert!(!object.exists(), "{} was left behind", object.display());
    }
}

/// Guards: privacy action: `mv b c; mkdir b` while the service was stopped,
/// then `mv a b` heard, offers the versions of the document now in `c` for
/// no file arriving in `b`, and forgetting the document in `c` erases them.
fn folder_moved_while_stopped(history: &mut SharedHistory, home: &Path) {
    let docs = home.join("Documentos");
    let document = docs.join("destino/x.odt");
    kept_twice(history, &document);
    let kept = listed_objects(history, &document);
    std::fs::rename(docs.join("destino"), docs.join("guardada")).expect("move the folder");
    std::fs::create_dir(docs.join("destino")).expect("create its namesake");
    *history = restarted();
    std::fs::create_dir(docs.join("origem")).expect("create a folder");
    std::fs::write(docs.join("origem/x.odt"), "outra").expect("write");
    std::fs::rename(docs.join("origem"), docs.join("destino")).expect("move onto the namesake");
    history
        .lock()
        .note_rename(&docs.join("origem"), &docs.join("destino"))
        .expect("note the rename");
    heard_save(history, &document);
    assert!(listed_objects(history, &document).is_empty());
    let moved = docs.join("guardada/x.odt");
    history
        .lock()
        .forget(&[&moved], Some(&moved))
        .expect("forget");
    for object in kept {
        assert!(!object.exists(), "{} was left behind", object.display());
    }
}

/// The objects of the versions listed for `path`.
fn listed_objects(history: &SharedHistory, path: &Path) -> Vec<PathBuf> {
    history
        .lock()
        .versions(&[path])
        .expect("versions")
        .into_iter()
        .map(|version| version.object)
        .collect()
}

/// The pass after a start: each document whose state no version holds is
/// saved, as the watcher never heard it change.
fn caught_up(history: &SharedHistory, documents: &[&Path]) {
    for document in documents {
        history
            .lock()
            .save_version(document, Reason::Saved)
            .expect("save");
    }
}

/// Guards: privacy action: `mv r s; mv t r` while the service was stopped
/// lists neither document's versions for the other, however a heard save
/// and the pass after the start reach them.
fn rename_chain_while_stopped(history: &mut SharedHistory, home: &Path) {
    let docs = home.join("Documentos/cadeia");
    let (r, s, t) = (docs.join("r.odt"), docs.join("s.odt"), docs.join("t.odt"));
    kept_twice(history, &r);
    kept_twice(history, &t);
    let (of_r, of_t) = (listed_objects(history, &r), listed_objects(history, &t));
    std::fs::rename(&r, &s).expect("move r");
    std::fs::rename(&t, &r).expect("move t");
    *history = restarted();
    write_by_replacing(&r, "editada");
    heard_save(history, &r);
    caught_up(history, &[&s]);
    for document in [&r, &s] {
        let listed = listed_objects(history, document);
        assert!(
            listed
                .iter()
                .all(|object| !of_r.contains(object) && !of_t.contains(object)),
            "{} lists {listed:?}",
            document.display()
        );
    }
}

/// Guards: privacy action: a document created where one moved away from
/// while the service was stopped starts its own history.
fn created_where_a_document_left(history: &mut SharedHistory, home: &Path) {
    let docs = home.join("Documentos/deixada");
    let (left, now_at) = (docs.join("d.odt"), docs.join("e.odt"));
    kept_twice(history, &left);
    std::fs::rename(&left, &now_at).expect("move d");
    *history = restarted();
    std::fs::write(&left, "nova").expect("create");
    heard_save(history, &left);
    write_by_replacing(&left, "nova, editada");
    heard_save(history, &left);
    let listed = listed_objects(history, &left);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(std::fs::read_to_string(&listed[0]).expect("read"), "nova");
}

/// Guards: privacy action: `mv r s; mv t r` and a save of `s` while the
/// service was stopped leave `r`'s versions parked, listed for neither;
/// forgetting `r` erases them.
fn parked_versions_forgotten(history: &mut SharedHistory, home: &Path) {
    let docs = home.join("Documentos/estacionada");
    let (r, s, t) = (docs.join("r.odt"), docs.join("s.odt"), docs.join("t.odt"));
    kept_twice(history, &r);
    kept_twice(history, &t);
    let of_r = listed_objects(history, &r);
    std::fs::rename(&r, &s).expect("move r");
    std::fs::rename(&t, &r).expect("move t");
    write_by_replacing(&s, "editada");
    *history = restarted();
    caught_up(history, &[&r, &s]);
    assert!(listed_objects(history, &s).is_empty());
    history.lock().forget(&[&r], Some(&r)).expect("forget");
    for object in of_r {
        assert!(!object.exists(), "{} was left behind", object.display());
    }
}

/// Guards: persisted data: a save heard before a folder on the way was
/// replaced by a link keeps nothing of the file the link leads to under the
/// path, which would give the path's history that file's identity.
fn linked_folder_never_kept(history: &SharedHistory, home: &Path) {
    let docs = home.join("Documentos");
    let other = docs.join("outra/d.odt");
    std::fs::create_dir_all(other.parent().expect("a folder")).expect("create the folder");
    std::fs::write(&other, "outra").expect("write");
    std::os::unix::fs::symlink(docs.join("outra"), docs.join("ligada")).expect("link");
    let through_link = docs.join("ligada/d.odt");
    assert!(!heard_save(history, &through_link));
    assert!(
        history
            .lock()
            .versions(&[&through_link])
            .expect("versions")
            .is_empty()
    );
}

fn non_documents_never_kept(history: &SharedHistory, home: &Path) {
    // Non-documents never reach the store. Same test because the home folder
    // is resolved once per process.
    for name in ["filme.mkv", "senhas.txt", "cofre.kdbx", "vazio.odt"] {
        let path = home.join("Documentos").join(name);
        std::fs::write(&path, if name == "vazio.odt" { "" } else { "conteúdo" }).expect("write");
        assert!(!heard_save(history, &path), "{name} should never be kept");
    }
}
