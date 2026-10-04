//! Packs the CoTracker worker's Python code into the program: the worker and
//! the v1 engine it drives (`editor/`), the vendored CoTracker3 package
//! (`cotracker/`, its `.py` files) and the license they come under. The
//! doctor writes them out on a computer without the repository
//! (src/cotracker.rs); `cotracker_code.rs` in OUT_DIR lists them.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn python_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n != "__pycache__") {
                println!("cargo:rerun-if-changed={}", p.display());
                python_files(&p, out);
            }
        } else if p.extension().is_some_and(|e| e == "py") {
            out.push(p);
        }
    }
}

fn main() {
    // (Not canonicalized: on Windows that gives verbatim paths, which include_bytes! may not take.)
    let repo = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it")).join("..").join("..");
    let mut files: Vec<PathBuf> = ["editor/cotracker_worker.py", "editor/engine.py", "editor/frames.py", "LICENSE.md"].iter().map(|f| repo.join(f)).collect();
    println!("cargo:rerun-if-changed={}", repo.join("cotracker").display());
    python_files(&repo.join("cotracker"), &mut files);
    let mut code = String::from("/// The CoTracker worker's code: (path under the code folder, its bytes).\npub const FILES: &[(&str, &[u8])] = &[\n");
    for f in files.iter().filter(|f| f.is_file()) {
        println!("cargo:rerun-if-changed={}", f.display());
        let rel = f.strip_prefix(&repo).expect("in the repository").to_string_lossy().replace('\\', "/");
        let _ = writeln!(code, "    ({rel:?}, include_bytes!({:?})),", f.display().to_string());
    }
    code.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets it")).join("cotracker_code.rs");
    std::fs::write(out, code).expect("write the list");
}
