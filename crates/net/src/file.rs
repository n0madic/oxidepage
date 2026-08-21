//! `file://` loading (design doc §8): opt-in, jailed, regular files only.
//!
//! Disabled unless [`ResourcePolicy::allow_file`]. When a `file_root` jail is
//! set, the canonicalized target must stay within it — which rejects both
//! `..` traversal and symlinks that point outside the jail, since
//! canonicalization resolves them before the containment check.
//!
//! **Caveat the jail does not close:** a hard link *inside* the jail pointing at
//! a file outside it is indistinguishable from an ordinary jail member by any
//! path-based check — it has no link target to resolve and its canonical path is
//! genuinely inside the root. Closing that needs the jail to be a mount or user
//! namespace, not a prefix comparison. `allow_file` is off by default precisely
//! because "jailed" here means "path-confined", not "sandboxed".

use std::io::Read;
use std::path::Path;

use oxidepage_base::NetErrorKind;
use url::Url;

use crate::error::{NetError, NetResult};
use crate::policy::ResourcePolicy;

/// A loaded local file: its bytes and a best-effort content type.
#[derive(Debug)]
pub struct FileBody {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

/// Loads a `file://` URL under `policy`, reading at most `cap` bytes.
///
/// `cap` comes from the same cumulative byte budget an HTTP response is charged
/// against, and is enforced on the *read*, not on `metadata().len()`: a length
/// taken from metadata is a hint even for a regular file (it can change between
/// the `stat` and the read, and `/proc`-style files report zero while producing
/// unbounded output). Reading `cap + 1` and refusing on overflow needs no trust
/// in the number.
pub fn load_file(policy: &ResourcePolicy, url: &Url, cap: u64) -> NetResult<FileBody> {
    if !policy.allow_file {
        return Err(NetError::blocked("file:// scheme is disabled by policy"));
    }
    let path = url
        .to_file_path()
        .map_err(|()| NetError::invalid_url(format!("not a usable file path: {url}")))?;

    // Reject a direct symlink target outright (regular files only). Intermediate
    // symlink escapes are caught by the canonical containment check below.
    if let Ok(meta) = std::fs::symlink_metadata(&path)
        && meta.file_type().is_symlink()
    {
        return Err(NetError::new(
            NetErrorKind::File,
            format!("refusing to follow symlink: {}", path.display()),
        ));
    }

    // Open the file *once* and perform every subsequent check and the read on
    // this handle. Reading the opened inode (rather than re-opening the path
    // after canonicalizing) closes the check-then-reopen TOCTOU gap: the bytes
    // we return are the object we vetted, not one an attacker swapped in.
    let file = std::fs::File::open(&path)
        .map_err(|e| NetError::new(NetErrorKind::File, format!("{}: {e}", path.display())))?;

    // Regular-file check against the open handle, not a fresh path lookup.
    let meta = file
        .metadata()
        .map_err(|e| NetError::new(NetErrorKind::File, e.to_string()))?;
    if !meta.is_file() {
        return Err(NetError::new(
            NetErrorKind::File,
            format!("not a regular file: {}", path.display()),
        ));
    }

    let canonical = std::fs::canonicalize(&path)
        .map_err(|e| NetError::new(NetErrorKind::File, format!("{}: {e}", path.display())))?;

    if let Some(root) = &policy.file_root {
        let root = std::fs::canonicalize(root)
            .map_err(|e| NetError::new(NetErrorKind::File, format!("file_root: {e}")))?;
        if !canonical.starts_with(&root) {
            return Err(NetError::blocked(format!(
                "path escapes file_root jail: {}",
                canonical.display()
            )));
        }
        // `canonicalize` is a *second* path walk, so what it vetted is not
        // necessarily what we opened: swap a directory component for a symlink
        // between the two calls and the containment check passes on a path that
        // no longer names the open inode. Confirming the handle *is* the object
        // the check cleared closes that window — the read below then cannot be
        // of anything else.
        if !same_object(&file, &canonical)
            .map_err(|e| NetError::new(NetErrorKind::File, e.to_string()))?
        {
            return Err(NetError::blocked(format!(
                "path changed under the open file: {}",
                canonical.display()
            )));
        }
    }

    // `cap + 1`: reading one byte past the limit is what distinguishes "exactly
    // at the cap" from "over it" without trusting a reported length.
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| NetError::new(NetErrorKind::File, e.to_string()))?;
    if bytes.len() as u64 > cap {
        return Err(NetError::blocked(format!(
            "file exceeds the response byte budget ({cap} bytes): {}",
            canonical.display()
        )));
    }
    Ok(FileBody {
        bytes,
        content_type: guess_content_type(&canonical),
    })
}

