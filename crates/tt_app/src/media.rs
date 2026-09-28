//! The opened video: index, decode services and colour info, as world state.
//!
//! Opening goes through intents: the Open action (Ctrl+O) shows a dialog, and
//! drag-and-drop or the command line set [`OpenRequest`]. The transport takes
//! its frame grid from the video. Long-GOP sources get a scrub proxy built in
//! the background (tt_media::proxy); the viewport decides per frame which
//! source to show and records it in [`ActiveSource`], and only that source's
//! decode service is kept busy.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;

use bevy_ecs::prelude::*;
use tt_core::input::{Action, PendingActions};
use tt_core::transport::Transport;
use tt_core::{AppBuilder, Class, Module, Set};
use tt_media::{ColorInfo, DecodeOptions, Player, VideoIndex, Want, probe_color, proxy};

/// Frame cache budgets (DESIGN §13): ~660 frames of 1080p NV12, and the same
/// again of 720p proxy frames.
const CACHE_BYTES: usize = 2 << 30;
const PROXY_CACHE_BYTES: usize = 1 << 30;
/// Build a proxy when keyframes are further apart than this (frames).
const PROXY_WHEN_GOP_OVER: usize = 30;

pub struct Source {
    pub index: Arc<VideoIndex>,
    pub player: Player,
}

pub enum ProxyState {
    NotNeeded,
    Building { progress: Arc<AtomicU32>, total: u32, job: JoinHandle<anyhow::Result<VideoIndex>> },
    Ready(Source),
    Failed(String),
}

#[derive(Resource)]
pub struct Media {
    pub name: String,
    pub original: Source,
    pub proxy: ProxyState,
    pub color: ColorInfo,
    /// Increments per opened file, so GPU uploads never confuse two videos.
    pub generation: u64,
}

impl Media {
    pub fn index(&self) -> &VideoIndex {
        &self.original.index
    }

    /// Presented frame shown at a grid frame (the same index in every rendition).
    pub fn presented(&self, grid_frame: i64) -> usize {
        self.original.index.presented_at(grid_frame)
    }

    pub fn proxy(&self) -> Option<&Source> {
        match &self.proxy {
            ProxyState::Ready(s) => Some(s),
            _ => None,
        }
    }

