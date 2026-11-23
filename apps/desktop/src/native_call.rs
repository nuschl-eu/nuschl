// SPDX-License-Identifier: EUPL-1.2
//! Desktop calls. Capture and decoding use confined video workers.

mod audio;
mod audio_control;
mod gallery;
mod publisher;
mod stage;
mod stats;
mod viewer;

use crate::{
    dialogs::{Tone, icon_button, styled_button},
    images::VideoImage,
    layout,
    menus::{MenuAnchor, MenuTarget},
    services::ServiceEvent,
    state::Desktop,
    theme::{
        ACCENT, BLACK, BORDER, DANGER, DANGER_TEXT, ELEVATED, MAIN, SELECTED, SIDEBAR, TEXT,
        TEXT_MUTED, WHITE, color, tint,
    },
};
use futures_util::{FutureExt as _, StreamExt as _, future::BoxFuture};
use gpui_kit::{prelude::*, *};
use nuschl_client::native::sfu::{
    Connect, Connection, Media, MediaEvent, MediaOptions, Purpose, Quality, VideoKind,
};
use nuschl_core::{model::TextTarget, network::PeerPolicy};
#[cfg(target_os = "linux")]
use nuschl_ipc::video::portal::Grant;
use nuschl_ipc::video::{CaptureFailure, DecodedFrame, Source};
use nuschl_protocol::sfu::Codec;
use nuschl_session::{config::ClientSettings, identity::CryptoHandle};
use std::{
    cell::Cell,
    collections::BTreeSet,
    future::pending,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{Notify, mpsc, oneshot, watch};

pub(crate) use gallery::Feed;
use gallery::{Selection, Sources, Stream, TILES};

const SFU_PORT: u16 = 3479;
static WORKERS: LazyLock<Option<(PathBuf, PathBuf)>> = LazyLock::new(|| {
    let rtc = nuschl_sandbox::sibling("nuschl-rtc-worker").ok()?;
    let video = nuschl_sandbox::sibling("nuschl-video-worker").ok()?;
    (rtc.is_file() && video.is_file()).then_some((rtc, video))
});

pub struct Active {
    owner: bool,
    inviter: Option<(u32, u64)>,
    policy: PeerPolicy,
    room: String,
    server: String,
    conversation: TextTarget,
    voice_generation: u64,
    surface: Entity<Surface>,
    quality: watch::Sender<Selection>,
    stats_open: bool,
    local_sources: [bool; 2],
    can_publish: bool,
    source_requests: [mpsc::Sender<SourceRequest>; 2],
    source_pending: Option<Source>,
    reopening_screen: bool,
    updates: Arc<Latest>,
    stop: Option<oneshot::Sender<()>>,
    minimized: bool,
    stage: Option<stage::Controls>,
    monitor: bool,
    audio_requests: mpsc::Sender<bool>,
    sharing_audio: bool,
}

pub enum Finish {
    Left,
    RoomEnded,
    MediaEnded,
}

#[derive(Clone, Copy)]
struct SourceRequest {
    source: Source,
    enable: bool,
}

pub enum SourceChange {
    CanPublish(bool),
    MediaClosed,
    Active { source: Source, monitor: bool },
    ReopeningScreen,
    Stopped(Source),
    Cancelled(Source),
    Failed(Source, String),
    Audio(audio::Change),
}

pub struct Start {
    owner: bool,
    crypto: CryptoHandle,
    generation: u64,
    url: String,
    host: String,
    policy: PeerPolicy,
    source: Option<Source>,
    source_requests: [mpsc::Receiver<SourceRequest>; 2],
    audio_requests: mpsc::Receiver<bool>,
    volumes: watch::Receiver<audio::Volumes>,
    updates: Arc<Latest>,
    quality: watch::Receiver<Selection>,
    stop: oneshot::Receiver<()>,
}

struct VideoStats {
    stream: Stream,
    sample: stats::Snapshot,
    encoder: Option<stats::Encoder>,
}

struct Display<'a> {
    source_changes: SourceChanges<'a>,
    updates: Arc<Latest>,
    quality: watch::Receiver<Selection>,
}

impl Display<'_> {
    fn report(&self, stream: Stream, sample: stats::Snapshot, encoder: Option<stats::Encoder>) {
        self.updates.stats(Some(Arc::new(VideoStats {
            stream,
            sample,
            encoder,
        })));
    }
}

struct Update {
    status: String,
    tiles: [Tile; TILES],
    stats: Option<Arc<VideoStats>>,
    sources: Sources,
    visible: bool,
    /// Publishers whose screen share carries audio.
    audio: BTreeSet<u64>,
    /// The publisher id of the call's host, who invited this viewer.
    host: Option<u64>,
}

#[derive(Default)]
struct Tile {
    stream: Option<Stream>,
    frame: Option<DecodedFrame>,
    failed: bool,
}

impl Active {
    fn featured(&self, sources: &Sources, selection: Selection) -> Option<Feed> {
        sources
            .featured(selection.view)
            .map(|stream| stream.feed)
            .or_else(|| {
                self.source_pending
                    .map(|source| Feed::local(video_kind(source)))
            })
    }
}

struct Latest {
    update: Mutex<Update>,
    ready: Notify,
}

impl Latest {
    fn sources(&self) -> Sources {
        self.update
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sources
    }

    fn show_video(&self, visible: bool) {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if std::mem::replace(&mut update.visible, visible) != visible {
            if !visible {
                for tile in &mut update.tiles {
                    tile.frame = None;
                }
            }
            drop(update);
            self.ready.notify_one();
        }
    }

    fn shared_audio(&self, publishers: BTreeSet<u64>) {
        self.update
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .audio = publishers;
        self.ready.notify_one();
    }

    fn host(&self, host: Option<u64>) {
        self.update
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .host = host;
        self.ready.notify_one();
    }

    fn status(&self, status: impl Into<String>) {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        update.status = status.into();
        drop(update);
        self.ready.notify_one();
    }

    fn stats(&self, stats: Option<Arc<VideoStats>>) {
        self.update
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stats = stats;
        self.ready.notify_one();
    }

    fn layout(&self, sources: Sources, tiles: [Option<Stream>; TILES]) -> bool {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let visible = update.visible;
        if update.sources == sources && update.tiles.each_ref().map(|tile| tile.stream) == tiles {
            return visible;
        }
        update.sources = sources;
        update.tiles = tiles.map(|stream| {
            update
                .tiles
                .iter_mut()
                .find(|tile| tile.stream.is_some() && tile.stream == stream)
                .map(std::mem::take)
                .unwrap_or(Tile {
                    stream,
                    frame: None,
                    failed: false,
                })
        });
        drop(update);
        self.ready.notify_one();
        visible
    }

    fn frame(&self, stream: Stream, frame: DecodedFrame) {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if update.visible
            && let Some(tile) = update
                .tiles
                .iter_mut()
                .find(|tile| tile.stream == Some(stream))
        {
            tile.frame = Some(frame);
            drop(update);
            self.ready.notify_one();
        }
    }

    fn video_unavailable(&self, stream: Stream) {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(tile) = update
            .tiles
            .iter_mut()
            .find(|tile| tile.stream == Some(stream))
        {
            tile.failed = true;
            tile.frame = None;
            drop(update);
            self.ready.notify_one();
        }
    }
}

