//! Media pipeline (DESIGN §13): container index → ffmpeg decode streams →
//! background decode service → NV12 frame cache. Renditions (proxies) next.

pub mod cache;
pub mod ffmpeg;
pub mod index;
pub mod player;
pub mod probe;
pub mod proxy;

pub use cache::{FrameCache, FrameData};
pub use probe::{ColorInfo, Matrix, probe_color};
pub use ffmpeg::{DecodeOptions, FrameStream};
pub use index::{Frame, VideoIndex};
pub use player::{Player, PlayerStats, Want};