    pub fn source(&self, which: Which) -> Option<&Source> {
        match which {
            Which::Original => Some(&self.original),
            Which::Proxy => self.proxy(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Which {
    #[default]
    Original,
    Proxy,
}

/// Which rendition the viewport shows (set by the viewport; read by the decode requests).
#[derive(Resource, Default)]
pub struct ActiveSource(pub Which);

#[derive(Resource, Default)]
pub struct OpenRequest(pub Option<PathBuf>);

/// One line of feedback in the top bar (errors stay until replaced).
#[derive(Resource, Default)]
pub struct StatusLine(pub Option<(String, bool)>);

#[derive(Resource, Default)]
struct Generation(u64);

fn open_dialog(mut actions: ResMut<PendingActions>, mut request: ResMut<OpenRequest>) {
    if actions.take(|a| a == Action::OpenFile).is_empty() {
        return;
    }
    // A native dialog blocks this frame until it closes, as expected.
    if let Some(path) =
        rfd::FileDialog::new().add_filter("Video", &["mp4", "mov", "m4v"]).add_filter("All files", &["*"]).pick_file()
    {
        request.0 = Some(path);
    }
}

fn open_requested(world: &mut World) {
    let Some(path) = world.resource_mut::<OpenRequest>().0.take() else { return };
    let t = std::time::Instant::now();
    let index = match VideoIndex::open(&path) {
        Ok(index) => Arc::new(index),
        Err(e) => {
            tracing::warn!("could not open {}: {e:#}", path.display());
            world.resource_mut::<StatusLine>().0 = Some((format!("Could not open {}: {e}", path.display()), true));
            return;
        }
    };
    let color = probe_color(&path, index.height);
    let generation = {
        let mut g = world.resource_mut::<Generation>();
        g.0 += 1;
        g.0
    };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut gops = index.gop_lengths();
    gops.sort_unstable();
    let median_gop = gops.get(gops.len() / 2).copied().unwrap_or(1);
    tracing::info!(
        "opened {name}: {}x{} {} · {} frames @ {}/{} · GOP {median_gop} · indexed in {:?}",
        index.width,
        index.height,
        index.codec,
        index.frame_count(),
        index.fps.num,
        index.fps.den,
        t.elapsed()
    );
    {
        let mut tr = world.resource_mut::<Transport>();
        tr.fps = index.fps;
        tr.frame_count = index.frame_count();
        tr.playhead = 0.0;
        tr.playing = false;
    }

    let proxy = if median_gop <= PROXY_WHEN_GOP_OVER {
        ProxyState::NotNeeded
    } else {
        start_proxy(&index)
    };
    let original = Source { player: Player::new(index.clone(), DecodeOptions::default(), CACHE_BYTES), index };
    world.insert_resource(Media { name, original, proxy, color, generation });
    world.insert_resource(ActiveSource(Which::Original));
    world.resource_mut::<StatusLine>().0 = None;
}

fn start_proxy(index: &Arc<VideoIndex>) -> ProxyState {
    let path = match proxy::proxy_path(&index.path) {
        Ok(p) => p,
        Err(e) => return ProxyState::Failed(format!("{e:#}")),
    };
    if let Some(existing) = proxy::open_matching(index, &path) {
        tracing::info!("using existing proxy {}", path.display());
        return ProxyState::Ready(proxy_source(existing));
    }
    let progress = Arc::new(AtomicU32::new(0));
    let total = index.frames.len() as u32;
    let (source, sink) = (index.clone(), progress.clone());
    let job = std::thread::Builder::new()
        .name("proxy".into())
        .spawn(move || {
            let t = std::time::Instant::now();
            let r = proxy::build(&source, &path, &DecodeOptions::default(), sink);
            if r.is_ok() {
                tracing::info!("proxy built in {:.1} s: {}", t.elapsed().as_secs_f64(), path.display());
            }
            r
        })
        .expect("spawn proxy thread");
    ProxyState::Building { progress, total, job }
}

fn proxy_source(index: VideoIndex) -> Source {
    let index = Arc::new(index);
    Source { player: Player::new(index.clone(), DecodeOptions::default(), PROXY_CACHE_BYTES), index }
}

fn poll_proxy(media: Option<ResMut<Media>>) {
    let Some(mut media) = media else { return };
    if !matches!(&media.proxy, ProxyState::Building { job, .. } if job.is_finished()) {
        return;
    }
    let ProxyState::Building { job, .. } = std::mem::replace(&mut media.proxy, ProxyState::NotNeeded) else { unreachable!() };
    media.proxy = match job.join() {
        Ok(Ok(index)) => ProxyState::Ready(proxy_source(index)),
        Ok(Err(e)) => {
            tracing::warn!("proxy build failed: {e:#}");
            ProxyState::Failed(format!("{e:#}"))
        }
        Err(_) => ProxyState::Failed("proxy thread panicked".into()),
    };
}

fn request_frames(media: Option<ResMut<Media>>, active: Res<ActiveSource>, t: Res<Transport>) {
    let Some(mut media) = media else { return };
    let want = Want { frame: media.presented(t.frame()), playing: t.playing, rate: t.rate };
    match (active.0, &mut media.proxy) {
        (Which::Proxy, ProxyState::Ready(proxy)) => proxy.player.want(want),
        _ => media.original.player.want(want),
    }
}

/// Short status of the proxy for the top bar.
pub fn proxy_status(media: &Media) -> Option<String> {
    match &media.proxy {
        ProxyState::NotNeeded => None,
        ProxyState::Building { progress, total, .. } => {
            Some(format!("building scrub proxy {:.0}%", 100.0 * progress.load(Ordering::Relaxed) as f64 / (*total).max(1) as f64))
        }
        ProxyState::Ready(p) => Some(format!("proxy {}p", p.index.height)),
        ProxyState::Failed(e) => Some(format!("proxy failed: {e}")),
    }
}

pub struct MediaModule;

impl Module for MediaModule {
    fn build(&self, app: &mut AppBuilder) {
        app.declare::<Media>(Class::Document)
            .declare::<ActiveSource>(Class::Session)
            .init_resource::<OpenRequest>()
            .init_resource::<StatusLine>()
            .init_resource::<Generation>()
            .init_resource::<ActiveSource>()
            .add_systems((open_dialog, open_requested).chain().in_set(Set::Intents))
            .add_systems(poll_proxy.in_set(Set::Jobs))
            .add_systems(request_frames.in_set(Set::Media));
    }
}
