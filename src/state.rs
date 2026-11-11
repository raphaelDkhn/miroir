//! State directories: bake moves them into the pristine copy, each reset copies them back,
//! and scans catch writes outside them.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown, symlink};
use std::path::{Path, PathBuf};

use uuid::Uuid;

/// Where `dir` lives inside a copy rooted at `pristine`.
pub fn pristine_dir(pristine: &Path, dir: &Path) -> PathBuf {
    pristine.join(dir.strip_prefix("/").unwrap_or(dir))
}

/// Move each state directory's contents into the pristine copy; a state file is
/// copied.
pub fn bake_move(dirs: &[PathBuf], pristine: &Path) -> io::Result<()> {
    for dir in dirs {
        ensure_no_symlink_above(dir)?;
        let meta = fs::symlink_metadata(dir).map_err(|e| context(&e, dir))?;
        let target = pristine_dir(pristine, dir);
        if meta.is_file() {
            create_parent(&target)?;
            copy_tree(dir, &target)?;
            continue;
        }
        if !meta.is_dir() {
            let message = format!("{} must be a directory or a file, not a symlink", dir.display());
            return Err(io::Error::new(ErrorKind::InvalidInput, message));
        }
        fs::create_dir_all(&target).map_err(|e| context(&e, &target))?;
        copy_owner_and_mode(&meta, &target)?;
        for entry in fs::read_dir(dir).map_err(|e| context(&e, dir))? {
            let from = entry?.path();
            let to = target.join(from.file_name().unwrap_or_default());
            match fs::rename(&from, &to) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::CrossesDevices => {
                    copy_tree(&from, &to)?;
                    remove(&from)?;
                }
                Err(e) => return Err(context(&e, &from)),
            }
        }
    }
    Ok(())
}

/// Stamps of the files the last restore or save left, by path.
pub type Stamps = HashMap<PathBuf, Stamp>;

/// A file's inode, size and times, which change when anything touches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Stamp {
        Stamp {
            inode: meta.ino(),
            size: meta.size(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// Make the state match the copy at `source`, copying only files changed since their
/// stamp.
pub fn restore(dirs: &[PathBuf], source: &Path, stamps: &mut Stamps) -> io::Result<()> {
    for dir in dirs {
        ensure_no_symlink_above(dir)?;
        let from = pristine_dir(source, dir);
        let meta = fs::metadata(&from).map_err(|e| context(&e, &from))?;
        if meta.is_file() {
            restore_file(&from, dir, &meta, stamps)?;
            continue;
        }
        match fs::symlink_metadata(dir) {
            Ok(found) if found.is_dir() => {}
            Ok(_) => {
                remove(dir)?;
                fs::create_dir(dir).map_err(|e| context(&e, dir))?;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                fs::create_dir_all(dir).map_err(|e| context(&e, dir))?;
            }
            Err(e) => return Err(context(&e, dir)),
        }
        sync_tree(&from, dir, stamps)?;
        copy_owner_and_mode(&meta, dir)?;
    }
    Ok(())
}

fn restore_file(
    from: &Path,
    to: &Path,
    meta: &fs::Metadata,
    stamps: &mut Stamps,
) -> io::Result<()> {
    match fs::symlink_metadata(to) {
        Ok(found) if found.is_file() => {
            if stamps.get(to) == Some(&Stamp::of(&found)) {
                return Ok(());
            }
            fs::copy(from, to).map_err(|e| context(&e, to))?;
            copy_owner_and_mode(meta, to)?;
        }
        Ok(_) => {
            remove(to)?;
            copy_tree(from, to)?;
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            create_parent(to)?;
            copy_tree(from, to)?;
        }
        Err(e) => return Err(context(&e, to)),
    }
    let left = fs::symlink_metadata(to).map_err(|e| context(&e, to))?;
    stamps.insert(to.to_path_buf(), Stamp::of(&left));
    Ok(())
}

fn create_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) => fs::create_dir_all(parent).map_err(|e| context(&e, parent)),
        None => Ok(()),
    }
}

