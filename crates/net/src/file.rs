//! `file://` loading (design doc §8): opt-in, jailed, regular files only.
//!
//! Disabled unless [`ResourcePolicy::allow_file`]. When a `file_root` jail is
//! set, the canonicalized target must stay within it — which rejects both
//! `..` traversal and symlinks that point outside the jail, since
//! canonicalization resolves them before the containment check.

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

/// Loads a `file://` URL under `policy`.
pub fn load_file(policy: &ResourcePolicy, url: &Url) -> NetResult<FileBody> {
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
    let mut file = std::fs::File::open(&path)
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
    }

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| NetError::new(NetErrorKind::File, e.to_string()))?;
    Ok(FileBody {
        bytes,
        content_type: guess_content_type(&canonical),
    })
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
        let body = load_file(&jailed_policy(&root), &url).unwrap();
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
        let err = load_file(&jailed_policy(&inner), &url).unwrap_err();
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
        let err = load_file(&ResourcePolicy::default(), &url).unwrap_err();
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
        let err = load_file(&policy, &url).unwrap_err();
        assert_eq!(err.kind, NetErrorKind::File, "detail: {}", err.detail);
        std::fs::remove_dir_all(&root).ok();
    }
}
