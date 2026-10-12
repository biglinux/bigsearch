use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write;
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const FIEMAP_EXTENTS_PER_CALL: usize = 64;
const FIEMAP_MAX_RECORDED_EXTENTS: usize = 1024;
const FIEMAP_EXTENT_LAST: u32 = 0x0000_0001;
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0000_0002;
const FIEMAP_EXTENT_DELALLOC: u32 = 0x0000_0004;
const FIEMAP_EXTENT_ENCODED: u32 = 0x0000_0008;
const FIEMAP_EXTENT_ENCRYPTED_BYTES: u32 = 0x0000_0080;
const FIEMAP_EXTENT_INLINE_BYTES: u32 = 0x0000_0200;
const FIEMAP_EXTENT_TAIL_BYTES: u32 = 0x0000_0400;
const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x0000_0800;
const FIEMAP_EXTENT_MERGED: u32 = 0x0000_1000;
const FIEMAP_EXTENT_SHARED: u32 = 0x0000_2000;
const FIEMAP_UNSAFE_FLAGS: u32 = FIEMAP_EXTENT_UNKNOWN
    | FIEMAP_EXTENT_DELALLOC
    | FIEMAP_EXTENT_ENCODED
    | FIEMAP_EXTENT_ENCRYPTED_BYTES
    | FIEMAP_EXTENT_INLINE_BYTES
    | FIEMAP_EXTENT_TAIL_BYTES
    | FIEMAP_EXTENT_UNWRITTEN
    | FIEMAP_EXTENT_MERGED;
const FS_IOC_FIEMAP: libc::c_ulong = ioctl_read_write(b'f', 11, size_of::<FiemapHeader>());

#[derive(Default, Serialize, Deserialize)]
struct ContentSignatureMap {
    #[serde(default)]
    physical_extents: HashMap<String, ContentRepresentative>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ContentRepresentative {
    path: String,
    /// Files skipped as copies of it. When it is edited they hold the only
    /// copy of the text it had, and must be read again.
    #[serde(default)]
    duplicates: Vec<String>,
}

/// Most duplicates remembered per representative; past it, an edit to the
/// representative leaves the rest unread until they change.
const MAX_DUPLICATES: usize = 64;

pub(crate) struct ContentSignatures {
    file: PathBuf,
    map: ContentSignatureMap,
    /// Representative path → the identity it claimed; built at load, not stored.
    claimed_by: HashMap<String, String>,
    /// Duplicates whose representative claimed new text since; see
    /// [`Self::take_released`].
    released: Vec<PathBuf>,
    dirty: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ContentClaim {
    Unique,
    Duplicate,
}

impl ContentSignatures {
    pub(crate) fn load(file: PathBuf) -> Self {
        let map: ContentSignatureMap = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let claimed_by = map
            .physical_extents
            .iter()
            .map(|(identity, representative)| (representative.path.clone(), identity.clone()))
            .collect();
        Self {
            file,
            map,
            claimed_by,
            released: Vec::new(),
            dirty: false,
        }
    }

    /// Files skipped as duplicates of a representative that has since claimed
    /// different text. Their content state must be forgotten so the next pass
    /// reads them.
    pub(crate) fn take_released(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.released)
    }

    #[cfg(test)]
    fn empty(file: PathBuf) -> Self {
        Self {
            file,
            map: ContentSignatureMap::default(),
            claimed_by: HashMap::new(),
            released: Vec::new(),
            dirty: true,
        }
    }

    pub(crate) fn claim(&mut self, path: &Path, size: u64) -> ContentClaim {
        self.claim_identity(
            path,
            physical_content_identity(path, size),
            |representative| {
                let size = std::fs::metadata(representative).ok()?.len();
                physical_content_identity(representative, size)
            },
        )
    }