fn sync_tree(from: &Path, to: &Path, stamps: &mut Stamps) -> io::Result<()> {
    let mut wanted = HashSet::<OsString>::new();
    let sources = fs::read_dir(from).map_err(|e| context(&e, from))?;
    let sources = sources.collect::<io::Result<Vec<_>>>().map_err(|e| context(&e, from))?;
    wanted.extend(sources.iter().map(fs::DirEntry::file_name));
    for entry in fs::read_dir(to).map_err(|e| context(&e, to))? {
        let entry = entry?;
        if !wanted.contains(&entry.file_name()) {
            remove(&entry.path())?;
        }
    }
    for entry in sources {
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let meta = fs::symlink_metadata(&source).map_err(|e| context(&e, &source))?;
        let found = match fs::symlink_metadata(&target) {
            Ok(found) => Some(found),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(context(&e, &target)),
        };
        if meta.is_dir() {
            if found.as_ref().is_some_and(|found| !found.is_dir()) {
                remove(&target)?;
            }
            if !found.as_ref().is_some_and(fs::Metadata::is_dir) {
                fs::create_dir(&target).map_err(|e| context(&e, &target))?;
            }
            sync_tree(&source, &target, stamps)?;
            copy_owner_and_mode(&meta, &target)?;
            continue;
        }
        if found.as_ref().is_some_and(|found| stamps.get(&target) == Some(&Stamp::of(found))) {
            continue;
        }
        stamps.remove(&target);
        if found.is_some() {
            remove(&target)?;
        }
        copy_tree(&source, &target)?;
        if let Ok(left) = fs::symlink_metadata(&target) {
            stamps.insert(target, Stamp::of(&left));
        }
    }
    Ok(())
}

/// Copy the state to `target`, and stamp the files it copied.
pub fn save(dirs: &[PathBuf], target: &Path) -> io::Result<Stamps> {
    if fs::symlink_metadata(target).is_ok() {
        remove(target)?;
    }
    fs::create_dir_all(target).map_err(|e| context(&e, target))?;
    fs::set_permissions(target, fs::Permissions::from_mode(0o700))
        .map_err(|e| context(&e, target))?;
    let mut stamps = Stamps::new();
    for dir in dirs {
        ensure_no_symlink_above(dir)?;
        let to = pristine_dir(target, dir);
        let meta = fs::symlink_metadata(dir).map_err(|e| context(&e, dir))?;
        if meta.is_file() {
            create_parent(&to)?;
            copy_tree(dir, &to)?;
            stamps.insert(dir.clone(), Stamp::of(&meta));
            continue;
        }
        fs::create_dir_all(&to).map_err(|e| context(&e, &to))?;
        copy_owner_and_mode(&meta, &to)?;
        stamp_tree(dir, &to, &mut stamps)?;
    }
    Ok(stamps)
}

fn stamp_tree(from: &Path, to: &Path, stamps: &mut Stamps) -> io::Result<()> {
    for entry in fs::read_dir(from).map_err(|e| context(&e, from))? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let meta = fs::symlink_metadata(&source).map_err(|e| context(&e, &source))?;
        if meta.is_dir() {
            fs::create_dir(&target).map_err(|e| context(&e, &target))?;
            stamp_tree(&source, &target, stamps)?;
            copy_owner_and_mode(&meta, &target)?;
        } else {
            copy_tree(&source, &target)?;
            stamps.insert(source, Stamp::of(&meta));
        }
    }
    Ok(())
}

/// Remove everything inside a directory; a missing one is fine.
pub fn empty(dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(context(&e, dir)),
    };
    for entry in entries {
        remove(&entry?.path())?;
    }
    Ok(())
}

fn ensure_no_symlink_above(dir: &Path) -> io::Result<()> {
    for ancestor in dir.ancestors().skip(1) {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) if meta.is_symlink() => {
                let message = format!(
                    "{} goes through the symlink {}; name the real path",
                    dir.display(),
                    ancestor.display()
                );
                return Err(io::Error::new(ErrorKind::InvalidInput, message));
            }
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(context(&e, ancestor)),
            _ => {}
        }
    }
    Ok(())
}

fn remove(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path).map_err(|e| context(&e, path))?;
    let result = if meta.is_dir() { remove_tree(path) } else { fs::remove_file(path) };
    result.map_err(|e| context(&e, path))
}