type Picture = (Stream, Option<VideoImage>, bool);

struct Surface {
    status: String,
    pictures: [Option<Picture>; TILES],
    stats: Option<Arc<VideoStats>>,
    sources: Sources,
    quality: watch::Sender<Selection>,
    audio: audio_control::Controls,
    _updates: Task<()>,
}

impl EventEmitter<audio_control::Remember> for Surface {}

impl Surface {
    fn new(
        updates: Arc<Latest>,
        status: &str,
        quality: watch::Sender<Selection>,
        audio: audio_control::Controls,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.spawn(async move |this, cx| {
            loop {
                updates.ready.notified().await;
                let (status, tiles, stats, sources, visible, audio, host) = {
                    let mut update = updates
                        .update
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    (
                        update.status.clone(),
                        update
                            .tiles
                            .each_mut()
                            .map(|tile| (tile.stream, tile.frame.take(), tile.failed)),
                        update.stats.clone(),
                        update.sources,
                        update.visible,
                        update.audio.clone(),
                        update.host,
                    )
                };
                if this
                    .update(cx, |this, cx| {
                        this.status = status;
                        this.stats = stats;
                        this.sources = sources;
                        this.audio.audio = audio;
                        this.audio.set_host(host);
                        this.pictures = tiles.map(|(stream, frame, failed)| {
                            let stream = stream?;
                            let image = (visible && !failed)
                                .then(|| {
                                    frame.and_then(render_frame).or_else(|| {
                                        this.pictures.iter_mut().find_map(|picture| {
                                            picture
                                                .as_mut()
                                                .filter(|(previous, _, _)| *previous == stream)
                                                .and_then(|(_, image, _)| image.take())
                                        })
                                    })
                                })
                                .flatten();
                            Some((stream, image, failed))
                        });
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            status: status.into(),
            pictures: std::array::from_fn(|_| None),
            stats: None,
            sources: Sources::default(),
            quality,
            audio,
            _updates: observe,
        }
    }
}

impl Surface {
    fn tile(
        &self,
        stream: Stream,
        image: Option<Arc<RenderImage>>,
        failed: bool,
        gallery: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let label = stream.feed.label();
        let shared_audio = stream
            .feed
            .publisher
            .filter(|_| stream.feed.kind == VideoKind::Screen);
        div()
            .id(format!("call-picture:{}", label))
            .role(Role::Image)
            .aria_label(label.clone())
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .when(!window.is_fullscreen() || gallery, |picture| {
                picture.rounded(px(8.))
            })
            .bg(color(BLACK))
            .children(image.is_none().then(|| {
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .justify_center()
                    .items_center()
                    .text_sm()
                    .text_color(color(WHITE))
                    .child(gpui_kit::text!(if failed {
                        "Video source unavailable"
                    } else if stream.preview {
                        "Waiting for video"
                    } else {
                        "Local preview unavailable"
                    }))
            }))
            .when_some(image, |tile, image| {
                let image_size = image.size(0);
                let preferences = self.quality.clone();
                let sources = self.sources;
                tile.child(
                    img(image)
                        .absolute()
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                )
                .when(stream.feed.publisher.is_some(), |tile| {
                    tile.child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, (), window, _| {
                                let fitted = ObjectFit::Contain.get_bounds(bounds, image_size);
                                let height = fitted.size.width.min(fitted.size.height);
                                let pixels = (f32::from(height) * window.scale_factor()) as u32;
                                preferences.send_if_modified(|selection| {
                                    selection.change(stream.feed, &sources, |preference| {
                                        preference.pixels = pixels
                                    })
                                });
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                })
            })
            .when(gallery, |tile| {
                tile.child(
                    div()
                        .absolute()
                        .bottom_2()
                        .left_2()
                        .max_w_full()
                        .px_1()
                        .text_xs()
                        .bg(tint(MAIN, 0xeb))
                        .child(gpui_kit::text!(label)),
                )
            })
            .when_some(shared_audio, |tile, publisher| {
                tile.child(audio_control::control(&self.audio, publisher, window, cx))
            })
            .into_any_element()
    }
}

impl Render for Surface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::images::collect(window);
        let screens = self.pictures.iter().flatten().filter_map(|(stream, _, _)| {
            stream
                .feed
                .publisher
                .filter(|_| stream.feed.kind == VideoKind::Screen)
        });
        self.audio
            .prepare(screens.collect::<Vec<_>>().into_iter(), cx);
        let mut pictures = self.pictures.each_mut().map(|picture| {
            picture.as_mut().map(|(stream, image, failed)| {
                (*stream, image.as_mut().map(VideoImage::for_render), *failed)
            })
        });
        let count = pictures.iter().flatten().count();
        let gallery = count > 1;
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(color(BLACK))
            .when(gallery, |stage| stage.gap_1())
            .when(count == 0, |stage| {
                stage.items_center().justify_center().child(
                    div()
                        .id("native-call-status")
                        .text_sm()
                        .text_color(color(WHITE))
                        .child(gpui_kit::text!(self.status.clone())),
                )
            })
            .children(pictures[..count].chunks_mut(2).map(|row| {
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .when(gallery, |row| row.gap_1())
                    .children(row.iter_mut().flatten().map(|(stream, image, failed)| {
                        self.tile(*stream, image.take(), *failed, gallery, window, cx)
                    }))
            }))
    }
}

fn render_frame(frame: DecodedFrame) -> Option<VideoImage> {
    let (width, height) = (u32::from(frame.width), u32::from(frame.height));
    let image = image::RgbaImage::from_raw(width, height, frame.bgra)?;
    Some(VideoImage::new(RenderImage::new(vec![image::Frame::new(
        image,
    )])))
}

impl Desktop {
    pub fn native_available(&self, host: &str) -> bool {
        WORKERS.is_some()
            && self.call_host().eq_ignore_ascii_case(host)
            && !matches!(host.split(':').next(), Some("localhost" | "127.0.0.1"))
    }

    pub fn native_call_in(&self, server: &str, target: &TextTarget) -> bool {
        self.native_call
            .as_ref()
            .is_some_and(|call| call.server == server && call.conversation == *target)
    }

    pub fn open_native_call(
        &mut self,
        server: String,
        conversation: TextTarget,
        link: &nuschl_stream::Link,
        source: Option<Source>,
        inviter: Option<(u32, u64)>,
        cx: &mut Context<Self>,
    ) {
        if self.native_call.is_some() || !self.native_available(link.host()) {
            return;
        }
        let Some((server_index, channel_index)) = self.selected_indices() else {
            return;
        };
        let selected = &self.client.servers[server_index];
        if selected.id != server || selected.channels[channel_index].target != conversation {
            return;
        }
        let Some(generation) = self.native_call_generation.checked_add(1) else {
            return;
        };
        let voice_generation = selected.generation;
        // The inviter is the call's host: their shared audio keeps its volume.
        let remembered = inviter
            .and_then(|(session, instance)| {
                selected
                    .users
                    .get(&session)
                    .filter(|user| user.instance == instance)?
                    .certificate_hash
                    .clone()
            })
            .map(|hash| {
                let choice = self.profile.shared_audio().get(&hash).copied();
                (hash, choice)
            });
        let owner = link.publisher().is_some();
        let room = link.url();
        if owner == inviter.is_some()
            || inviter.is_some_and(|inviter| {
                !self
                    .client
                    .calls_in(server_index, &conversation)
                    .iter()
                    .any(|call| (call.sender, call.sender_instance) == inviter && call.link == room)
            })
        {
            return;
        }
        let url = if owner {
            let Some(url) = link.creator_url() else {
                return;
            };
            url
        } else {
            room.clone()
        };
        let status = "Joining video call";
        let updates = Arc::new(Latest {
            update: Mutex::new(Update {
                status: status.into(),
                tiles: std::array::from_fn(|_| Tile::default()),
                stats: None,
                sources: Sources::default(),
                visible: true,
                audio: BTreeSet::new(),
                host: None,
            }),
            ready: Notify::new(),
        });
        let (stop, stopped) = oneshot::channel();
        // A camera command leaves the screen's Stop sharing action available.
        let [
            (request_camera, camera_requests),
            (request_screen, screen_requests),
        ] = std::array::from_fn(|_| mpsc::channel(1));
        let (selection, quality) = watch::channel(Selection::default());
        let (request_audio, audio_requests) = mpsc::channel(4);
        let (set_volumes, volumes) = watch::channel(audio::Volumes::new());
        let controls = audio_control::Controls::new(set_volumes, remembered);
        let surface =
            cx.new(|cx| Surface::new(updates.clone(), status, selection.clone(), controls, cx));
        cx.subscribe(
            &surface,
            |this, _, audio_control::Remember(hash, choice), _| {
                if let Err(error) = this.profile.remember_shared_audio(hash, *choice) {
                    tracing::warn!(%error, "could not keep a shared audio volume");
                }
            },
        )
        .detach();
        let policy = PeerPolicy {
            relay_only: self.settings.relay_only,
        };
        self.native_call_generation = generation;
        self.native_call = Some(Active {
            owner,
            inviter,
            policy,
            room,
            server,
            conversation,
            voice_generation,
            surface,
            quality: selection,
            stats_open: false,
            local_sources: [false; 2],
            can_publish: false,
            source_requests: [request_camera, request_screen],
            source_pending: source,
            reopening_screen: false,
            updates: updates.clone(),
            stop: Some(stop),
            minimized: false,
            stage: None,
            monitor: false,
            audio_requests: request_audio,
            sharing_audio: false,
        });
        self.services.native_call(Start {
            owner,
            crypto: self.driver.call_crypto(),
            generation,
            url,
            host: link.host().to_owned(),
            policy,
            source,
            source_requests: [camera_requests, screen_requests],
            audio_requests,
            volumes,
            updates,
            quality,
            stop: stopped,
        });
    }

    pub fn start_native_call(
        &mut self,
        index: usize,
        target: TextTarget,
        source: Option<Source>,
        cx: &mut Context<Self>,
    ) {
        if self.native_call.is_some() || !self.native_available(self.call_host()) {
            return;
        }
        match self.create_call(index, target.clone()) {
            Ok(link) => {
                let server = self.client.servers[index].id.clone();
                self.open_native_call(server, target, &link, source, None, cx);
                self.announce_call(link);
            }
            Err(error) => self.client.servers[index].set_notice(error),
        }
    }

    fn stop_native(&mut self) {
        if let Some(call) = &mut self.native_call
            && let Some(stop) = call.stop.take()
        {
            let _ = stop.send(());
        }
    }

    pub fn stop_native_owner(&mut self) {
        if self.native_call.as_ref().is_some_and(|call| call.owner) {
            self.stop_native();
        }
    }

    pub fn sync_native_call(&mut self) {
        let Some(call) = &self.native_call else {
            return;
        };
        let current = self
            .client
            .servers
            .iter()
            .enumerate()
            .any(|(index, server)| {
                server.status == nuschl_client::server::Status::Connected
                    && server.id == call.server
                    && server.generation == call.voice_generation
                    && if call.owner {
                        self.client
                            .own_call()
                            .is_some_and(|own| own.link == call.room)
                    } else {
                        self.client
                            .calls_in(index, &call.conversation)
                            .iter()
                            .any(|announced| {
                                Some((announced.sender, announced.sender_instance)) == call.inviter
                                    && announced.link == call.room
                            })
                    }
            });
        if !current || (self.settings.relay_only && !call.policy.relay_only) {
            self.stop_native();
            return;
        }
        let shown = !call.minimized
            && (call.stage.is_some()
                || self.selected_indices().is_some_and(|(index, channel)| {
                    let server = &self.client.servers[index];
                    self.native_call_in(&server.id, &server.channels[channel].target)
                }));
        call.updates.show_video(shown);
    }

    fn set_call_expanded(&mut self, expanded: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(call) = &mut self.native_call else {
            return;
        };
        if expanded {
            call.stage = Some(stage::Controls::new(window, cx));
            stage::activity(self, cx);
            window.focus(&self.window_focus, cx);
            if !window.is_fullscreen() && self.native_call_fullscreen.is_none() {
                self.native_call_fullscreen = Some(Instant::now());
                window.toggle_fullscreen();
            }
        } else {
            call.stage = None;
            self.menu = None;
            window.focus(&self.window_focus, cx);
            self.restore_call_window(window);
        }
        cx.notify();
    }

    fn restore_call_window(&mut self, window: &Window) {
        if let Some(started) = self.native_call_fullscreen {
            if window.is_fullscreen() {
                self.native_call_fullscreen = None;
                window.toggle_fullscreen();
            } else if started.elapsed() >= Duration::from_secs(2) {
                self.native_call_fullscreen = None;
            }
        }
    }

    fn minimize_native_call(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_call_expanded(false, window, cx);
        if let Some(call) = &mut self.native_call {
            call.minimized = true;
            call.updates.show_video(false);
            cx.notify();
        }
    }

    fn restore_native_call(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.selected_indices();
        let Some(call) = &mut self.native_call else {
            return;
        };
        if let Some((index, server)) = self
            .client
            .servers
            .iter()
            .enumerate()
            .find(|(_, server)| server.id == call.server)
            && let Some((channel_index, channel)) = server
                .channels
                .iter()
                .enumerate()
                .find(|(_, channel)| channel.target == call.conversation)
        {
            call.minimized = false;
            call.updates.show_video(true);
            if selected != Some((index, channel_index)) {
                self.browse_channel(index, channel.id, window, cx);
            }
            cx.notify();
        }
    }

    fn choose_native_source(&mut self, source: Source, cx: &mut Context<Self>) {
        let Some(call) = self.native_call.as_mut().filter(|call| call.stop.is_some()) else {
            return;
        };
        let enable = !call.local_sources[publisher::index(source)];
        if enable && (!call.can_publish || call.source_pending.is_some()) {
            return;
        }
        if call.source_requests[publisher::index(source)]
            .try_send(SourceRequest { source, enable })
            .is_ok()
            && enable
        {
            call.source_pending = Some(source);
            cx.notify();
        }
    }

    pub fn native_source_changed(
        &mut self,
        generation: u64,
        change: SourceChange,
        cx: &mut Context<Self>,
    ) {
        let Some(call) = self
            .native_call
            .as_mut()
            .filter(|call| generation == self.native_call_generation && call.stop.is_some())
        else {
            return;
        };
        let source = match change {
            SourceChange::CanPublish(allowed) => {
                call.can_publish = allowed;
                cx.notify();
                return;
            }
            SourceChange::MediaClosed => {
                call.can_publish = false;
                call.sharing_audio = false;
                call.local_sources = [false; 2];
                call.source_pending = None;
                call.reopening_screen = false;
                call.monitor = false;
                cx.notify();
                return;
            }
            SourceChange::Active { source, monitor } => {
                call.local_sources[publisher::index(source)] = true;
                if source == Source::Screen {
                    call.reopening_screen = false;
                    call.monitor = monitor;
                }
                source
            }
            SourceChange::ReopeningScreen => {
                call.reopening_screen = true;
                Source::Screen
            }
            SourceChange::Stopped(source) => {
                call.local_sources[publisher::index(source)] = false;
                if source == Source::Screen {
                    call.reopening_screen = false;
                    call.monitor = false;
                }
                source
            }
            SourceChange::Cancelled(source) => source,
            SourceChange::Audio(audio::Change(active, notice)) => {
                call.sharing_audio = active;
                if let Some(notice) = notice
                    && let Some(server) = self.client.servers.iter_mut().find(|server| {
                        server.id == call.server && server.generation == call.voice_generation
                    })
                {
                    server.set_notice(notice);
                }
                cx.notify();
                return;
            }
            SourceChange::Failed(source, error) => {
                call.local_sources[publisher::index(source)] = false;
                if source == Source::Screen {
                    call.reopening_screen = false;
                    call.monitor = false;
                }
                if let Some(server) = self.client.servers.iter_mut().find(|server| {
                    server.id == call.server && server.generation == call.voice_generation
                }) {
                    server.set_notice(format!("Video source unavailable. {error}"));
                }
                source
            }
        };
        if call.source_pending == Some(source) {
            call.source_pending = None;
        }
        cx.notify();
    }

    pub fn native_finished(&mut self, generation: u64, result: Result<Finish, String>) {
        if generation != self.native_call_generation {
            return;
        }
        let Some(call) = self.native_call.take() else {
            return;
        };
        if call.owner
            && self
                .client
                .own_call()
                .is_some_and(|own| own.link == call.room)
        {
            self.end_call();
        }
        if call.stop.is_some()
            && let Some(index) = self.client.server_index(&call.server)
            && self.client.servers[index].generation == call.voice_generation
        {
            match result {
                Err(error) => {
                    self.client.servers[index].set_notice(format!("Video unavailable. {error}"))
                }
                Ok(Finish::RoomEnded) => {
                    if let Some(inviter) = call.inviter {
                        self.client.end_received_call(
                            index,
                            &call.conversation,
                            &call.room,
                            inviter,
                        );
                    }
                }
                Ok(_) => {}
            }
        }
    }
}

/// The user can focus video on demand. The ordinary view keeps the chat visible.
pub fn expanded(
    this: &mut Desktop,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) -> Option<AnyElement> {
    if !this
        .native_call
        .as_ref()
        .is_some_and(|call| call.stage.is_some())
    {
        this.restore_call_window(window);
        return None;
    }
    let menu = crate::menus::menu(this, cx);
    if menu.is_none() {
        this.menu = None;
    }
    let controls = this.native_call.as_mut()?.stage.as_mut()?;
    let toolbar = (
        controls.focus.clone(),
        controls.opacity(menu.is_some(), window, cx),
    );
    let cover = div()
        .id("expanded-video-call")
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .text_color(color(TEXT))
        .bg(color(BLACK))
        .track_focus(&this.window_focus)
        .on_mouse_move(cx.listener(|this, _, _, cx| stage::activity(this, cx)))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            stage::activity(this, cx);
            if event.keystroke.key == "escape" {
                this.set_call_expanded(false, window, cx);
                cx.stop_propagation();
            }
        }))
        .children(panel_with_toolbar(this, window, cx, Some(toolbar)))
        .children(menu);
    let cover = cover.into_any_element();
    if window.is_fullscreen() {
        window.set_client_inset(px(0.));
        Some(cover)
    } else {
        Some(crate::chrome::frame(window, cover))
    }
}