    /// `current_identity` reads a representative's present identity: one
    /// edited since it claimed `identity` has lost that text, so the
    /// claimant takes its place instead of being dropped as its duplicate.
    fn claim_identity(
        &mut self,
        path: &Path,
        identity: Option<String>,
        current_identity: impl FnOnce(&Path) -> Option<String>,
    ) -> ContentClaim {
        let Some(identity) = identity else {
            return ContentClaim::Unique;
        };
        let path_text = path.to_string_lossy().into_owned();
        self.release_previous_claim(&path_text, &identity);
        if let Some(representative) = self.map.physical_extents.get_mut(&identity) {
            if representative.path == path_text {
                return ContentClaim::Unique;
            }
            if current_identity(Path::new(&representative.path)).as_ref() != Some(&identity) {
                self.claimed_by.remove(&representative.path);
                representative.path.clone_from(&path_text);
                representative.duplicates.retain(|kept| *kept != path_text);
                self.claimed_by.insert(path_text, identity);
                self.dirty = true;
                return ContentClaim::Unique;
            }
            if representative.duplicates.len() < MAX_DUPLICATES
                && !representative.duplicates.contains(&path_text)
            {
                representative.duplicates.push(path_text);
                self.dirty = true;
            }
            return ContentClaim::Duplicate;
        }
        self.claim_new_identity(path_text, identity)
    }

    /// This file stood for other text before: whatever was skipped as a copy
    /// of that text has no document now.
    fn release_previous_claim(&mut self, path_text: &str, identity: &str) {
        if let Some(previous) = self.claimed_by.get(path_text).cloned()
            && previous != identity
        {
            self.claimed_by.remove(path_text);
            if let Some(old) = self.map.physical_extents.remove(&previous) {
                self.released
                    .extend(old.duplicates.into_iter().map(PathBuf::from));
            }
            self.dirty = true;
        }
    }

    /// The first file seen with `identity` becomes its representative.
    fn claim_new_identity(&mut self, path_text: String, identity: String) -> ContentClaim {
        self.claimed_by.insert(path_text.clone(), identity.clone());
        self.map.physical_extents.insert(
            identity,
            ContentRepresentative {
                path: path_text,
                duplicates: Vec::new(),
            },
        );
        self.dirty = true;
        ContentClaim::Unique
    }

    pub(crate) fn clear(&mut self) {
        if !self.map.physical_extents.is_empty() {
            self.map.physical_extents.clear();
            self.claimed_by.clear();
            self.dirty = true;
        }
    }

