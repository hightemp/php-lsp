use std::path::{Path, PathBuf};

/// Convert a file:// URI to a filesystem path.
///
/// This preserves the historical lightweight conversion behavior. PHA-002 will
/// replace this implementation with standards-compliant percent-encoding and
/// platform-specific path handling in this single module.
pub(crate) fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(PathBuf::from)
}

/// Convert a file path to a file:// URI.
///
/// This preserves the historical lightweight conversion behavior. New call
/// sites should go through this helper so PHA-002 can upgrade URI handling
/// without hunting duplicate `file://` formatting across the server.
pub(crate) fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uri_to_path_and_back() {
        let path = PathBuf::from("/home/user/project/src/Foo.php");
        let uri = path_to_uri(&path);
        assert_eq!(uri, "file:///home/user/project/src/Foo.php");

        let back = uri_to_path(&uri).unwrap();
        assert_eq!(back, path);
    }
}