pub fn panel(this: &Desktop, window: &mut Window, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    panel_with_toolbar(this, window, cx, None)
}

fn panel_with_toolbar(
    this: &Desktop,
    window: &mut Window,
    cx: &mut Context<Desktop>,
    toolbar: Option<(FocusHandle, f32)>,
) -> Option<AnyElement> {
    let call = this.native_call.as_ref()?;
    if call.minimized {
        return None;
    }
    let expanded = toolbar.is_some();
    let generation = this.native_call_generation;
    let surface = call.surface.read(cx);
    let has_video_area = surface.sources.iter().next().is_some();
    let selection = *call.quality.borrow();
    let stream = surface.sources.featured(selection.view);
    let featured = call.featured(&surface.sources, selection);
    let stats = surface
        .stats
        .as_ref()
        .filter(|stats| Some(stats.stream) == stream)
        .cloned();
    let owned = featured.is_some_and(|feed| feed.publisher.is_none());
    let many_sources = surface.sources.iter().nth(1).is_some();
    let menu_target = this.menu.as_ref().map(|menu| menu.target);
    let actions = layout::actions()
        .id("native-call-toolbar")
        .role(Role::Toolbar)
        .aria_label("Video call controls")
        .items_center()
        .justify_center()
        .w_auto()
        .max_w_full()
        .mx_auto()
        .rounded(px(8.))
        .bg(color(SIDEBAR))
        .px_2()
        .py_1()
        .when_some(toolbar, |actions, (focus, opacity)| {
            actions
                .track_focus(&focus)
                .opacity(opacity)
                .on_hover(cx.listener(|this, hovering, _, cx| stage::hover(this, *hovering, cx)))
        })
        .children(source_controls(call, cx))
        .when(many_sources, |actions| {
            let target = MenuTarget::VideoFeeds(generation);
            actions.child(call_dropdown(
                "native-call-feeds",
                "Video sources".into(),
                generation,
                target,
                menu_target == Some(target),
                cx,
            ))
        })
        .when_some(featured, |actions, feed| {
            let target = MenuTarget::VideoQuality(generation, feed);
            let quality = quality_readout(
                feed.kind,
                selection.preference(feed).quality,
                stats.as_deref(),
            );
            actions.child(call_dropdown(
                "native-call-quality",
                format!("Quality · {quality}").into(),
                generation,
                target,
                menu_target == Some(target),
                cx,
            ))
        })
        .child(icon_button(
            "native-call-expand",
            if expanded {
                "fullscreen-exit"
            } else {
                "fullscreen"
            },
            if expanded {
                "Exit full screen"
            } else {
                "Full screen"
            },
            TEXT,
            None,
            move |this, window, cx| this.set_call_expanded(!expanded, window, cx),
            cx,
        ))
        .child(icon_button(
            "native-call-minimize",
            "window-minimize",
            "Minimize call",
            TEXT,
            None,
            |this, window, cx| this.minimize_native_call(window, cx),
            cx,
        ))
        .child(icon_button(
            "native-call-options",
            "more",
            "Video options",
            TEXT,
            None,
            move |this, window, cx| {
                this.open_video_menu(
                    generation,
                    MenuTarget::VideoStats(generation),
                    window.mouse_position(),
                    cx,
                )
            },
            cx,
        ))
        .child(stop_button(call, cx));
    let has_notices = call.source_pending.is_some() || call.reopening_screen;
    let notices = has_notices.then(|| {
        div()
            .when_some(call.source_pending, |notices, source| {
                notices.child(layout::text(
                    "native-call-source-pending",
                    portal_prompt(source),
                ))
            })
            .when(call.reopening_screen, |notices| {
                notices.child(layout::text(
                    "native-call-screen-reopening",
                    "Reconnecting screen share…",
                ))
            })
    });
    let (inline_notices, overlay_notices) = if expanded {
        (None, notices)
    } else {
        (notices, None)
    };
    let show_stats = call.stats_open && (expanded || has_video_area);
    Some(
        div()
            .id("native-call")
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .when(expanded, |panel| panel.relative().flex_1())
            .when(!expanded, |panel| {
                panel
                    .max_h(relative(0.5))
                    .px_3()
                    .py_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(color(BORDER))
                    .bg(color(MAIN))
                    .when(has_video_area, |panel| panel.h(relative(0.5)))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_video_menu(
                        generation,
                        MenuTarget::VideoStats(generation),
                        event.position,
                        cx,
                    );
                    cx.stop_propagation();
                }),
            )
            .children(inline_notices)
            .when(call.stats_open && !show_stats, |panel| {
                panel.child(video_stats(stats.as_deref(), owned))
            })
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(call.surface.clone())
                    .when(show_stats, |stage| {
                        let scroll_height = window.viewport_size().height;
                        stage.child(
                            layout::scroll_region(
                                "native-call-metrics-scroll",
                                "Video metrics",
                                scroll_height,
                                window,
                                cx,
                            )
                            .absolute()
                            .top_0()
                            .right_0()
                            .w(px(352.))
                            .max_w_full()
                            .max_h_full()
                            .pl_2()
                            .py_2()
                            .rounded(px(8.))
                            .border_1()
                            .border_color(color(BORDER))
                            .bg(tint(MAIN, 0xeb))
                            .child(video_stats(stats.as_deref(), owned)),
                        )
                    }),
            )
            .children(overlay_notices.map(|notices| {
                notices
                    .absolute()
                    .top_2()
                    .left_2()
                    .max_w_full()
                    .pr_2()
                    .bg(tint(MAIN, 0xeb))
            }))
            .child(if expanded {
                div()
                    .absolute()
                    .bottom_2()
                    .left_0()
                    .right_0()
                    .px_2()
                    .flex()
                    .justify_center()
                    .child(actions)
                    .into_any_element()
            } else {
                actions.into_any_element()
            })
            .into_any_element(),
    )
}