    pub(crate) fn persist(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary_file = self.file.with_extension("json.tmp");
        let bytes = serde_json::to_vec(&self.map)?;
        let mut file = std::fs::File::create(&temporary_file).context("create signatures tmp")?;
        // No fsync: the signatures are a cache the next pass rebuilds, and the
        // rename alone already keeps a reader from seeing half a file.
        file.write_all(&bytes).context("write signatures")?;
        std::fs::rename(&temporary_file, &self.file).context("rename signatures")?;
        self.dirty = false;
        Ok(())
    }
}

fn physical_content_identity(path: &Path, size: u64) -> Option<String> {
    if size == 0 {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() != size {
        return None;
    }
    if metadata.nlink() > 1 {
        return Some(format!(
            "inode-v1:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            size
        ));
    }

    extent_identity(&file, metadata.dev(), size)
}

/// The identity of a file's extent map, read through `FS_IOC_FIEMAP` in batches:
/// `None` unless every extent is physical, shared and safe to compare, and the
/// map is short enough to record.
fn extent_identity(file: &std::fs::File, dev: u64, size: u64) -> Option<String> {
    let mut extents = Vec::new();
    let mut has_physical_extents = false;
    let mut all_extents_shared = true;
    let mut start = 0u64;
    loop {
        let mapped_extents = fiemap(file, start)?;
        if mapped_extents.is_empty() {
            break;
        }
        for extent in mapped_extents {
            if extent.fe_flags & FIEMAP_UNSAFE_FLAGS != 0 {
                return None;
            }
            let next_start = extent.fe_logical.checked_add(extent.fe_length)?;
            let is_last = extent.fe_flags & FIEMAP_EXTENT_LAST != 0;
            has_physical_extents = true;
            all_extents_shared &= extent.fe_flags & FIEMAP_EXTENT_SHARED != 0;
            extents.push((extent.fe_logical, extent.fe_physical, extent.fe_length));
            if extents.len() > FIEMAP_MAX_RECORDED_EXTENTS {
                return None;
            }
            start = next_start;
            if is_last {
                return shared_extent_identity(
                    dev,
                    size,
                    &extents,
                    has_physical_extents,
                    all_extents_shared,
                );
            }
        }
    }
    shared_extent_identity(
        dev,
        size,
        &extents,
        has_physical_extents,
        all_extents_shared,
    )
}

fn shared_extent_identity(
    dev: u64,
    size: u64,
    extents: &[(u64, u64, u64)],
    has_physical_extents: bool,
    all_extents_shared: bool,
) -> Option<String> {
    if !has_physical_extents || !all_extents_shared {
        return None;
    }
    let mut identity = format!("fiemap-v1:{dev}:{size}:");
    for (logical, physical, length) in extents {
        write!(identity, "{logical:x},{physical:x},{length:x};").ok()?;
    }
    Some(identity)
}

fn fiemap(file: &std::fs::File, start: u64) -> Option<Vec<FiemapExtent>> {
    let mut request = FiemapRequest {
        header: FiemapHeader {
            fm_start: start,
            fm_length: u64::MAX,
            fm_flags: 0,
            fm_mapped_extents: 0,
            fm_extent_count: FIEMAP_EXTENTS_PER_CALL as u32,
            fm_reserved: 0,
        },
        extents: [FiemapExtent::default(); FIEMAP_EXTENTS_PER_CALL],
    };
    // SAFETY: FS_IOC_FIEMAP writes into a properly sized repr(C) buffer for a
    // valid file descriptor. On unsupported filesystems it fails and dedupe is skipped.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP, &mut request) };
    if rc != 0 {
        return None;
    }
    let mapped = (request.header.fm_mapped_extents as usize).min(FIEMAP_EXTENTS_PER_CALL);
    Some(request.extents[..mapped].to_vec())
}

const fn ioctl_read_write(ioctl_type: u8, number: u8, size: usize) -> libc::c_ulong {
    const IOC_NRBITS: u64 = 8;
    const IOC_TYPEBITS: u64 = 8;
    const IOC_SIZEBITS: u64 = 14;
    const IOC_NRSHIFT: u64 = 0;
    const IOC_TYPESHIFT: u64 = IOC_NRSHIFT + IOC_NRBITS;
    const IOC_SIZESHIFT: u64 = IOC_TYPESHIFT + IOC_TYPEBITS;
    const IOC_DIRSHIFT: u64 = IOC_SIZESHIFT + IOC_SIZEBITS;
    const IOC_WRITE: u64 = 1;
    const IOC_READ: u64 = 2;
    (((IOC_READ | IOC_WRITE) << IOC_DIRSHIFT)
        | ((ioctl_type as u64) << IOC_TYPESHIFT)
        | ((number as u64) << IOC_NRSHIFT)
        | ((size as u64) << IOC_SIZESHIFT)) as libc::c_ulong
}

#[repr(C)]
struct FiemapRequest {
    header: FiemapHeader,
    extents: [FiemapExtent; FIEMAP_EXTENTS_PER_CALL],
}

#[repr(C)]
struct FiemapHeader {
    fm_start: u64,
    fm_length: u64,
    fm_flags: u32,
    fm_mapped_extents: u32,
    fm_extent_count: u32,
    fm_reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FiemapExtent {
    fe_logical: u64,
    fe_physical: u64,
    fe_length: u64,
    fe_reserved64: [u64; 2],
    fe_flags: u32,
    fe_reserved: [u32; 3],
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn content_signatures_skip_shared_physical_identity() {
        let base =
            std::env::temp_dir().join(format!("lsearch-content-signatures-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let first = base.join("first.md");
        let second = base.join("second.md");
        std::fs::write(&first, b"same personal note").unwrap();
        std::fs::write(&second, b"same personal note").unwrap();

        let identity = || Some("fiemap-v1:1:18:0,1000,12;".to_string());
        let mut signatures = ContentSignatures::empty(base.join("content-signatures.json"));
        assert_eq!(
            signatures.claim_identity(&first, identity(), |_| unreachable!()),
            ContentClaim::Unique
        );
        assert_eq!(
            signatures.claim_identity(&second, identity(), |_| identity()),
            ContentClaim::Duplicate
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn edited_representative_releases_its_duplicates() {
        let mut signatures = ContentSignatures::empty(PathBuf::from("/nonexistent/sig.json"));
        let (first, second) = (Path::new("/d/a.md"), Path::new("/d/b.md"));
        let old = || Some("extents-old".to_owned());
        assert_eq!(
            signatures.claim_identity(first, old(), |_| unreachable!()),
            ContentClaim::Unique
        );
        assert_eq!(
            signatures.claim_identity(second, old(), |_| old()),
            ContentClaim::Duplicate
        );
        assert!(signatures.take_released().is_empty());
        // The representative is read again with new extents.
        assert_eq!(
            signatures.claim_identity(first, Some("extents-new".to_owned()), |_| unreachable!()),
            ContentClaim::Unique
        );
        assert_eq!(signatures.take_released(), [second.to_path_buf()]);
    }

    #[test]
    fn edited_representative_loses_its_identity() {
        let base =
            std::env::temp_dir().join(format!("lsearch-content-edited-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let (first, second) = (base.join("first.md"), base.join("second.md"));
        std::fs::write(&first, b"edited note").unwrap();
        std::fs::write(&second, b"old note").unwrap();

        let identity = || Some("fiemap-v1:1:18:0,1000,12;".to_string());
        let mut signatures = ContentSignatures::empty(base.join("content-signatures.json"));
        assert_eq!(
            signatures.claim_identity(&first, identity(), |_| unreachable!()),
            ContentClaim::Unique
        );
        let edited = Some("fiemap-v1:1:18:0,2000,12;".to_string());
        assert_eq!(
            signatures.claim_identity(&second, identity(), |_| edited),
            ContentClaim::Unique
        );
        // The claimant is the representative now.
        assert_eq!(
            signatures.claim_identity(&first, identity(), |_| identity()),
            ContentClaim::Duplicate
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn equal_text_needs_physical_identity() {
        let base = std::env::temp_dir().join(format!(
            "lsearch-content-no-identity-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let first = base.join("first.md");
        let second = base.join("second.md");
        std::fs::write(&first, b"same personal note").unwrap();
        std::fs::write(&second, b"same personal note").unwrap();

        let mut signatures = ContentSignatures::empty(base.join("content-signatures.json"));
        assert_eq!(
            signatures.claim_identity(&first, None, |_| unreachable!()),
            ContentClaim::Unique
        );
        assert_eq!(
            signatures.claim_identity(&second, None, |_| unreachable!()),
            ContentClaim::Unique
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn reflinked_files_share_physical_identity() {
        let base = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("lsearch-content-reflink-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let first = base.join("first.bin");
        let second = base.join("second.bin");
        // Incompressible bytes: btrfs stores compressible data compressed
        // (compress-force on BigLinux), and FIEMAP reports such an extent as
        // ENCODED, which has no identity.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let bytes: Vec<u8> = (0..2 * 1024 * 1024 / 8)
            .flat_map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()
            })
            .collect();
        std::fs::write(&first, bytes).unwrap();
        // Force extent allocation: a freshly written file may still be in
        // delalloc, which FIEMAP reports as an unsafe flag → identity None and
        // a flaky assert depending on writeback timing.
        std::fs::File::open(&first).unwrap().sync_all().unwrap();

        if crate::history::clone_file(&first, &second).is_err() {
            std::fs::remove_dir_all(&base).ok();
            eprintln!("skipped: this filesystem cannot clone files");
            return;
        }

        let size = std::fs::metadata(&first).unwrap().len();
        let first_identity = physical_content_identity(&first, size);
        let second_identity = physical_content_identity(&second, size);
        std::fs::remove_dir_all(&base).ok();
        // Extents with unsafe FIEMAP flags (encrypted, inline) have no identity
        // by design; then neither clone may have one.
        if first_identity.is_none() {
            eprintln!("skipped: these extents carry no physical identity");
        }
        assert_eq!(first_identity, second_identity);
    }
}