fn remove_tree(root: &Path) -> io::Result<()> {
    let mut pending = vec![root.to_path_buf()];
    let mut emptied = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)?.collect::<io::Result<Vec<_>>>()? {
            if !entry.file_type()?.is_dir() {
                fs::remove_file(entry.path())?;
                continue;
            }
            let moved = root.join(Uuid::new_v4().to_string());
            match fs::rename(entry.path(), &moved) {
                Ok(()) => pending.push(moved),
                Err(e) if e.kind() == ErrorKind::CrossesDevices => pending.push(entry.path()),
                Err(e) => return Err(e),
            }
        }
        emptied.push(dir);
    }
    emptied.iter().rev().try_for_each(fs::remove_dir)
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from).map_err(|e| context(&e, from))?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        symlink(fs::read_link(from)?, to).map_err(|e| context(&e, to))?;
        lchown(to, Some(meta.uid()), Some(meta.gid())).map_err(|e| context(&e, to))?;
    } else if kind.is_dir() {
        fs::create_dir(to).map_err(|e| context(&e, to))?;
        for entry in fs::read_dir(from).map_err(|e| context(&e, from))? {
            let child = entry?.path();
            copy_tree(&child, &to.join(child.file_name().unwrap_or_default()))?;
        }
        copy_owner_and_mode(&meta, to)?;
    } else if kind.is_file() {
        fs::copy(from, to).map_err(|e| context(&e, from))?;
        copy_owner_and_mode(&meta, to)?;
    } else {
        tracing::debug!(path = %from.display(), "skipped a special file");
    }
    Ok(())
}

fn copy_owner_and_mode(meta: &fs::Metadata, to: &Path) -> io::Result<()> {
    lchown(to, Some(meta.uid()), Some(meta.gid())).map_err(|e| context(&e, to))?;
    fs::set_permissions(to, meta.permissions()).map_err(|e| context(&e, to))
}

fn context(e: &io::Error, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

/// Emptied at every restore.
pub const TEMP_DIRS: [&str; 3] = ["/tmp", "/var/tmp", "/dev/shm"];
/// Never walked to derive the state.
pub const UNWALKED: [&str; 4] = ["/proc", "/sys", "/dev", "/miroir"];
const WRITE: u32 = 0o2;
const SEARCH: u32 = 0o1;

/// Whether `user` has `bits` in the permission class it falls in; an owner always
/// does.
pub fn permits(mode: u32, owner: (u32, u32), user: (u32, u32), bits: u32) -> bool {
    if user.0 == owner.0 {
        return true;
    }
    let class = if user.1 == owner.1 { mode >> 3 } else { mode };
    class & bits == bits
}

/// The topmost paths under `top`, on its filesystem, that any of `users` can write.
pub fn writable_roots(
    top: &Path,
    users: &[(u32, u32)],
    skip: &[PathBuf],
) -> io::Result<Vec<PathBuf>> {
    let device = fs::symlink_metadata(top).map_err(|e| context(&e, top))?.dev();
    let mut roots = Vec::new();
    let mut pending = vec![(top.to_path_buf(), users.to_vec())];
    while let Some((dir, reaching)) = pending.pop() {
        let meta = fs::symlink_metadata(&dir).map_err(|e| context(&e, &dir))?;
        let owner = (meta.uid(), meta.gid());
        if reaching.iter().any(|&user| permits(meta.mode(), owner, user, WRITE | SEARCH)) {
            roots.push(dir);
            continue;
        }
        let inside: Vec<(u32, u32)> = reaching
            .into_iter()
            .filter(|&user| permits(meta.mode(), owner, user, SEARCH))
            .collect();
        if inside.is_empty() {
            continue;
        }
        for entry in fs::read_dir(&dir).map_err(|e| context(&e, &dir))? {
            let path = entry.map_err(|e| context(&e, &dir))?.path();
            if skip.iter().any(|s| path.starts_with(s)) {
                continue;
            }
            let meta = fs::symlink_metadata(&path).map_err(|e| context(&e, &path))?;
            let owner = (meta.uid(), meta.gid());
            if meta.is_dir() && meta.dev() == device {
                pending.push((path, inside.clone()));
            } else if meta.is_file()
                && inside.iter().any(|&user| permits(meta.mode(), owner, user, WRITE))
            {
                roots.push(path);
            }
        }
    }
    roots.sort();
    Ok(roots)
}
/// The container runtime mounts the last three and touches them, as `docker cp` does.
const UNSCANNED: [&str; 9] = [
    "/run",
    "/proc",
    "/sys",
    "/dev",
    "/var/log",
    "/miroir",
    "/etc/hostname",
    "/etc/hosts",
    "/etc/resolv.conf",
];

/// Each path's mtime, ctime, size and inode.
pub type Scan = BTreeMap<PathBuf, (i64, i64, u64, u64)>;

/// Scan the filesystem outside the state, temporary and system directories.
pub async fn scan(state: &[PathBuf]) -> Scan {
    let skip: Vec<PathBuf> = TEMP_DIRS
        .iter()
        .chain(&UNSCANNED)
        .map(PathBuf::from)
        .chain(state.iter().cloned())
        .collect();
    tokio::task::spawn_blocking(move || {
        let mut found = Scan::new();
        let mut pending = vec![PathBuf::from("/")];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if skip.iter().any(|s| path.starts_with(s)) {
                    continue;
                }
                let Ok(meta) = fs::symlink_metadata(&path) else { continue };
                let changed = if meta.is_dir() {
                    pending.push(path.clone());
                    0
                } else {
                    meta.ctime() * 1_000_000_000 + meta.ctime_nsec()
                };
                let modified = meta.mtime() * 1_000_000_000 + meta.mtime_nsec();
                found.insert(path, (modified, changed, meta.size(), meta.ino()));
            }
        }
        found
    })
    .await
    .unwrap_or_default()
}