impl Desktop {
    fn open_video_menu(
        &mut self,
        generation: u64,
        target: MenuTarget,
        anchor: impl Into<MenuAnchor>,
        cx: &mut Context<Self>,
    ) {
        let Some(call) = self
            .native_call
            .as_ref()
            .filter(|call| self.native_call_generation == generation && !call.minimized)
        else {
            return;
        };
        if let Some(index) = self
            .client
            .servers
            .iter()
            .position(|server| server.id == call.server)
        {
            self.make_menu(index, target, anchor);
            cx.notify();
        }
    }

    pub(crate) fn video_quality_commands(
        &self,
        generation: u64,
        feed: Feed,
    ) -> impl Iterator<Item = (Quality, String, bool)> {
        let kind = feed.kind;
        let current = self.call_with_generation(generation).and_then(|call| {
            let selection = *call.quality.borrow();
            (call.featured(&call.updates.sources(), selection) == Some(feed)).then(|| {
                match selection.preference(feed).quality {
                    Quality::Auto => Quality::Auto,
                    Quality::Tier(tier) => Quality::Tier(tier.min(kind.top() as u8)),
                }
            })
        });
        std::iter::once(Quality::Auto)
            .chain((0..=kind.top() as u8).map(Quality::Tier))
            .filter_map(move |quality| {
                current.map(|current| (quality, quality_label(kind, quality), current == quality))
            })
    }