/// Whether `file` and the object `path` currently names are the same object.
///
/// Only the jail needs this, and only against a *canonical* path. There is no
/// portable answer, so each platform gets the identity its filesystem API
/// exposes and the rest degrade to the path-only check.
#[cfg(unix)]
fn same_object(file: &std::fs::File, path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let open = file.metadata()?;
    let named = std::fs::metadata(path)?;
    Ok(open.dev() == named.dev() && open.ino() == named.ino())
}

/// Windows has no `dev`/`ino`; the equivalent pair is the volume serial number
/// and the file index, and `std` surfaces them only on a **handle-derived**
/// `Metadata` — `fs::metadata` returns `None` for both. So the comparison needs
/// a second open of the canonical path rather than a cheap `stat`.
#[cfg(windows)]
fn same_object(file: &std::fs::File, path: &Path) -> std::io::Result<bool> {
    use std::os::windows::fs::MetadataExt;
    let open = file.metadata()?;
    let named = std::fs::File::open(path)?.metadata()?;
    Ok(open.volume_serial_number() == named.volume_serial_number()
        && open.file_index() == named.file_index())
}

/// Everywhere else (wasm and friends): no inode identity is available, so the
/// jail degrades to the path-only containment check above. Documented rather
/// than silently `false`, which would refuse every jailed read.
#[cfg(not(any(unix, windows)))]
fn same_object(_file: &std::fs::File, _path: &Path) -> std::io::Result<bool> {
    Ok(true)
}

