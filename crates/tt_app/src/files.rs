//! Showing files and folders in the system's file manager: Explorer,
//! Finder, or the desktop's.

use std::path::{Path, PathBuf};

/// The path as Explorer reads it: absolute, backslashes, no `\\?\` prefix.
fn for_explorer(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().map(|d| d.join(path)).unwrap_or_else(|_| path.to_path_buf()) };
    let s = abs.to_string_lossy().replace('/', "\\");
    PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
}

/// The command line Explorer gets to show `path` selected in its folder.
///
/// Explorer parses its own command line, and `/select,` must be followed by
/// the path in quotes: `/select,"C:\a b\c.mov"`. Given as one argument,
/// Rust quotes the whole of it when the path has a space
/// (`"/select,C:\a b\c.mov"`), which Explorer doesn't understand: it opened
/// the Documents folder instead, with nothing selected.
fn select_line(path: &Path) -> String {
    format!("/select,\"{}\"", for_explorer(path).display())
}

/// Show `path` in the file manager: its folder, with it selected where the
/// file manager can (Explorer, Finder).
pub fn reveal(path: &Path) {
    let spawned = if cfg!(windows) {
        explorer(&select_line(path))
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg("-R").arg(path).spawn().map(drop)
    } else {
        std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn().map(drop)
    };
    if let Err(e) = spawned {
        tracing::warn!("could not show {}: {e}", path.display());
    }
}

/// Open the folder `dir` in the file manager.
pub fn open_folder(dir: &Path) {
    let spawned = if cfg!(windows) {
        explorer(&format!("\"{}\"", for_explorer(dir).display()))
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(dir).spawn().map(drop)
    } else {
        std::process::Command::new("xdg-open").arg(dir).spawn().map(drop)
    };
    if let Err(e) = spawned {
        tracing::warn!("could not open {}: {e}", dir.display());
    }
}

/// Windows' own Explorer, with `line` as its command line exactly.
fn explorer(line: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let exe = root.join("explorer.exe");
        let program = if exe.is_file() { exe } else { PathBuf::from("explorer") };
        std::process::Command::new(program).raw_arg(line).spawn().map(drop)
    }
    #[cfg(not(windows))]
    {
        let _ = line;
        Err(std::io::Error::other("Explorer is Windows'"))
    }
}

/// (Windows paths: elsewhere `C:\…` isn't absolute.)
#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn explorer_gets_the_path_in_quotes_after_select() {
        let p = Path::new(r"C:\Users\Some One\Videos\clip - stabilized (Tracker 1).mov");
        assert_eq!(select_line(p), r#"/select,"C:\Users\Some One\Videos\clip - stabilized (Tracker 1).mov""#);
        assert_eq!(select_line(Path::new(r"\\?\C:\a b\c.mov")), r#"/select,"C:\a b\c.mov""#, "no verbatim prefix");
        assert_eq!(select_line(Path::new("C:/a b/c.mov")), r#"/select,"C:\a b\c.mov""#, "backslashes");
    }
}