    pub(crate) fn set_video_quality(
        &mut self,
        generation: u64,
        feed: Feed,
        quality: Quality,
        cx: &mut Context<Self>,
    ) {
        if let Some(call) = self.call_with_generation(generation) {
            let sources = call.updates.sources();
            call.quality.send_if_modified(|selection| {
                call.featured(&sources, *selection) == Some(feed)
                    && selection.change(feed, &sources, |preference| preference.quality = quality)
            });
            cx.notify();
        }
    }

    fn call_with_generation(&self, generation: u64) -> Option<&Active> {
        self.native_call
            .as_ref()
            .filter(|call| self.native_call_generation == generation && !call.minimized)
    }

    pub(crate) fn video_feed_commands(&self, generation: u64) -> Vec<(Option<Feed>, String, bool)> {
        let Some(call) = self.call_with_generation(generation) else {
            return Vec::new();
        };
        let sources = call.updates.sources();
        let selection = *call.quality.borrow();
        let featured = sources.featured(selection.view).map(|stream| stream.feed);
        std::iter::once((None, "Tiled view".into(), selection.tiled))
            .chain(sources.iter().map(|stream| {
                let feed = stream.feed;
                (
                    Some(feed),
                    format!("Pin {}", feed.label()),
                    !selection.tiled && featured == Some(feed),
                )
            }))
            .collect()
    }

    pub(crate) fn set_video_feed(
        &mut self,
        generation: u64,
        feed: Option<Feed>,
        cx: &mut Context<Self>,
    ) {
        if let Some(call) = self.call_with_generation(generation) {
            let sources = call.updates.sources();
            call.quality.send_if_modified(|selection| {
                if feed.is_some_and(|feed| !sources.iter().any(|stream| stream.feed == feed)) {
                    return false;
                }
                let previous = *selection;
                if let Some(feed) = feed {
                    selection.view = Some(feed);
                }
                selection.tiled = feed.is_none();
                *selection != previous
            });
            cx.notify();
        }
    }

    fn toggle_shared_audio(&mut self, cx: &mut Context<Self>) {
        if let Some(call) = self.native_call.as_ref().filter(|call| call.stop.is_some()) {
            let _ = call.audio_requests.try_send(!call.sharing_audio);
            cx.notify();
        }
    }

    pub(crate) fn video_stats_command(&self, generation: u64) -> Option<&'static str> {
        let call = self.native_call.as_ref()?;
        (self.native_call_generation == generation && !call.minimized).then_some(
            if call.stats_open {
                "Hide video stats"
            } else {
                "Video stats"
            },
        )
    }

    pub(crate) fn toggle_video_stats(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.native_call_generation == generation
            && let Some(call) = &mut self.native_call
        {
            call.stats_open = !call.stats_open;
            cx.notify();
        }
    }
}

