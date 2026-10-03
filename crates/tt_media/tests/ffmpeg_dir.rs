//! tt_media::ffmpeg::tool: the folder setup installed FFmpeg into comes
//! first (after the environment variable). Its own test binary: the folder
//! is global to the process.

use tt_media::ffmpeg::{set_dir, tool};

#[test]
fn the_chosen_folder_comes_first() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ffmpeg_dir_test");
    std::fs::create_dir_all(&dir).expect("a folder");
    let exe = dir.join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(&exe, b"not really ffmpeg").expect("a file");
    // (An environment variable nobody sets.)
    let var = "TT_TEST_FFMPEG_NOBODY_SETS_THIS";
    set_dir(Some(dir.clone()));
    assert_eq!(tool("ffmpeg", var), exe, "the chosen folder's");
    set_dir(Some(dir.join("empty")));
    assert_ne!(tool("ffmpeg", var), exe, "a folder without it: looked for elsewhere");
    set_dir(None);
    assert_ne!(tool("ffmpeg", var), exe);
}