/// A tiny extension → MIME map for local files (the common web types).
fn guess_content_type(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "xhtml" => "application/xhtml+xml",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "text/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        _ => return None,
    };
    Some(mime.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A byte cap generous enough that no test hits it by accident; the tests
    /// that *are* about the cap pass their own.
    const CAP: u64 = 1 << 20;

    /// A unique, freshly-created temp directory for one test.
    fn unique_dir(tag: &str) -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("oxidepage-file-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn jailed_policy(root: &Path) -> ResourcePolicy {
        ResourcePolicy {
            allow_file: true,
            file_root: Some(root.to_path_buf()),
            ..ResourcePolicy::default()
        }
    }

    #[test]
    fn reads_regular_file_within_jail() {
        let root = unique_dir("read");
        let path = root.join("index.html");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"<h1>hi</h1>")
            .unwrap();
        let url = Url::from_file_path(&path).unwrap();
        let body = load_file(&jailed_policy(&root), &url, CAP).unwrap();
        assert_eq!(body.bytes, b"<h1>hi</h1>");
        assert_eq!(
            body.content_type.as_deref(),
            Some("text/html; charset=utf-8")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn traversal_outside_jail_is_blocked() {
        let root = unique_dir("jail");
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        // A secret in the jail root, outside the `inner` sub-jail.
        let secret = root.join("secret.txt");
        std::fs::File::create(&secret)
            .unwrap()
            .write_all(b"top-secret")
            .unwrap();
        // Jail is `inner`; a `..` escape must resolve outside it and be blocked.
        let escape = inner.join("../secret.txt");
        let url = Url::from_file_path(&escape).unwrap();
        let err = load_file(&jailed_policy(&inner), &url, CAP).unwrap_err();
        assert_eq!(err.kind, NetErrorKind::Blocked, "detail: {}", err.detail);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn file_scheme_disabled_by_default() {
        let root = unique_dir("disabled");
        let path = root.join("a.txt");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        let url = Url::from_file_path(&path).unwrap();
        let err = load_file(&ResourcePolicy::default(), &url, CAP).unwrap_err();
        assert_eq!(err.kind, NetErrorKind::Blocked);
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn direct_symlink_is_refused() {
        let root = unique_dir("symlink");
        let target = root.join("real.txt");
        std::fs::File::create(&target)
            .unwrap()
            .write_all(b"data")
            .unwrap();
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let url = Url::from_file_path(&link).unwrap();
        let policy = ResourcePolicy {
            allow_file: true,
            ..ResourcePolicy::default()
        };
        let err = load_file(&policy, &url, CAP).unwrap_err();
        assert_eq!(err.kind, NetErrorKind::File, "detail: {}", err.detail);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn file_over_the_cap_is_blocked_and_the_boundary_is_inclusive() {
        let root = unique_dir("cap");
        let path = root.join("big.txt");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&[b'x'; 64])
            .unwrap();
        let url = Url::from_file_path(&path).unwrap();
        let policy = jailed_policy(&root);

        // Exactly at the cap: allowed, and the whole file comes back.
        let body = load_file(&policy, &url, 64).unwrap();
        assert_eq!(body.bytes.len(), 64);

        // One byte under: refused. The read stops at `cap + 1`, so the refusal
        // does not depend on the file being small enough to buffer.
        let err = load_file(&policy, &url, 63).unwrap_err();
        assert_eq!(err.kind, NetErrorKind::Blocked, "detail: {}", err.detail);

        // A zero cap (an exhausted byte budget) refuses a non-empty file.
        let err = load_file(&policy, &url, 0).unwrap_err();
        assert_eq!(err.kind, NetErrorKind::Blocked, "detail: {}", err.detail);

        std::fs::remove_dir_all(&root).ok();
    }

    /// The TOCTOU the identity check exists for: the path is vetted by a second
    /// walk (`canonicalize`), so swapping a directory component between the
    /// `open` and that walk would otherwise clear a path that no longer names
    /// the inode we hold. Staged directly rather than raced: the swap is
    /// performed while the "open" handle is already held, which is exactly the
    /// state the check has to catch.
    #[cfg(unix)]
    #[test]
    fn a_component_swapped_after_open_is_refused() {
        let root = unique_dir("toctou");
        let real = root.join("real");
        let decoy = root.join("decoy");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&decoy).unwrap();
        std::fs::File::create(real.join("f.txt"))
            .unwrap()
            .write_all(b"vetted")
            .unwrap();
        std::fs::File::create(decoy.join("f.txt"))
            .unwrap()
            .write_all(b"swapped")
            .unwrap();
        // `live` is a symlink *component*, not the leaf — the leaf-symlink guard
        // does not fire, so containment is the only thing standing here.
        let live = root.join("live");
        std::os::unix::fs::symlink(&real, &live).unwrap();

        let handle = std::fs::File::open(live.join("f.txt")).unwrap();
        // The attacker's move, between our `open` and our `canonicalize`.
        std::fs::remove_file(&live).unwrap();
        std::os::unix::fs::symlink(&decoy, &live).unwrap();

        let canonical = std::fs::canonicalize(live.join("f.txt")).unwrap();
        assert!(
            !same_object(&handle, &canonical).unwrap(),
            "the canonical path now names the decoy, not the open handle"
        );
        // Sanity: unswapped, the same comparison holds.
        let fresh = std::fs::File::open(&canonical).unwrap();
        assert!(same_object(&fresh, &canonical).unwrap());

        std::fs::remove_dir_all(&root).ok();
    }
}