fn call_dropdown(
    id: &'static str,
    label: SharedString,
    generation: u64,
    target: MenuTarget,
    open: bool,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let anchor = Rc::new(Cell::new(Bounds::default()));
    let position = anchor.clone();
    let activate = Rc::new(
        move |this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>| {
            if this.menu.as_ref().is_some_and(|menu| menu.target == target) {
                this.menu = None;
                window.focus(&this.window_focus, cx);
                cx.notify();
            } else {
                this.open_video_menu(generation, target, MenuAnchor::Above(position.get()), cx);
            }
        },
    );
    let click = activate.clone();
    let key = activate.clone();
    let control = div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_expanded(open)
        .tab_index(0)
        .w(px(if matches!(target, MenuTarget::VideoQuality(..)) {
            224.
        } else {
            148.
        }))
        .max_w_full()
        .min_w_0()
        .flex_shrink_0()
        .px_3()
        .py(px(6.))
        .rounded(px(6.))
        .flex()
        .items_center()
        .gap_2()
        .bg(color(if open { SELECTED } else { ELEVATED }))
        .hover(|style| style.bg(color(SELECTED)))
        .cursor_pointer()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(color(TEXT))
        .on_click(cx.listener(move |this, _, window, cx| click(this, window, cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space" | "down") {
                key(this, window, cx);
                cx.stop_propagation();
            }
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(text!(label).with_id("call-dropdown-label")),
        )
        .child(crate::assets::icon("chevron-down", 14., color(TEXT_MUTED)))
        .child(
            canvas(move |bounds, _, _| anchor.set(bounds), |_, _, _, _| {})
                .absolute()
                .inset_0(),
        );
    layout::focusable_button(id, control, crate::dialogs::accessible_click(activate, cx))
}

fn quality_readout(kind: VideoKind, quality: Quality, stats: Option<&VideoStats>) -> String {
    if quality == Quality::Auto
        && let Some(stats) = stats.filter(|stats| stats.stream.feed.kind == kind)
    {
        if let Some(encoder) = stats.encoder {
            return format!("Auto ({}p{})", encoder.target.height, encoder.target.fps);
        }
        if let Some(height) = stats.sample.height() {
            return format!("Auto ({height}p)");
        }
    }
    quality_label(kind, quality)
}

fn quality_label(kind: VideoKind, quality: Quality) -> String {
    match quality {
        Quality::Auto => "Auto".into(),
        Quality::Tier(_) => {
            let tier = kind.ladder()[kind.request(quality, 0)];
            format!("{}p{}", tier.height, tier.fps)
        }
    }
}

fn video_stats(stats: Option<&VideoStats>, owned: bool) -> impl IntoElement {
    div()
        .id("native-call-video-stats")
        .role(Role::Group)
        .aria_label("Video stats")
        .flex()
        .flex_wrap()
        .gap_x_2()
        .gap_y_1()
        .min_w_0()
        .children(
            stats::rows(
                stats.map(|stats| stats.sample),
                stats.and_then(|stats| stats.encoder),
                owned,
            )
            .map(|(label, value)| {
                div()
                    .w(px(156.))
                    .max_w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .child(
                        div()
                            .text_xs()
                            .text_color(color(TEXT))
                            .child(layout::text(label, label)),
                    )
                    .child(div().text_sm().child(layout::text((label, 1_usize), value)))
            }),
        )
}

fn video_kind(source: Source) -> VideoKind {
    match source {
        Source::Camera => VideoKind::Camera,
        Source::Screen => VideoKind::Screen,
    }
}

fn source_controls(call: &Active, cx: &mut Context<Desktop>) -> Vec<AnyElement> {
    if !call.can_publish && !call.local_sources.into_iter().any(|active| active) {
        return Vec::new();
    }
    let mut controls: Vec<_> = [
        (
            Source::Camera,
            "native-call-camera",
            "video",
            "Start camera",
            "Stop camera",
        ),
        (
            Source::Screen,
            "native-call-screen",
            "screen",
            "Share screen",
            "Stop sharing screen",
        ),
    ]
    .into_iter()
    .filter_map(|(source, id, icon, start, stop)| {
        let active = call.local_sources[publisher::index(source)];
        if !active && (!call.can_publish || call.source_pending.is_some()) {
            return None;
        }
        Some(icon_button(
            id,
            icon,
            if active { stop } else { start },
            if active { ACCENT } else { TEXT },
            active.then_some(ACCENT),
            move |this, _, cx| this.choose_native_source(source, cx),
            cx,
        ))
    })
    .collect();
    if call.local_sources[publisher::index(Source::Screen)] && audio::can_share() {
        controls.push(icon_button(
            "native-call-audio",
            "volume",
            if call.sharing_audio {
                "Stop sharing audio"
            } else {
                "Share audio"
            },
            if call.sharing_audio { ACCENT } else { TEXT },
            call.sharing_audio.then_some(ACCENT),
            |this, _, cx| this.toggle_shared_audio(cx),
            cx,
        ));
    }
    controls
}

fn portal_prompt(source: Source) -> &'static str {
    match source {
        Source::Camera => "Waiting for camera permission",
        Source::Screen => "Choose a screen or window",
    }
}

fn purpose(source: Source) -> Purpose {
    match source {
        Source::Camera => Purpose::Camera,
        Source::Screen => Purpose::ScreenVideo,
    }
}

fn stop_button(call: &Active, cx: &mut Context<Desktop>) -> impl IntoElement {
    icon_button(
        "native-call-stop",
        "hang-up",
        if call.owner {
            "End call"
        } else {
            "Leave video"
        },
        DANGER_TEXT,
        Some(DANGER),
        |this, _, cx| {
            this.stop_native();
            cx.notify();
        },
        cx,
    )
}

fn return_button(generation: u64, cx: &mut Context<Desktop>) -> AnyElement {
    styled_button(
        "native-call-restore",
        "Return to call",
        Tone::Secondary,
        move |this, window, cx| {
            if this.native_call_generation == generation {
                this.restore_native_call(window, cx);
            }
        },
        cx,
    )
}

pub fn minimized(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let call = this.native_call.as_ref()?;
    if !call.minimized
        && this.selected_indices().is_some_and(|(index, channel)| {
            let server = &this.client.servers[index];
            this.native_call_in(&server.id, &server.channels[channel].target)
        })
    {
        return None;
    }
    Some(
        layout::actions()
            .id("native-call-minimized")
            .items_center()
            .when(call.monitor, |bar| bar.child("Sharing entire monitor"))
            .child(return_button(this.native_call_generation, cx))
            .child(stop_button(call, cx))
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(color(BORDER))
            .into_any_element(),
    )
}

pub fn card_actions(
    this: &Desktop,
    server: &crate::state::ServerView,
    target: &TextTarget,
    room: &str,
    inviter: Option<(u32, u64)>,
    cx: &mut Context<Desktop>,
) -> Option<Div> {
    this.native_call.as_ref().filter(|call| {
        call.minimized
            && call.stop.is_some()
            && call.server == server.id
            && call.voice_generation == server.generation
            && call.conversation == *target
            && call.room == room
            && call.inviter == inviter
    })?;
    Some(layout::actions().child(return_button(this.native_call_generation, cx)))
}

pub async fn run(
    start: Start,
    mut settings: watch::Receiver<ClientSettings>,
    events: mpsc::Sender<ServiceEvent>,
) {
    let generation = start.generation;
    let result = drive(start, &mut settings, &events).await;
    let _ = events
        .send(ServiceEvent::NativeFinished { generation, result })
        .await;
}

