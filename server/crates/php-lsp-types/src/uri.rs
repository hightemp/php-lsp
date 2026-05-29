use std::path::{Path, PathBuf};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUriError {
    path: PathBuf,
    message: String,
}

impl FileUriError {
    fn new(path: &Path, message: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            message: message.into(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Display for FileUriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "failed to convert {} to file URI: {}",
            self.path.display(),
            self.message
        )
    }
}

impl std::error::Error for FileUriError {}

pub fn path_to_uri(path: &Path) -> Result<String, FileUriError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| FileUriError::new(path, err.to_string()))?
            .join(path)
    };

    Url::from_file_path(&absolute)
        .map(|url| url.to_string())
        .map_err(|_| FileUriError::new(&absolute, "path is not representable as a file URI"))
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_with_spaces_hash_percent_and_non_ascii_round_trips() {
        let path = PathBuf::from("/tmp/php lsp/Foo #1%/Привет.php");
        let uri = path_to_uri(&path).unwrap();

        assert!(uri.starts_with("file:///tmp/php%20lsp/"));
        assert!(uri.contains("Foo%20%231%25"));
        assert!(uri.contains("%D0%9F%D1%80%D0%B8%D0%B2%D0%B5%D1%82.php"));
        assert_eq!(uri_to_path(&uri), Some(path));
    }

    #[test]
    fn non_file_uri_does_not_decode_to_path() {
        assert_eq!(uri_to_path("phpstub://Core/standard.php"), None);
    }

    #[test]
    fn relative_paths_are_absolutized_before_encoding() {
        let path = PathBuf::from("relative #1%.php");
        let uri = path_to_uri(&path).unwrap();
        assert!(uri.starts_with("file:///"));
        assert!(uri.ends_with("relative%20%231%25.php"));
        let decoded = uri_to_path(&uri).unwrap();
        assert_eq!(decoded.file_name(), path.file_name());
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_path_round_trips() {
        let path = PathBuf::from(r"C:\Users\php lsp\Foo #1%.php");
        let uri = path_to_uri(&path).unwrap();

        assert!(uri.starts_with("file:///C:/Users/php%20lsp/"));
        assert!(uri.ends_with("Foo%20%231%25.php"));
        assert_eq!(uri_to_path(&uri), Some(path));
    }
}