/// Paths added, removed or changed between two scans.
pub fn diff(before: &Scan, after: &Scan) -> Vec<String> {
    let paths: BTreeSet<&PathBuf> = before.keys().chain(after.keys()).collect();
    paths
        .into_iter()
        .filter(|p| before.get(*p) != after.get(*p))
        .map(|p| p.display().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("miroir-{name}-{}", std::process::id()));
        let _ = remove(&root);
        root
    }

    #[test]
    fn a_tree_too_deep_for_any_path_or_stack_is_removed() {
        let root = temp_root("deep");
        fs::create_dir_all(&root).unwrap();
        let (deep, wrapper) = (root.join("deep"), root.join("wrapper"));
        fs::create_dir(&deep).unwrap();
        fs::write(deep.join("file"), "x").unwrap();
        for _ in 0..5_000 {
            fs::create_dir(&wrapper).unwrap();
            fs::rename(&deep, wrapper.join("n")).unwrap();
            fs::rename(&wrapper, &deep).unwrap();
        }
        empty(&root).unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn a_state_directory_behind_a_symlink_is_refused() {
        let root = temp_root("above");
        let real = root.join("real");
        fs::create_dir_all(real.join("data")).unwrap();
        symlink(&real, root.join("link")).unwrap();
        let dirs = [root.join("link/data")];
        let pristine = root.join("pristine");
        assert_eq!(bake_move(&dirs, &pristine).unwrap_err().kind(), ErrorKind::InvalidInput);
        let refused = restore(&dirs, &pristine, &mut Stamps::new()).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::InvalidInput);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bake_moves_then_restore_copies_back() {
        let root = temp_root("state");
        let state = root.join("data");
        let pristine = root.join("pristine");
        fs::create_dir_all(state.join("sub")).unwrap();
        fs::write(state.join("sub/file"), "seed").unwrap();
        fs::set_permissions(state.join("sub/file"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        symlink("sub/file", state.join("link")).unwrap();
        let dirs = [state.clone()];

        bake_move(&dirs, &pristine).unwrap();
        assert_eq!(fs::read_dir(&state).unwrap().count(), 0);
        let kept = pristine_dir(&pristine, &state);
        assert_eq!(fs::read_to_string(kept.join("sub/file")).unwrap(), "seed");

        fs::write(state.join("junk"), "episode write").unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        restore(&dirs, &pristine, &mut Stamps::new()).unwrap();
        assert!(!state.join("junk").exists());
        assert_eq!(fs::read_to_string(state.join("link")).unwrap(), "seed");
        assert_eq!(fs::read_link(state.join("link")).unwrap(), Path::new("sub/file"));
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&state), mode(&state.join("sub/file"))), (0o700, 0o600));
        assert_eq!(fs::read_to_string(kept.join("sub/file")).unwrap(), "seed");
        empty(&root.join("missing")).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_restore_keeps_only_what_nothing_touched() {
        let root = temp_root("touched");
        let (state, pristine) = (root.join("data"), root.join("pristine"));
        fs::create_dir_all(state.join("sub")).unwrap();
        for name in ["kept", "edited", "deleted", "chmodded", "backdated", "sub/nested"] {
            fs::write(state.join(name), name).unwrap();
        }
        fs::set_permissions(state.join("chmodded"), fs::Permissions::from_mode(0o600)).unwrap();
        symlink("kept", state.join("link")).unwrap();
        let dirs = [state.clone()];
        bake_move(&dirs, &pristine).unwrap();
        let mut stamps = Stamps::new();
        restore(&dirs, &pristine, &mut stamps).unwrap();
        let stamp = |name: &str| Stamp::of(&fs::symlink_metadata(state.join(name)).unwrap());
        let before = (stamp("kept"), stamp("link"), stamp("edited"));
        std::thread::sleep(std::time::Duration::from_millis(20));

        fs::write(state.join("edited"), "episode write").unwrap();
        fs::remove_file(state.join("deleted")).unwrap();
        fs::write(state.join("added"), "episode file").unwrap();
        fs::create_dir(state.join("added-dir")).unwrap();
        fs::write(state.join("added-dir/inner"), "x").unwrap();
        fs::set_permissions(state.join("chmodded"), fs::Permissions::from_mode(0o644)).unwrap();
        let backdated = fs::File::options().write(true).open(state.join("backdated")).unwrap();
        let old = fs::metadata(state.join("backdated")).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::io::Write::write_all(&mut &backdated, b"BACKDATED").unwrap();
        backdated.set_modified(old).unwrap();
        drop(backdated);
        fs::remove_file(state.join("sub/nested")).unwrap();
        fs::create_dir(state.join("sub/nested")).unwrap();
        fs::remove_dir_all(state.join("sub")).unwrap();
        fs::write(state.join("sub"), "a file where a directory was").unwrap();

        restore(&dirs, &pristine, &mut stamps).unwrap();
        let read = |name: &str| fs::read_to_string(state.join(name)).unwrap();
        assert_eq!(read("edited"), "edited");
        assert_eq!(read("deleted"), "deleted");
        assert_eq!(read("backdated"), "backdated", "the mtime was no alibi");
        assert_eq!(read("sub/nested"), "sub/nested");
        assert!(!state.join("added").exists() && !state.join("added-dir").exists());
        let mode = |name: &str| fs::metadata(state.join(name)).unwrap().permissions().mode();
        assert_eq!(mode("chmodded") & 0o777, 0o600);
        assert_eq!((before.0, before.1), (stamp("kept"), stamp("link")), "not copied again");
        assert_ne!(before.2, stamp("edited"), "replaced");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_saved_copy_restores_and_vouches_for_what_it_copied() {
        let root = temp_root("saved");
        let (state, saved) = (root.join("data"), root.join("saved"));
        fs::create_dir_all(state.join("sub")).unwrap();
        fs::write(state.join("sub/file"), "shifted").unwrap();
        fs::write(state.join("other"), "shifted").unwrap();
        let dirs = [state.clone()];
        let mut stamps = save(&dirs, &saved).unwrap();
        let stamp = |name: &str| Stamp::of(&fs::symlink_metadata(state.join(name)).unwrap());
        let kept = stamp("sub/file");
        let mode = fs::metadata(&saved).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "seed data stays closed to the product's users");

        fs::write(state.join("other"), "episode write").unwrap();
        restore(&dirs, &saved, &mut stamps).unwrap();
        assert_eq!(fs::read_to_string(state.join("other")).unwrap(), "shifted");
        assert_eq!(stamp("sub/file"), kept, "the copy already matched");

        fs::write(state.join("other"), "next day").unwrap();
        let again = save(&dirs, &saved).unwrap();
        assert_eq!(again.len(), 2, "saving again replaces the copy");
        let copy = pristine_dir(&saved, &state).join("other");
        assert_eq!(fs::read_to_string(copy).unwrap(), "next day");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_tree_too_deep_for_any_path_or_stack_inside_state_is_restored_away() {
        let root = temp_root("state-deep");
        let (state, pristine) = (root.join("data"), root.join("pristine"));
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("seed"), "seed").unwrap();
        let dirs = [state.clone()];
        bake_move(&dirs, &pristine).unwrap();
        let mut stamps = Stamps::new();
        restore(&dirs, &pristine, &mut stamps).unwrap();
        let (deep, wrapper) = (state.join("deep"), state.join("wrapper"));
        fs::create_dir(&deep).unwrap();
        for _ in 0..5_000 {
            fs::create_dir(&wrapper).unwrap();
            fs::rename(&deep, wrapper.join("n")).unwrap();
            fs::rename(&wrapper, &deep).unwrap();
        }
        restore(&dirs, &pristine, &mut stamps).unwrap();
        assert!(!deep.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn permits_goes_by_the_class_a_user_falls_in() {
        let owner = (1, 10);
        let (group, other) = ((2, 10), (3, 30));
        assert!(permits(0o000, owner, owner, WRITE | SEARCH), "an owner can chmod its way in");
        assert!(!permits(0o707, owner, group, SEARCH), "the group class never uses other bits");
        assert!(permits(0o070, owner, group, WRITE | SEARCH));
        assert!(!permits(0o772, owner, other, WRITE | SEARCH), "no adding without searching");
        assert!(permits(0o772, owner, other, WRITE));
        assert!(permits(0o1777, owner, other, WRITE | SEARCH), "sticky is still writable");
    }

    #[test]
    fn writable_roots_are_what_each_user_can_reach_and_write() {
        let root = temp_root("roots");
        let mode = |path: &Path, mode: u32| {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        };
        for dir in ["locked/open", "shared", "conf", "world", "skipped"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("conf/app.ini"), "").unwrap();
        fs::write(root.join("conf/readonly"), "").unwrap();
        symlink(root.join("world"), root.join("link")).unwrap();
        for (path, bits) in [
            ("", 0o755),
            ("locked/open", 0o777),
            ("locked", 0o700),
            ("shared", 0o775),
            ("conf", 0o755),
            ("conf/app.ini", 0o666),
            ("conf/readonly", 0o644),
            ("world", 0o777),
            ("skipped", 0o777),
        ] {
            mode(&root.join(path), bits);
        }
        let gid = fs::metadata(&root).unwrap().gid();
        let skip = [root.join("skipped")];
        let roots = |user: (u32, u32)| -> Vec<String> {
            let found = writable_roots(&root, &[user], &skip).unwrap();
            found.iter().map(|p| p.strip_prefix(&root).unwrap().display().to_string()).collect()
        };
        assert_eq!(roots((4242, gid)), ["conf/app.ini", "shared", "world"]);
        assert_eq!(roots((4242, 4243)), ["conf/app.ini", "world"], "locked/open is out of reach");
        mode(&root.join("locked"), 0o755);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_state_file_is_copied_at_bake_and_rewritten_in_place() {
        let root = temp_root("file");
        let (dir, pristine) = (root.join("etc"), root.join("pristine"));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("app.ini");
        fs::write(&file, "seed").unwrap();
        let roots = [file.clone()];
        bake_move(&roots, &pristine).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "seed", "the live file stays");
        let mut stamps = Stamps::new();
        restore(&roots, &pristine, &mut stamps).unwrap();
        let (inode, listed) =
            (fs::metadata(&file).unwrap().ino(), fs::metadata(&dir).unwrap().modified().unwrap());
        std::thread::sleep(std::time::Duration::from_millis(20));

        fs::write(&file, "episode write").unwrap();
        restore(&roots, &pristine, &mut stamps).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "seed");
        assert_eq!(fs::metadata(&file).unwrap().ino(), inode, "rewritten, not replaced");
        assert_eq!(fs::metadata(&dir).unwrap().modified().unwrap(), listed, "dir untouched");
        fs::remove_file(&file).unwrap();
        restore(&roots, &pristine, &mut stamps).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "seed", "put back when gone");

        fs::write(&file, "shifted").unwrap();
        let saved = root.join("saved");
        let mut stamps = save(&roots, &saved).unwrap();
        assert_eq!(fs::read_to_string(pristine_dir(&saved, &file)).unwrap(), "shifted");
        assert!(stamps.contains_key(&file), "the copy vouches for the live file");
        restore(&roots, &saved, &mut stamps).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "shifted");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restore_replaces_a_symlink_instead_of_following_it() {
        let root = temp_root("link");
        let (state, pristine, outside) =
            (root.join("data"), root.join("pristine"), root.join("outside"));
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(state.join("seed"), "seed").unwrap();
        fs::write(outside.join("precious"), "keep").unwrap();
        let dirs = [state.clone()];
        bake_move(&dirs, &pristine).unwrap();

        fs::remove_dir(&state).unwrap();
        symlink(&outside, &state).unwrap();
        let refused = bake_move(&dirs, &pristine.join("again")).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::InvalidInput);
        restore(&dirs, &pristine, &mut Stamps::new()).unwrap();

        assert_eq!(fs::read_to_string(outside.join("precious")).unwrap(), "keep");
        assert!(!outside.join("seed").exists(), "nothing was copied through the link");
        assert!(fs::symlink_metadata(&state).unwrap().is_dir(), "a real directory again");
        assert_eq!(fs::read_to_string(state.join("seed")).unwrap(), "seed");
        fs::remove_dir_all(&root).unwrap();
    }
}