async fn drive(
    start: Start,
    settings: &mut watch::Receiver<ClientSettings>,
    events: &mpsc::Sender<ServiceEvent>,
) -> Result<Finish, String> {
    let Start {
        owner,
        crypto,
        generation,
        url,
        host,
        policy,
        source,
        source_requests,
        mut audio_requests,
        mut volumes,
        updates,
        quality,
        mut stop,
    } = start;
    let revoke = async {
        loop {
            if settings.borrow_and_update().relay_only && !policy.relay_only {
                return;
            }
            if settings.changed().await.is_err() {
                return;
            }
        }
    };
    let cancel = async {
        tokio::select! {
            _ = &mut stop => {},
            () = revoke => {},
        }
    };
    tokio::pin!(cancel);
    let options = Connect {
        generation,
        host: &host,
        link: &url,
        codecs: &[Codec::Av1],
        policy,
    };
    let (mut media, video_worker, host) = tokio::select! {
        biased;
        () = &mut cancel => return Ok(Finish::Left),
        result = prepare(crypto, options) => result?,
    };
    let mut display = Display {
        source_changes: SourceChanges { generation, events },
        updates,
        quality,
    };
    let mut publisher = publisher::Publisher::new(policy.relay_only);
    let mut viewer = viewer::Viewer::new(
        video_worker.clone(),
        display.updates.clone(),
        policy.relay_only,
    );
    display.updates.host(host);
    let mut audio = audio::Audio::new(display.updates.clone());
    let result = tokio::select! {
        biased;
        () = &mut cancel => Ok(Finish::Left),
        result = meeting(
            &mut media, &mut display, &mut publisher, &mut viewer,
            (video_worker.as_path(), source), source_requests, (&mut audio, &mut audio_requests, &mut volumes),
        ) => result,
    };
    let (closed, (), decoded, ()) =
        tokio::join!(media.stop(), publisher.stop(), viewer.stop(), audio.stop());
    let result = result.and_then(|finish| closed.map_err(text).and(decoded).map(|()| finish));
    if owner && matches!(result, Ok(Finish::MediaEnded) | Err(_)) {
        if let Err(error) = result {
            tracing::warn!(%error, "native video stopped while the meeting remains open");
        }
        display.updates.layout(Sources::default(), [None; TILES]);
        display
            .updates
            .status("Meeting open · browser guests can use the chat link");
        display
            .source_changes
            .report(SourceChange::MediaClosed)
            .await?;
        cancel.await;
        return Ok(Finish::Left);
    }
    result
}

#[derive(Clone, Copy)]
struct SourceChanges<'a> {
    generation: u64,
    events: &'a mpsc::Sender<ServiceEvent>,
}

impl SourceChanges<'_> {
    async fn report(self, change: SourceChange) -> Result<(), String> {
        self.events
            .send(ServiceEvent::NativeSourceChanged {
                generation: self.generation,
                change,
            })
            .await
            .map_err(text)
    }
}

#[cfg(not(target_os = "linux"))]
struct Grant(Source);

#[cfg(not(target_os = "linux"))]
impl Grant {
    fn source(&self) -> Source {
        self.0
    }
    fn can_include_app(&self) -> bool {
        self.0 == Source::Screen
    }
}

type Picked = (Source, Result<Option<Grant>, String>);
type Picker = BoxFuture<'static, Picked>;

fn pick_source(source: Source) -> Picker {
    async move {
        #[cfg(target_os = "linux")]
        let result = match nuschl_ipc::video::portal::request(source, None).await {
            Ok(grant) => Ok(Some(grant)),
            Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => Ok(None),
            Err(error) => Err(text(error)),
        };
        #[cfg(not(target_os = "linux"))]
        let result = Ok(Some(Grant(source)));
        (source, result)
    }
    .boxed()
}

async fn next_grant(picker: &mut Option<Picker>) -> Picked {
    match picker {
        Some(picker) => picker.await,
        None => pending().await,
    }
}

enum SourceAction {
    Request(Option<SourceRequest>),
    Reopened(publisher::CaptureAttempt, Option<Grant>),
}

async fn next_source_action(
    [camera, screen]: &mut [mpsc::Receiver<SourceRequest>; 2],
    reopening: &mut Option<publisher::Reopening>,
) -> SourceAction {
    tokio::select! {
        biased;
        request = screen.recv() => SourceAction::Request(request),
        request = camera.recv() => SourceAction::Request(request),
        (attempt, grant) = publisher::next_reopen(reopening) => SourceAction::Reopened(attempt, grant),
    }
}

fn activate_source(
    media: &mut Media,
    publisher: &mut publisher::Publisher,
    grant: Grant,
    worker: &Path,
    selection: Selection,
    updates: &Arc<Latest>,
    attempt: publisher::CaptureAttempt,
) -> Result<SourceChange, String> {
    let source = grant.source();
    let monitor = grant.can_include_app();
    if let Err(error) = media.set_source(purpose(source), true) {
        return Ok(SourceChange::Failed(source, text(error)));
    }
    match publisher.start(grant, worker, selection, updates.clone(), attempt) {
        Ok(()) => Ok(SourceChange::Active { source, monitor }),
        Err(error) => {
            media.set_source(purpose(source), false).map_err(text)?;
            Ok(SourceChange::Failed(source, error))
        }
    }
}

async fn meeting(
    media: &mut Media,
    display: &mut Display<'_>,
    publisher: &mut publisher::Publisher,
    viewer: &mut viewer::Viewer,
    (worker, initial): (&Path, Option<Source>),
    mut source_requests: [mpsc::Receiver<SourceRequest>; 2],
    (audio, audio_requests, volumes): (
        &mut audio::Audio,
        &mut mpsc::Receiver<bool>,
        &mut watch::Receiver<audio::Volumes>,
    ),
) -> Result<Finish, String> {
    let changes = display.source_changes;
    let mut selection = *display.quality.borrow_and_update();
    let mut picker = initial.map(pick_source);
    let mut allowed = None;
    let mut timer = tokio::time::interval(stats::INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    display.updates.status("Waiting for video");
    loop {
        let sources = Sources {
            local: publisher.sources(),
            remote: viewer.sources(),
        };
        let tiles = sources.tiles(selection);
        let visible = display.updates.layout(sources, tiles);
        publisher.preview(tiles, visible);
        viewer.layout(
            media,
            if visible { tiles } else { [None; TILES] },
            selection,
        )?;
        let can_publish = media.can_publish();
        if allowed != Some(can_publish) {
            changes
                .report(SourceChange::CanPublish(can_publish))
                .await?;
            allowed = Some(can_publish);
        }
        if !can_publish && publisher.reopening.take().is_some() {
            changes
                .report(SourceChange::Stopped(Source::Screen))
                .await?;
        }
        // Stop sharing ends the screen's audio too.
        if audio.sharing()
            && !publisher.enabled(Source::Screen)
            && let Some(change) = audio.share(media, false).await?
        {
            changes.report(SourceChange::Audio(change)).await?;
        }
        let [camera, screen] = &mut publisher.slots;
        let (source, frame) = tokio::select! {
            event = media.next() => {
                let event = match event {
                    Some(Ok(event)) => event,
                    Some(Err(nuschl_client::native::sfu::Error::Refused(reason))) if reason == "ended" => {
                        return Ok(Finish::RoomEnded);
                    }
                    Some(Err(error)) => return Err(text(error)),
                    None => return Ok(Finish::MediaEnded),
                };
                let Some(event) = audio.event(event).await else { continue };
                let retune = matches!(event, MediaEvent::SourceLimits { .. } | MediaEvent::Route(_));
                publisher.event(&event);
                if retune {
                    publisher.retune(display, selection, None);
                }
                viewer.handle(media, event)?;
                continue;
            }
            (source, result) = next_grant(&mut picker) => {
                picker = None;
                let change = match result {
                    Ok(Some(grant)) => activate_source(
                        media, publisher, grant, worker, selection, &display.updates, publisher::CaptureAttempt::default(),
                    )?,
                    Ok(None) => SourceChange::Cancelled(source),
                    Err(error) => SourceChange::Failed(source, error),
                };
                changes.report(change).await?;
                publisher.retune(display, selection, None);
                continue;
            }
            action = next_source_action(&mut source_requests, &mut publisher.reopening) => {
                match action {
                    SourceAction::Request(request) => {
                        let Some(SourceRequest { source, enable }) = request else { return Ok(Finish::Left); };
                        if enable {
                            if picker.is_none() && !publisher.enabled(source) && can_publish {
                                picker = Some(pick_source(source));
                            } else {
                                changes.report(SourceChange::Cancelled(source)).await?;
                            }
                        } else if publisher.enabled(source) {
                            publisher.retire(source);
                            media.set_source(purpose(source), false).map_err(text)?;
                            changes.report(SourceChange::Stopped(source)).await?;
                            publisher.retune(display, selection, None);
                        }
                    }
                    SourceAction::Reopened(attempt, grant) => {
                        publisher.reopening = None;
                        let change = if !media.can_publish() {
                            SourceChange::Stopped(Source::Screen)
                        } else if let Some(grant) = grant {
                            activate_source(media, publisher, grant, worker, selection, &display.updates, attempt)?
                        } else {
                            SourceChange::Failed(Source::Screen, "Choose Share screen to open the source again.".into())
                        };
                        let worker_started = matches!(change, SourceChange::Active { .. });
                        tracing::info!(target: "nuschl_capture", attempt = attempt.number, worker_started, "screen capture reopen result");
                        changes.report(change).await?;
                        publisher.retune(display, selection, None);
                    }
                }
                continue;
            }
            Some(share) = audio_requests.recv() => {
                if let Some(change) = audio.share(media, share && publisher.enabled(Source::Screen)).await? {
                    changes.report(SourceChange::Audio(change)).await?;
                }
                continue;
            }
            Ok(()) = volumes.changed() => {
                audio.set_volumes(&volumes.borrow_and_update());
                continue;
            }
            Some(event) = audio.next() => {
                if let Some(change) = audio.handle(media, event).await? {
                    changes.report(SourceChange::Audio(change)).await?;
                }
                continue;
            }
            Some(decoded) = viewer.decoded.recv() => {
                viewer.complete(media, display, decoded)?;
                continue;
            }
            frame = publisher::LocalVideo::next(camera) => (Source::Camera, frame),
            frame = publisher::LocalVideo::next(screen) => (Source::Screen, frame),
            _ = publisher.retiring.next(), if !publisher.retiring.is_empty() => continue,
            Ok(()) = display.quality.changed() => {
                selection = *display.quality.borrow_and_update();
                publisher.retune(display, selection, None);
                continue;
            }
            _ = timer.tick() => {
                let report = sources.featured(selection.view).map(|stream| stream.feed);
                publisher.retune(display, selection, report);
                if let Some(feed) = report {
                    viewer.report(display, feed);
                }
                continue;
            }
        };
        match frame {
            Ok(frame) => publisher.frame(source, frame, media)?,
            Err(error) => {
                media.set_source(purpose(source), false).map_err(text)?;
                let change = if matches!(
                    error,
                    nuschl_ipc::video::Error::Capture(CaptureFailure::Cancelled, _)
                ) {
                    publisher.retire(source);
                    SourceChange::Cancelled(source)
                } else if publisher.recover(source, media.can_publish(), &error) {
                    SourceChange::ReopeningScreen
                } else {
                    let error = if source == Source::Screen {
                        format!("{error} Choose Share screen to select the source again.")
                    } else {
                        text(error)
                    };
                    SourceChange::Failed(source, error)
                };
                changes.report(change).await?;
                publisher.retune(display, selection, None);
            }
        }
    }
}

async fn prepare(
    crypto: CryptoHandle,
    options: Connect<'_>,
) -> Result<(Media, PathBuf, Option<u64>), String> {
    let (executable, video_worker) = WORKERS
        .as_ref()
        .ok_or("The native video workers are unavailable.")?
        .clone();
    let connection = Connection::connect(crypto, options).await.map_err(text)?;
    let welcome = connection.welcome();
    let host = welcome
        .peers
        .iter()
        .find(|peer| peer.role == nuschl_protocol::sfu::Role::Host && peer.id != welcome.peer)
        .map(|peer| peer.id);
    let media = connection
        .into_media(MediaOptions {
            executable,
            sfu_port: SFU_PORT,
            publish: None,
        })
        .await
        .map_err(text)?;
    Ok((media, video_worker, host))
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        Instant, Quality, Source, SourceAction, SourceRequest, VideoKind, VideoStats,
        next_source_action, publisher::CaptureAttempt, quality_readout, stats,
    };
    use futures_util::FutureExt as _;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn queued_stop_wins_when_screen_reopen_is_ready() {
        let [(camera, camera_requests), (screen, screen_requests)] =
            std::array::from_fn(|_| mpsc::channel(1));
        camera
            .try_send(SourceRequest {
                source: Source::Camera,
                enable: true,
            })
            .unwrap();
        let mut requests = [camera_requests, screen_requests];
        screen
            .try_send(SourceRequest {
                source: Source::Screen,
                enable: false,
            })
            .unwrap();
        let mut reopening = Some(async { (CaptureAttempt::default(), None) }.boxed());
        assert!(matches!(
            next_source_action(&mut requests, &mut reopening).await,
            SourceAction::Request(Some(SourceRequest {
                source: Source::Screen,
                enable: false,
            }))
        ));
    }
    use nuschl_ipc::video::CaptureLimits;

    #[test]
    fn auto_quality_shows_the_current_encoder_target_or_received_resolution() {
        let now = Instant::now();
        let kind = VideoKind::Screen;
        let mut measurements = stats::Stats::new(false, now);
        measurements.frame(2580, 1080, None);
        let mut video = VideoStats {
            stream: super::Stream {
                feed: super::Feed::local(kind),
                serial: 1,
                preview: true,
            },
            sample: measurements.sample(now + stats::INTERVAL),
            encoder: None,
        };
        assert_eq!(quality_readout(kind, Quality::Auto, None), "Auto");
        assert_eq!(
            quality_readout(kind, Quality::Auto, Some(&video)),
            "Auto (1080p)"
        );
        video.encoder = Some(stats::Encoder {
            requested: CaptureLimits {
                height: 1440,
                fps: 60,
                bitrate: 8_000_000,
            },
            limit: "Within selected quality",
            target: CaptureLimits {
                height: 1440,
                fps: 60,
                bitrate: 8_000_000,
            },
            budget_bps: None,
            ceiling: None,
            hardware: None,
        });
        assert_eq!(
            quality_readout(kind, Quality::Auto, Some(&video)),
            "Auto (1440p60)"
        );
    }
}
