// SPDX-License-Identifier: EUPL-1.2
//! Context menus for users, channels and servers, and the text prompt dialog.
//! Entries the server would refuse are hidden using its reported permissions.
use crate::{
    assets::icon,
    dialogs::{Tone, modal, styled_button},
    names::display_name,
    native_call::VideoChoice,
    state::{Desktop, Panel, Status},
    theme::*,
};
use gpui_kit::base::{Align, Placement, Positioner};
use gpui_kit::{
    base::{
        FocusTrapElement,
        input::{Input, InputState},
    },
    prelude::*,
    *,
};
use nuschl_client::native::{Presences, UserKey, sfu::Quality};
use nuschl_session::{
    connection::{ActionSubject, Command, CommandAuthority, TextTarget, User},
    model::{ChannelEdit, UserAction, VoiceTargetSpec, permission as perm},
};
use std::rc::Rc;

/// Voice target ids registered with the server (1–30 are available).
pub const WHISPER_TARGET: u32 = 1;

const QUALITY_MENU_GAP: Pixels = px(4.);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuTarget {
    User(u32),
    Channel(u32),
    Server,
    /// An image in a message (`Desktop::menu_image`).
    Image,
    /// A message's text (`Desktop::text.menu`).
    Message,
    /// An inherited permission in the channel permissions table
    /// (`Desktop::menu_acl`).
    AclSource,
    VideoStats(u64),
    VideoQuality(u64, crate::native_call::Feed),
    VideoFeeds(u64),
    /// The cards this server has, drawn by `crate::integrations::cards`. Open
    /// across toggles, so several are shown or hidden in one visit.
    /// Stays open: the user toggles several cards in a row.
    Integrations,
}

/// Original saved endpoint plus observed connection/target lifetimes. Local
/// preferences use `local_index`; live actions require `live_index` and carry
/// this snapshot through the native queue into the session actor.
#[derive(Clone, Debug)]
pub struct ActionOrigin {
    server: String,
    endpoint: (String, u16, String, Option<String>),
    generation: u64,
    authority: CommandAuthority,
    primary: MenuTarget,
    conversation: Option<nuschl_client::Conversation>,
}
impl ActionOrigin {
    pub fn capture(view: &crate::state::ServerView, primary: MenuTarget) -> Self {
        let own = view
            .own_session
            .and_then(|id| view.users.get(&id))
            .map(|user| (user.session, user.instance))
            .unwrap_or((0, 0));
        let mut users = std::collections::BTreeSet::from([own.0]);
        let mut channels = std::collections::BTreeSet::from([0]);
        channels.extend(view.voice_channel());
        match primary {
            MenuTarget::User(id) => {
                users.insert(id);
                channels.extend(view.users.get(&id).map(|user| user.channel));
            }
            MenuTarget::Channel(id) => {
                channels.insert(id);
                if let Some(channel) = view.channels.iter().find(|channel| channel.id == id) {
                    channels.extend(
                        channel
                            .links
                            .iter()
                            .copied()
                            .take(nuschl_session::connection::MAX_ACTION_TARGETS.saturating_sub(3)),
                    );
                }
            }
            _ => {}
        }
        Self {
            server: view.id.clone(),
            endpoint: (
                view.bookmark.host.clone(),
                view.bookmark.port,
                view.bookmark.username.clone(),
                view.bookmark.certificate_sha256.clone(),
            ),
            generation: view.generation,
            authority: CommandAuthority {
                accounts: Vec::new(),
                subject: match primary {
                    MenuTarget::User(id) => view
                        .users
                        .get(&id)
                        .map(|user| ActionSubject::User(id, user.instance)),
                    MenuTarget::Channel(id) => view
                        .channels
                        .iter()
                        .find(|channel| channel.id == id)
                        .map(|channel| ActionSubject::Channel(id, channel.instance)),
                    _ => None,
                },
                conversations: view
                    .channels
                    .iter()
                    .filter(|channel| match primary {
                        MenuTarget::Channel(id) => channel.target == TextTarget::Channel(id),
                        MenuTarget::User(id) => channel
                            .direct_peer
                            .as_ref()
                            .is_some_and(|peer| peer.session == id),
                        _ => false,
                    })
                    .filter_map(|channel| channel.conversation)
                    .take(nuschl_session::connection::MAX_ACTION_TARGETS)
                    .collect(),
                own,
                users: users
                    .into_iter()
                    .filter_map(|id| view.users.get(&id).map(|user| (id, user.instance)))
                    .collect(),
                channels: channels
                    .into_iter()
                    .filter_map(|id| {
                        view.channels
                            .iter()
                            .find(|channel| channel.target == TextTarget::Channel(id))
                            .map(|channel| (id, channel.instance))
                    })
                    .collect(),
            },
            primary,
            conversation: None,
        }
    }
    pub fn local_index(&self, this: &Desktop) -> Option<usize> {
        self.local_index_in(&this.client.servers)
    }
    fn local_index_in(&self, servers: &[crate::state::ServerView]) -> Option<usize> {
        servers.iter().position(|view| {
            view.id == self.server
                && view.bookmark.host == self.endpoint.0
                && view.bookmark.port == self.endpoint.1
                && view.bookmark.username == self.endpoint.2
                && view.bookmark.certificate_sha256 == self.endpoint.3
        })
    }
    pub fn live_index(&self, this: &Desktop) -> Option<usize> {
        self.live_index_in(&this.client.servers, this.client.navigation.selection())
    }
    fn live_index_in(
        &self,
        servers: &[crate::state::ServerView],
        selection: Option<&nuschl_client::Conversation>,
    ) -> Option<usize> {
        let index = self.local_index_in(servers)?;
        let view = &servers[index];
        if view.status != Status::Connected
            || view.generation != self.generation
            || view.own_session != Some(self.authority.own.0)
            || self.authority.own.1 == 0
            || view
                .users
                .get(&self.authority.own.0)
                .is_none_or(|user| user.instance != self.authority.own.1)
        {
            return None;
        }
        let valid = match self.primary {
            MenuTarget::User(id) => self
                .authority
                .users
                .iter()
                .find(|(held, _)| *held == id)
                .is_some_and(|(_, instance)| {
                    view.users
                        .get(&id)
                        .is_some_and(|user| user.instance == *instance)
                }),
            MenuTarget::Channel(id) => self
                .authority
                .channels
                .iter()
                .find(|(held, _)| *held == id)
                .is_some_and(|(_, instance)| {
                    view.channels
                        .iter()
                        .any(|channel| channel.id == id && channel.instance == *instance)
                }),
            _ => true,
        };
        (valid
            && self
                .conversation
                .as_ref()
                .is_none_or(|conversation| selection == Some(conversation)))
        .then_some(index)
    }
    /// Also covering channel `id` as it is now in `view`.
    pub(crate) fn with_channel(mut self, view: &crate::state::ServerView, id: u32) -> Self {
        if let Some(channel) = view
            .channels
            .iter()
            .find(|channel| channel.target == TextTarget::Channel(id))
        {
            self.authority.channels.push((id, channel.instance));
        }
        self
    }
    pub(crate) fn with_account(&self, account: [u8; 16]) -> Self {
        let mut origin = self.clone();
        origin.authority.accounts = vec![account];
        origin
    }
    pub fn with_scope<R>(&self, this: &mut Desktop, callback: impl FnOnce(&mut Desktop) -> R) -> R {
        let driver = this.driver.clone();
        driver.with_action_scope(
            self.server.clone(),
            self.generation,
            self.authority.clone(),
            || callback(this),
        )
    }
    pub fn run(&self, this: &mut Desktop, command: Command) {
        let Some(index) = self.live_index(this) else {
            return;
        };
        self.with_scope(this, |this| this.run(index, command));
    }
}

#[derive(Clone, Copy)]
pub enum MenuAnchor {
    Point(Point<Pixels>),
    Above(Bounds<Pixels>),
}

impl From<Point<Pixels>> for MenuAnchor {
    fn from(point: Point<Pixels>) -> Self {
        Self::Point(point)
    }
}

#[derive(Clone)]
pub struct Menu {
    pub origin: ActionOrigin,
    pub invocation: u64,
    pub user: Option<Rc<User>>,
    pub image: Option<crate::viewer::ViewerImage>,
    pub message: Option<crate::selection::MessageMenu>,
    pub channel_url: Option<String>,
    pub local_share_url: Option<String>,
    pub target: MenuTarget,
    anchor: MenuAnchor,
}

#[derive(Clone, Debug)]
pub enum PromptAction {
    CreateChannel {
        parent: u32,
        temporary: bool,
        kind: nuschl_core::access::Kind,
        /// For the Password type.
        password: Entity<InputState>,
    },
    Kick {
        session: u32,
    },
    Ban {
        session: u32,
    },
    Tokens,
    Comment,
    ImportCertificate {
        path: std::path::PathBuf,
    },
    ExportCertificate,
    Nickname {
        hash: String,
    },
    /// A message in the open conversation, by its serial (no text to enter).
    DeleteMessage {
        serial: u64,
    },
}

pub struct Prompt {
    pub origin: Option<ActionOrigin>,
    pub title: String,
    pub detail: String,
    pub confirm: &'static str,
    pub danger: bool,
    pub input: Entity<InputState>,
    pub action: PromptAction,
}

type Action = Rc<dyn Fn(&mut Desktop, usize, &mut Window, &mut Context<Desktop>)>;

#[derive(Clone, Copy)]
enum Access {
    Local,
    Bookmark,
    Live,
}

enum Entry {
    Item {
        key: String,
        access: Access,
        label: String,
        icon: &'static str,
        danger: bool,
        choice: Option<bool>,
        action: Action,
    },
    Volume {
        of: Volume,
        volume: f32,
    },
    /// An explanation in place of entries that do not apply.
    Note(&'static str),
    Separator,
}

/// Whose volume a menu sets: a person's or a server's playback (by id).
#[derive(Clone, Debug)]
enum Volume {
    User(UserKey),
    Server(String),
}

impl Volume {
    /// The setting its slider edits.
    fn setting(&self) -> crate::slider::Setting {
        match self {
            Volume::User(UserKey::Certificate(hash)) => {
                crate::slider::Setting::UserVolume(hash.clone())
            }
            Volume::User(UserKey::Presence(instance)) => {
                crate::slider::Setting::PresenceVolume(*instance)
            }
            Volume::Server(id) => crate::slider::Setting::ServerVolume(id.clone()),
        }
    }
}

#[track_caller]
fn item(
    label: impl Into<String>,
    icon: &'static str,
    action: impl Fn(&mut Desktop, usize, &mut Window, &mut Context<Desktop>) + 'static,
) -> Entry {
    let label = label.into();
    let caller = std::panic::Location::caller();
    Entry::Item {
        key: format!("builtin:{}:{}", caller.file(), caller.line()),
        access: Access::Live,
        label,
        icon,
        danger: false,
        choice: None,
        action: Rc::new(action),
    }
}

#[track_caller]
fn danger(
    label: impl Into<String>,
    icon: &'static str,
    action: impl Fn(&mut Desktop, usize, &mut Window, &mut Context<Desktop>) + 'static,
) -> Entry {
    let label = label.into();
    let caller = std::panic::Location::caller();
    Entry::Item {
        key: format!("builtin:{}:{}", caller.file(), caller.line()),
        access: Access::Live,
        label,
        icon,
        danger: true,
        choice: None,
        action: Rc::new(action),
    }
}

impl Entry {
    fn access(mut self, access: Access) -> Self {
        if let Self::Item {
            access: current, ..
        } = &mut self
        {
            *current = access;
        }
        self
    }
    fn key(mut self, key: String) -> Self {
        if let Self::Item { key: current, .. } = &mut self {
            *current = key;
        }
        self
    }
}
#[track_caller]
fn local_item(
    label: impl Into<String>,
    icon: &'static str,
    action: impl Fn(&mut Desktop, usize, &mut Window, &mut Context<Desktop>) + 'static,
) -> Entry {
    item(label, icon, action).access(Access::Local)
}
#[track_caller]
fn bookmark_item(
    label: impl Into<String>,
    icon: &'static str,
    action: impl Fn(&mut Desktop, usize, &mut Window, &mut Context<Desktop>) + 'static,
) -> Entry {
    item(label, icon, action).access(Access::Bookmark)
}

impl Desktop {
    pub fn open_menu(&mut self, server: usize, target: MenuTarget, position: Point<Pixels>) {
        let Some(view) = self.client.servers.get(server) else {
            return;
        };
        if view.status == Status::Connected {
            // Ask for the permissions this menu depends on; entries appear as they arrive.
            let channel = match target {
                MenuTarget::Channel(channel) => Some(channel),
                MenuTarget::User(session) => view.users.get(&session).map(|user| user.channel),
                MenuTarget::Server
                | MenuTarget::Image
                | MenuTarget::Message
                | MenuTarget::AclSource => None,
                MenuTarget::VideoStats(_)
                | MenuTarget::VideoQuality(..)
                | MenuTarget::VideoFeeds(_)
                | MenuTarget::Integrations => None,
            };
            let wanted = channel
                .into_iter()
                .chain([0])
                .collect::<std::collections::BTreeSet<_>>();
            let now = crate::state::monotonic_ms();
            for channel in wanted {
                let view = &mut self.client.servers[server];
                if view.begin_permission_query(channel, now) {
                    let origin = ActionOrigin::capture(view, MenuTarget::Channel(channel));
                    origin.run(self, Command::RequestPermissions(channel));
                }
            }
        }
        self.make_menu(server, target, position);
    }

    pub fn make_menu(&mut self, server: usize, target: MenuTarget, anchor: impl Into<MenuAnchor>) {
        let Some(view) = self.client.servers.get(server) else {
            return;
        };
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut origin = ActionOrigin::capture(view, target);
        if target == MenuTarget::Message {
            if let Some((_, channel)) = self.selected_indices() {
                let selected = &view.channels[channel];
                origin = ActionOrigin::capture(
                    view,
                    match selected.target {
                        TextTarget::Channel(id) => MenuTarget::Channel(id),
                        TextTarget::Direct(id) => MenuTarget::User(id),
                        _ => MenuTarget::Message,
                    },
                );
                origin.authority.conversations.extend(selected.conversation);
            }
            origin.conversation = self.client.navigation.selection().cloned();
        }
        let local_share = matches!(target, MenuTarget::Channel(_))
            && !(view.status == Status::Connected
                && view.has("share/1")
                && view.own_user_id().is_some());
        let mut menu = Menu {
            origin,
            invocation: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            user: match target {
                MenuTarget::User(id) => view.users.get(&id).cloned().map(Rc::new),
                _ => None,
            },
            image: self.menu_image.clone(),
            message: self.text.menu.clone(),
            channel_url: None,
            local_share_url: None,
            target,
            anchor: anchor.into(),
        };
        if let MenuTarget::Channel(channel) = target {
            menu.channel_url = self.channel_url(server, channel);
            if local_share {
                menu.local_share_url = self
                    .client
                    .share_link(server, channel)
                    .ok()
                    .flatten()
                    .map(|link| self.share_url(&link, None));
            }
        }
        self.menu = Some(menu);
    }

    pub fn run(&mut self, server: usize, command: Command) {
        if let Some(view) = self.client.servers.get_mut(server)
            && let Err(error) = self.driver.command(&view.id, command)
        {
            view.set_notice(error);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_prompt(
        &mut self,
        server: usize,
        action: PromptAction,
        title: impl Into<String>,
        detail: impl Into<String>,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (confirm, danger) = match action {
            PromptAction::CreateChannel { .. } => ("Create", false),
            PromptAction::Kick { .. } => ("Kick", true),
            PromptAction::Ban { .. } => ("Ban", true),
            PromptAction::Tokens | PromptAction::Comment | PromptAction::Nickname { .. } => {
                ("Save", false)
            }
            PromptAction::ImportCertificate { .. } => ("Import", true),
            PromptAction::ExportCertificate => ("Choose file…", false),
            PromptAction::DeleteMessage { .. } => ("Delete", true),
        };
        let secret = matches!(
            action,
            PromptAction::ImportCertificate { .. } | PromptAction::ExportCertificate
        );
        let title: String = title.into();
        let label = title.clone();
        let input = cx.new(|cx| {
            crate::state::accessible(
                InputState::new(window, cx)
                    .default_value(initial)
                    .masked(secret),
                &label,
                false,
                secret,
            )
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        let target = match action {
            PromptAction::Kick { session } | PromptAction::Ban { session } => {
                MenuTarget::User(session)
            }
            PromptAction::CreateChannel { parent, .. } => MenuTarget::Channel(parent),
            _ => MenuTarget::Server,
        };
        let mut origin = self
            .client
            .servers
            .get(server)
            .map(|view| ActionOrigin::capture(view, target));
        if matches!(action, PromptAction::DeleteMessage { .. })
            && let Some(origin) = &mut origin
        {
            if let Some((_, channel)) = self.selected_indices() {
                let view = &self.client.servers[server];
                let selected = &view.channels[channel];
                *origin = ActionOrigin::capture(
                    view,
                    match selected.target {
                        TextTarget::Channel(id) => MenuTarget::Channel(id),
                        TextTarget::Direct(id) => MenuTarget::User(id),
                        _ => MenuTarget::Message,
                    },
                );
                origin.authority.conversations.extend(selected.conversation);
            }
            origin.conversation = self.client.navigation.selection().cloned();
        }
        self.prompt = Some(Prompt {
            origin,
            title,
            detail: detail.into(),
            confirm,
            danger,
            input,
            action,
        });
    }

    pub fn confirm_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let local = matches!(
            prompt.action,
            PromptAction::Nickname { .. }
                | PromptAction::ImportCertificate { .. }
                | PromptAction::ExportCertificate
        );
        let persistent = matches!(prompt.action, PromptAction::Tokens);
        let index = if local {
            Some(usize::MAX)
        } else if persistent {
            prompt
                .origin
                .as_ref()
                .and_then(|origin| origin.local_index(self))
        } else {
            prompt
                .origin
                .as_ref()
                .and_then(|origin| origin.live_index(self))
        };
        let Some(index) = index else {
            return;
        };
        if !local && !persistent {
            let origin = prompt.origin.clone().expect("live prompt origin");
            origin.with_scope(self, |this| this.apply_prompt(prompt, index, cx));
        } else {
            self.apply_prompt(prompt, index, cx);
        }
    }

    fn apply_prompt(&mut self, prompt: Prompt, index: usize, cx: &mut Context<Self>) {
        let text = prompt.input.read(cx).value().to_string();
        let reason = (!text.trim().is_empty()).then(|| text.trim().to_string());
        let command = match prompt.action {
            PromptAction::DeleteMessage { serial } => {
                self.delete_message(serial);
                return;
            }
            PromptAction::Nickname { hash } => {
                let mut full = false;
                self.update_settings(|settings| full = !settings.set_nickname(&hash, &text));
                if full {
                    self.error = Some(format!(
                        "You have {} nicknames, the most Nuschl keeps.",
                        nuschl_session::config::MAX_PEOPLE
                    ));
                }
                self.rename_directs();
                return;
            }
            PromptAction::ImportCertificate { path } => {
                let result = std::fs::metadata(&path)
                    .map_err(|error| error.to_string())
                    .and_then(|metadata| {
                        if metadata.len() > nuschl_session::config::MAX_PKCS12_BYTES as u64 {
                            Err("The certificate file is too large.".to_string())
                        } else {
                            std::fs::read(&path).map_err(|error| error.to_string())
                        }
                    })
                    .and_then(|bytes| self.profile.import_identity(&bytes, &text));
                self.identity_status = Some(match result {
                    Ok(identity) => format!(
                        "Imported certificate {}. Reconnect to servers to use it.",
                        identity.mumble_hash()
                    ),
                    Err(error) => error,
                });
                return;
            }
            PromptAction::ExportCertificate => {
                let directory = std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir);
                let target = cx.prompt_for_new_path(&directory, Some("nuschl-certificate.p12"));
                let password = text;
                cx.spawn(async move |this, cx| {
                    let Ok(Ok(Some(path))) = target.await else {
                        return;
                    };
                    let _ = this.update(cx, |this, cx| {
                        let result = this.profile.export_identity_to(&path, &password);
                        this.identity_status = Some(match result {
                            Ok(()) => format!("Exported to {}. Import it in Mumble via Configure → Certificate Wizard.", path.display()),
                            Err(error) => format!("Export failed: {error}"),
                        });
                        cx.notify();
                    });
                })
                .detach();
                return;
            }
            PromptAction::CreateChannel {
                parent,
                temporary,
                kind,
                password,
            } => {
                if text.trim().is_empty() {
                    return;
                }
                let password = password.read(cx).value().to_string();
                // A server that hides channels takes the type's rules with the
                // create, so nobody sees the channel before they apply.
                let rules = prompt
                    .origin
                    .as_ref()
                    .and_then(|origin| origin.local_index(self))
                    .filter(|index| {
                        kind != nuschl_core::access::Kind::Open
                            && self.client.servers[*index]
                                .has(nuschl_protocol::nuschl::capability::HIDDEN_CHANNELS)
                    })
                    .and_then(|_| nuschl_core::admin::new_channel_rules(kind, &password).ok())
                    .unwrap_or_default();
                if let Some(origin) = prompt.origin.clone() {
                    let typed = !rules.is_empty();
                    crate::acl_editor::created(
                        self,
                        origin,
                        parent,
                        text.trim(),
                        kind,
                        password,
                        typed,
                    );
                }
                Command::CreateChannel {
                    parent,
                    name: text.trim().to_string(),
                    temporary,
                    rules,
                }
            }
            PromptAction::Kick { session } => Command::User {
                session,
                action: UserAction::Kick,
                reason,
            },
            PromptAction::Ban { session } => Command::User {
                session,
                action: UserAction::Ban,
                reason,
            },
            PromptAction::Comment => Command::SetComment(text),
            PromptAction::Tokens => {
                let tokens: Vec<String> = text
                    .split([',', '\n'])
                    .map(|token| token.trim().to_string())
                    .filter(|token| !token.is_empty())
                    .take(64)
                    .collect();
                if let Some(server) = self.client.servers.get_mut(index) {
                    server.bookmark.tokens = tokens.clone();
                }
                let bookmarks: Vec<_> = self
                    .client
                    .servers
                    .iter()
                    .map(|server| server.bookmark.clone())
                    .collect();
                if let Err(error) = self.write_servers(&bookmarks) {
                    self.error = Some(error.to_string());
                }
                Command::SetTokens(tokens)
            }
        };
        self.run(index, command);
    }

    pub fn import_certificate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import certificate".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.open_prompt(
                    usize::MAX,
                    PromptAction::ImportCertificate { path },
                    "Certificate password",
                    "The password chosen when the certificate was exported (often empty). Your current certificate is kept as a backup.",
                    String::new(),
                    window,
                    cx,
                );
                cx.notify();
            });
        })
        .detach();
    }

    /// Direct conversations are named after the other person: follow a
    /// nickname change.
    fn rename_directs(&mut self) {
        for server in &mut self.client.servers {
            for channel in &mut server.channels {
                if let Some(peer) = &channel.direct_peer {
                    channel.name = format!("@ {}", display_name(peer, &self.settings));
                }
            }
        }
    }

    pub fn whisper(&mut self, server: usize, target: VoiceTargetSpec, label: String) {
        let Some(view) = self.client.servers.get_mut(server) else {
            return;
        };
        match self.driver.voice_command(
            &view.id,
            view.generation,
            Command::Whisper {
                revision: 0,
                id: WHISPER_TARGET,
                targets: vec![target],
            },
        ) {
            Ok(()) => view.whisper = Some(label),
            Err(error) => {
                view.whisper = Some("Voice suspended; select a target again".into());
                view.set_notice(error);
            }
        }
    }
    pub fn stop_whisper(&mut self, server: usize) {
        let Some(view) = self.client.servers.get_mut(server) else {
            return;
        };
        match self
            .driver
            .voice_command(&view.id, view.generation, Command::SelectVoiceTarget(0))
        {
            Ok(()) => view.whisper = None,
            Err(error) => {
                view.whisper = Some("Voice suspended; select normal talking again".into());
                view.set_notice(error);
            }
        }
    }

    fn menu_entries(&self, menu: &Menu) -> Vec<Entry> {
        let mut entries = Vec::new();
        if matches!(menu.target, MenuTarget::Integrations) {
            let Some(index) = menu.origin.local_index(self) else {
                return entries;
            };
            let server = self.client.servers[index].id.clone();
            let rows: Vec<_> = self
                .client
                .surfaces(index)
                .iter()
                .map(|entry| {
                    let key = crate::integrations::surface_key(&server, entry);
                    let shown = !self.dismissed_surfaces.contains(&key);
                    (
                        key,
                        shown,
                        crate::integrations::describe(self, index, entry),
                    )
                })
                .collect();
            for (key, shown, label) in rows {
                let id = format!(
                    "integration:{}-{}-{}-{}",
                    key.0,
                    key.1.unwrap_or(0),
                    key.2,
                    key.3
                );
                let mut item = local_item(label, "check", move |this, _, _, cx| {
                    this.toggle_integration(key.clone(), cx);
                })
                .key(id);
                if let Entry::Item { choice, .. } = &mut item {
                    *choice = Some(shown);
                }
                entries.push(item);
            }
            return entries;
        }
        if let MenuTarget::VideoStats(generation) = menu.target {
            if let Some(label) = self.video_stats_command(generation) {
                entries.push(local_item(label, "info", move |this, _, _, cx| {
                    this.toggle_video_stats(generation, cx);
                }));
            }
            return entries;
        }
        if let MenuTarget::VideoQuality(generation, feed) = menu.target {
            for (video, label, selected) in self.video_quality_commands(generation, feed) {
                // The tier limits follow an own source's modes.
                if video == VideoChoice::Quality(Quality::Auto) && !entries.is_empty() {
                    entries.push(Entry::Separator);
                }
                let mut entry = local_item(label, "check", move |this, _, _, cx| {
                    this.set_video_choice(generation, feed, video, cx);
                })
                .key(format!("quality:{video:?}"));
                if let Entry::Item { choice, .. } = &mut entry {
                    *choice = Some(selected);
                }
                entries.push(entry);
            }
            return entries;
        }
        if let MenuTarget::VideoFeeds(generation) = menu.target {
            return self
                .video_feed_commands(generation)
                .into_iter()
                .map(|(feed, label, selected)| {
                    let mut entry = local_item(label, "check", move |this, _, _, cx| {
                        this.set_video_feed(generation, feed, cx);
                    })
                    .key(format!("feed:{feed:?}"));
                    if let Entry::Item { choice, .. } = &mut entry {
                        *choice = Some(selected);
                    }
                    entry
                })
                .collect();
        }
        if menu.target == MenuTarget::AclSource {
            for (number, (jump, label)) in self.menu_acl.iter().enumerate() {
                let jump = jump.clone();
                let mut entry = local_item(label.clone(), "search", move |this, _, _, cx| {
                    crate::acl_editor::show_source(this, jump.clone());
                    cx.notify();
                });
                // One call site for every source channel: keys differ by number.
                if let Entry::Item { key, .. } = &mut entry {
                    key.push_str(&format!(":{number}"));
                }
                entries.push(entry);
            }
            return entries;
        }
        if matches!(menu.target, MenuTarget::Image | MenuTarget::Message) {
            match menu.target {
                MenuTarget::Image => {
                    if let Some(image) = menu.image.clone() {
                        let viewed = image.clone();
                        if self.viewer.is_none() {
                            entries.push(local_item(
                                "Open image",
                                "search",
                                move |this, _index, _, _| this.open_viewer(viewed.clone()),
                            ));
                        }
                        let copied = image.clone();
                        entries.push(local_item(
                            "Copy image",
                            "copy",
                            move |this, _index, _, cx| this.copy_image(copied.image.clone(), cx),
                        ));
                        if image.source.is_some() {
                            entries.push(local_item(
                                "Save image as…",
                                "record",
                                move |this, _index, _, cx| this.save_image(&image, cx),
                            ));
                        }
                    }
                }
                MenuTarget::Message => {
                    if let Some(message) = &menu.message {
                        if let Some(link) = message.link.clone() {
                            entries.push(local_item(
                                "Copy link",
                                "link",
                                move |_, _index, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        link.to_string(),
                                    ))
                                },
                            ));
                        }
                        if let Some(selection) = message.selection.clone() {
                            entries.push(local_item("Copy", "copy", move |_, _index, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    selection.to_string(),
                                ))
                            }));
                        }
                        if !message.text.is_empty() {
                            let text = message.text.clone();
                            entries.push(local_item(
                                "Copy message",
                                "copy",
                                move |_, _index, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        text.to_string(),
                                    ))
                                },
                            ));
                        }
                        // Nuschl servers: edit or delete (see editing.rs).
                        let serial = message.serial;
                        let (edit, delete) = self
                            .selected_indices()
                            .filter(|_| menu.origin.live_index(self).is_some())
                            .and_then(|(server, channel)| {
                                let view = &self.client.servers[server];
                                let channel = &view.channels[channel];
                                let message = channel
                                    .timeline
                                    .messages
                                    .iter()
                                    .find(|message| message.serial == serial)?;
                                Some((
                                    view.may_edit(channel, message),
                                    view.may_delete(channel, message),
                                ))
                            })
                            .unwrap_or_default();
                        if edit || delete {
                            entries.push(Entry::Separator);
                        }
                        if edit {
                            entries.push(item(
                                "Edit message",
                                "pencil",
                                move |this, _index, window, cx| this.start_edit(serial, window, cx),
                            ));
                        }
                        if delete {
                            entries.push(danger(
                                "Delete message",
                                "trash",
                                move |this, _index, window, cx| {
                                    this.confirm_delete(serial, window, cx)
                                },
                            ));
                        }
                    }
                }
                _ => unreachable!(),
            }
            return entries;
        }
        let Some(index) = menu.origin.local_index(self) else {
            return entries;
        };
        let server = &self.client.servers[index];
        let connected = menu.origin.live_index(self).is_some();
        let own = server.own_session;
        let my_channel = server.voice_channel();
        match menu.target {
            // Handled above, before the entries that need a live connection.
            MenuTarget::Integrations => {}
            MenuTarget::User(session) => {
                let Some(user) = menu
                    .user
                    .as_ref()
                    .map(|original| {
                        server
                            .users
                            .get(&session)
                            .filter(|current| current.instance == original.instance)
                            .unwrap_or(original.as_ref())
                    })
                    .cloned()
                else {
                    return entries;
                };
                let name = display_name(&user, &self.settings).to_string();
                let mine = own == Some(session);
                let channel = user.channel;
                let registered = user.user_id.filter(|_| !mine);
                entries.push(item(
                    "Profile and connection",
                    "info",
                    move |this, index, _, _| {
                        this.run(index, Command::RequestStats(session));
                        this.render_comment(index, session);
                        if let Some(user_id) = registered {
                            crate::identity::check(this, index, user_id, false);
                        }
                        this.panel = Some(Panel::UserInfo {
                            origin: ActionOrigin::capture(
                                &this.client.servers[index],
                                MenuTarget::User(session),
                            ),
                            server: index,
                            session,
                        });
                    },
                ));
                if mine {
                    entries.push(item("Edit comment…", "pencil", move |this, index, window, cx| {
                        let current = this.client.servers[index].users.get(&session).and_then(|user| user.comment.clone()).unwrap_or_default();
                        this.open_prompt(index, PromptAction::Comment, "Your comment", "Shown to others in your profile. Plain text; line breaks are kept.", nuschl_legacy_html::html_text(&current, '\n'), window, cx);
                    }));
                    if user.user_id.is_none() && server.may_try(0, perm::SELF_REGISTER) {
                        entries.push(item(
                            "Register this name",
                            "certificate",
                            move |this, index, _, _| this.run(index, Command::RegisterSelf),
                        ));
                    }
                } else {
                    entries.push(item(
                        "Send direct message",
                        "message",
                        move |this, index, window, cx| this.open_direct(index, session, window, cx),
                    ));
                    volume_entries(&mut entries, (&self.settings, &self.presences), &user);
                    match user.certificate_hash.clone() {
                        Some(hash) => people_entries(&mut entries, &self.settings, &user, hash),
                        None => entries.push(Entry::Note(
                            "No certificate: cannot be a friend or have a nickname",
                        )),
                    }
                    if server.may_try(channel, perm::WHISPER) {
                        let label = format!("Whispering to {name}");
                        entries.push(item(
                            format!("Whisper to {name}"),
                            "whisper",
                            move |this, index, _, _| {
                                this.whisper(
                                    index,
                                    VoiceTargetSpec::Users(vec![session]),
                                    label.clone(),
                                );
                            },
                        ));
                    }
                    if let Some(mine) = my_channel.filter(|mine| server.may_move(session, *mine)) {
                        entries.push(
                            item("Move to my channel", "volume", move |this, index, _, _| {
                                if let Err(error) = this.client.move_user(index, session, mine) {
                                    this.client.servers[index].set_notice(error);
                                }
                            })
                            .key(format!("move-user:{session}:{mine}")),
                        );
                    }
                    if server.may(channel, perm::MOVE) {
                        entries.push(
                            item(
                                "Move to channel…",
                                "volume",
                                move |this, index, window, cx| {
                                    this.begin_keyboard_move(index, session, window, cx);
                                },
                            )
                            .key(format!("move-user-to:{session}")),
                        );
                    }
                }
                let moderate = server.may(channel, perm::MUTE_DEAFEN);
                let root = |bits| server.may(0, bits);
                if moderate
                    || root(perm::KICK)
                    || root(perm::BAN)
                    || root(perm::REGISTER)
                    || root(perm::RESET_USER_CONTENT)
                {
                    entries.push(Entry::Separator);
                }
                if moderate {
                    let (muted, deafened, priority) =
                        (user.muted, user.deafened, user.priority_speaker);
                    entries.push(
                        item(
                            if muted {
                                "Server unmute"
                            } else {
                                "Server mute"
                            },
                            "mic-off",
                            move |this, index, _, _| {
                                this.run(
                                    index,
                                    Command::User {
                                        session,
                                        action: UserAction::Mute(!muted),
                                        reason: None,
                                    },
                                );
                            },
                        )
                        .key(format!("server-mute:{session}:{muted}")),
                    );
                    entries.push(
                        item(
                            if deafened {
                                "Server undeafen"
                            } else {
                                "Server deafen"
                            },
                            "headphones-off",
                            move |this, index, _, _| {
                                this.run(
                                    index,
                                    Command::User {
                                        session,
                                        action: UserAction::Deafen(!deafened),
                                        reason: None,
                                    },
                                );
                            },
                        )
                        .key(format!("server-deafen:{session}:{deafened}")),
                    );
                    entries.push(
                        item(
                            if priority {
                                "Remove priority speaker"
                            } else {
                                "Make priority speaker"
                            },
                            "star",
                            move |this, index, _, _| {
                                this.run(
                                    index,
                                    Command::User {
                                        session,
                                        action: UserAction::PrioritySpeaker(!priority),
                                        reason: None,
                                    },
                                );
                            },
                        )
                        .key(format!("priority:{session}:{priority}")),
                    );
                }
                if !mine && user.user_id.is_none() && root(perm::REGISTER) {
                    entries.push(item(
                        format!("Register {name}"),
                        "certificate",
                        move |this, index, _, _| {
                            this.run(
                                index,
                                Command::User {
                                    session,
                                    action: UserAction::Register,
                                    reason: None,
                                },
                            );
                        },
                    ));
                }
                if root(perm::RESET_USER_CONTENT) {
                    entries.push(item("Reset comment", "pencil", move |this, index, _, _| {
                        this.run(
                            index,
                            Command::User {
                                session,
                                action: UserAction::ResetComment,
                                reason: None,
                            },
                        );
                    }));
                    entries.push(item("Reset avatar", "user", move |this, index, _, _| {
                        this.run(
                            index,
                            Command::User {
                                session,
                                action: UserAction::ResetAvatar,
                                reason: None,
                            },
                        );
                    }));
                }
                if !mine && root(perm::KICK) {
                    let title = format!("Kick {name}");
                    entries.push(danger(
                        "Kick…",
                        "hang-up",
                        move |this, index, window, cx| {
                            this.open_prompt(
                                index,
                                PromptAction::Kick { session },
                                title.clone(),
                                "Optional reason, shown to the user.",
                                String::new(),
                                window,
                                cx,
                            );
                        },
                    ));
                }
                if !mine && root(perm::BAN) {
                    let title = format!("Ban {name}");
                    entries.push(danger(
                        "Ban…",
                        "shield",
                        move |this, index, window, cx| {
                            this.open_prompt(
                                index,
                                PromptAction::Ban { session },
                                title.clone(),
                                "Bans this certificate and address. Optional reason.",
                                String::new(),
                                window,
                                cx,
                            );
                        },
                    ));
                }
                push_context_actions(
                    &mut entries,
                    server,
                    index,
                    |action| action.user,
                    Some(session),
                    None,
                );
            }
            MenuTarget::Channel(channel) => {
                let Some(view) = server
                    .channels
                    .iter()
                    .find(|entry| entry.target == TextTarget::Channel(channel))
                else {
                    return entries;
                };
                let links = view.links.clone();
                let listening = own
                    .and_then(|own| server.users.get(&own))
                    .is_some_and(|user| user.listening.contains(&channel));
                if connected && my_channel != Some(channel) && server.may_try(channel, perm::ENTER)
                {
                    entries.push(item("Join channel", "volume", move |this, index, _, _| {
                        this.run(index, Command::Join(channel))
                    }));
                }
                if connected && my_channel != Some(channel) && server.may_try(channel, perm::LISTEN)
                {
                    entries.push(
                        item(
                            if listening {
                                "Stop listening"
                            } else {
                                "Listen to channel"
                            },
                            "ear",
                            move |this, index, _, _| {
                                this.run(
                                    index,
                                    Command::Listen {
                                        channel,
                                        listen: !listening,
                                    },
                                );
                            },
                        )
                        .key(format!("listen:{channel}:{listening}")),
                    );
                }
                if connected && server.may_try(channel, perm::WHISPER) {
                    let label = format!("Shouting to #{}", view.name);
                    entries.push(item(
                        "Shout to channel",
                        "whisper",
                        move |this, index, _, _| {
                            this.whisper(
                                index,
                                VoiceTargetSpec::Channel {
                                    channel,
                                    links: false,
                                    children: false,
                                    group: None,
                                },
                                label.clone(),
                            );
                        },
                    ));
                    let label = format!("Shouting to #{} and sub-channels", view.name);
                    entries.push(item(
                        "Shout to channel and sub-channels",
                        "whisper",
                        move |this, index, _, _| {
                            this.whisper(
                                index,
                                VoiceTargetSpec::Channel {
                                    channel,
                                    links: false,
                                    children: true,
                                    group: None,
                                },
                                label.clone(),
                            );
                        },
                    ));
                }
                if let Some(mine) = my_channel.filter(|mine| *mine != channel)
                    && server.may(channel, perm::LINK_CHANNEL)
                {
                    let linked = links.contains(&mine);
                    entries.push(
                        item(
                            if linked {
                                "Unlink from my channel"
                            } else {
                                "Link with my channel"
                            },
                            "link",
                            move |this, index, _, _| {
                                let (add, remove) = if linked {
                                    (vec![], vec![mine])
                                } else {
                                    (vec![mine], vec![])
                                };
                                this.run(
                                    index,
                                    Command::LinkChannels {
                                        channel,
                                        add,
                                        remove,
                                    },
                                );
                            },
                        )
                        .key(format!("link:{channel}:{mine}:{linked}")),
                    );
                }
                if links.len() >= nuschl_session::connection::MAX_ACTION_TARGETS
                    && server.may(channel, perm::LINK_CHANNEL)
                {
                    entries.push(Entry::Note(
                        "Too many links for one delayed action; unlink individual channels.",
                    ));
                } else if !links.is_empty() && server.may(channel, perm::LINK_CHANNEL) {
                    let links_key = format!("{links:?}");
                    entries.push(
                        item("Unlink all", "link", move |this, index, _, _| {
                            this.run(
                                index,
                                Command::LinkChannels {
                                    channel,
                                    add: vec![],
                                    remove: links.iter().copied().collect(),
                                },
                            );
                        })
                        .key(format!("unlink-all:{channel}:{links_key}")),
                    );
                }
                if let Some(url) = menu.channel_url.clone() {
                    entries.push(local_item("Copy link", "link", move |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                    }));
                }
                if let Some(url) = menu.local_share_url.clone() {
                    entries.push(local_item("Copy share link", "link", move |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                    }));
                } else {
                    entries.push(item(
                        "Copy share link",
                        "link",
                        move |this, index, _, cx| {
                            this.copy_share_link(index, channel, cx);
                        },
                    ));
                }
                let write = server.may(channel, perm::WRITE);
                let edit_acl = write || server.may(0, perm::WRITE);
                let permanent = server.may(channel, perm::MAKE_CHANNEL);
                let temporary = server.may(channel, perm::MAKE_TEMP_CHANNEL);
                let create = permanent || temporary;
                if connected && (write || create || edit_acl) {
                    entries.push(Entry::Separator);
                }
                // Separate calls: a menu item's key is its call site.
                let create = move |temporary: bool| {
                    move |this: &mut Desktop,
                          index: usize,
                          window: &mut Window,
                          cx: &mut Context<Desktop>| {
                        let password = cx.new(|cx| {
                            crate::state::accessible(
                                InputState::new(window, cx).placeholder("Channel password"),
                                "Channel password",
                                false,
                                false,
                            )
                        });
                        this.open_prompt(
                            index,
                            PromptAction::CreateChannel {
                                parent: channel,
                                temporary,
                                kind: nuschl_core::access::Kind::Open,
                                password,
                            },
                            "Create a channel",
                            if temporary {
                                "Name of the new channel. It disappears when it is empty."
                            } else {
                                "Name of the new channel."
                            },
                            String::new(),
                            window,
                            cx,
                        );
                    }
                };
                if connected && permanent {
                    entries.push(item("Create sub-channel…", "plus", create(false)));
                }
                if connected && temporary {
                    entries.push(item("Create temporary sub-channel…", "plus", create(true)));
                }
                if connected && write {
                    entries.push(item(
                        "Edit channel…",
                        "pencil",
                        move |this, index, window, cx| {
                            this.open_channel_editor(index, channel, window, cx)
                        },
                    ));
                }
                if connected && edit_acl {
                    entries.push(item(
                        "Edit permissions…",
                        "shield",
                        move |this, index, window, cx| {
                            crate::acl_editor::open(this, index, channel, window, cx)
                        },
                    ));
                }
                if connected && write && channel != 0 {
                    entries.push(danger(
                        "Remove channel",
                        "window-close",
                        move |this, index, _, _| this.run(index, Command::RemoveChannel(channel)),
                    ));
                }
                push_context_actions(
                    &mut entries,
                    server,
                    index,
                    |action| action.channel,
                    None,
                    Some(channel),
                );
            }
            MenuTarget::Server => {
                if connected {
                    // Your voice: talk here, and how loud this server plays.
                    let role = self.client.voice_role(index);
                    if matches!(
                        role,
                        nuschl_client::voice::Role::Listening
                            | nuschl_client::voice::Role::Deafened
                    ) {
                        entries.push(bookmark_item(
                            "Use voice here",
                            "mic",
                            move |this, index, _, _| this.use_voice_here(index),
                        ));
                    }
                    entries.push(Entry::Volume {
                        of: Volume::Server(server.id.clone()),
                        volume: self.settings.server_volume(&server.id),
                    });
                    entries.push(Entry::Separator);
                }
                entries.push(bookmark_item(
                    "Connection info",
                    "signal",
                    move |this, index, _, _| {
                        this.panel = Some(Panel::Connection {
                            server: index,
                            origin: ActionOrigin::capture(
                                &this.client.servers[index],
                                MenuTarget::Server,
                            ),
                        })
                    },
                ));
                if connected {
                    let registered = own
                        .and_then(|own| server.users.get(&own))
                        .is_some_and(|user| user.user_id.is_some());
                    if !registered && server.may_try(0, perm::SELF_REGISTER) {
                        entries.push(item(
                            "Register your name",
                            "certificate",
                            move |this, index, _, _| this.run(index, Command::RegisterSelf),
                        ));
                    }
                    // managed.json declares a managed server's tokens.
                    if !server.bookmark.managed {
                        let tokens = server.bookmark.tokens.join(", ");
                        entries.push(bookmark_item("Access tokens…", "keyboard", move |this, index, window, cx| {
                            this.open_prompt(index, PromptAction::Tokens, "Access tokens", "Comma-separated. Tokens grant membership in password-protected groups and are saved with this server.", tokens.clone(), window, cx);
                        }));
                    }
                    if server.may(0, perm::REGISTER) {
                        entries.push(item(
                            "Registered users…",
                            "user",
                            move |this, index, _, _| {
                                this.client.servers[index].registered_users = None;
                                this.run(index, Command::QueryRegisteredUsers);
                                this.panel = Some(Panel::RegisteredUsers {
                                    server: index,
                                    origin: ActionOrigin::capture(
                                        &this.client.servers[index],
                                        MenuTarget::Server,
                                    ),
                                });
                            },
                        ));
                    }
                    if server.may(0, perm::BAN) {
                        entries.push(item("Ban list…", "shield", move |this, index, _, _| {
                            this.client.servers[index].bans = None;
                            this.run(index, Command::QueryBans);
                            this.panel = Some(Panel::Bans {
                                server: index,
                                origin: ActionOrigin::capture(
                                    &this.client.servers[index],
                                    MenuTarget::Server,
                                ),
                            });
                        }));
                    }
                }
                entries.push(bookmark_item(
                    "Edit server…",
                    "pencil",
                    move |this, index, window, cx| this.open_form(Some(index), window, cx),
                ));
                if let Some(position) = self
                    .client
                    .server_order()
                    .iter()
                    .position(|id| id == &server.id)
                {
                    if position > 0 {
                        let id = server.id.clone();
                        entries.push(bookmark_item(
                            "Move server earlier",
                            "chevron-up",
                            move |this, _index, window, cx| {
                                this.move_saved_server(&id, -1, window, cx)
                            },
                        ));
                    }
                    if position + 1 < self.client.server_order().len() {
                        let id = server.id.clone();
                        entries.push(bookmark_item(
                            "Move server later",
                            "chevron-down",
                            move |this, _index, window, cx| {
                                this.move_saved_server(&id, 1, window, cx)
                            },
                        ));
                    }
                }
                if connected {
                    entries.push(danger("Disconnect", "hang-up", move |this, index, _, _| {
                        this.disconnect(index)
                    }));
                }
                push_context_actions(
                    &mut entries,
                    server,
                    index,
                    |action| action.server,
                    None,
                    None,
                );
            }
            MenuTarget::Image | MenuTarget::Message | MenuTarget::AclSource => unreachable!(),
            MenuTarget::VideoStats(_)
            | MenuTarget::VideoQuality(..)
            | MenuTarget::VideoFeeds(_) => unreachable!(),
        }
        entries.retain(|entry| {
            !matches!(
                entry,
                Entry::Item {
                    access: Access::Live,
                    ..
                }
            ) || connected
        });
        entries
    }

    pub(crate) fn reconcile_delayed_actions(&mut self, cx: &mut Context<Self>) {
        if matches!(self.panel.as_ref(),Some(Panel::Acl {origin,..}) if origin.live_index(self).is_none())
        {
            crate::acl_editor::retain_stale(self, cx);
            return;
        }
        if let Some(panel) = self.panel.as_ref() {
            let index = panel.origin().and_then(|origin| match panel {
                Panel::Connection { .. } => origin.local_index(self),
                _ => origin.live_index(self),
            });
            if panel.origin().is_some() {
                match index {
                    Some(index) => {
                        if let Some(panel) = self.panel.as_mut() {
                            panel.set_server(index);
                        }
                    }
                    None => self.panel = None,
                }
            }
        }
    }

    fn activate_menu(
        &mut self,
        menu: &Menu,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .menu
            .as_ref()
            .is_none_or(|current| current.invocation != menu.invocation)
        {
            return;
        }
        let Some(Entry::Item { action, access, .. }) = self
            .menu_entries(menu)
            .into_iter()
            .find(|entry| matches!(entry, Entry::Item { key: current, .. } if current == key))
        else {
            return;
        };
        let index = match access {
            Access::Local => menu.origin.local_index(self).unwrap_or(usize::MAX),
            Access::Bookmark => match menu.origin.local_index(self) {
                Some(index) => index,
                None => return,
            },
            Access::Live => match menu.origin.live_index(self) {
                Some(index) => index,
                None => return,
            },
        };
        // The integrations picker stays open: hiding one card is a step, not
        // the end of the visit.
        let keep_open = matches!(menu.target, MenuTarget::Integrations);
        let mut activate = |this: &mut Self| {
            if !keep_open {
                this.menu = None;
                window.focus(&this.window_focus, cx);
            }
            action(this, index, window, cx);
        };
        if matches!(access, Access::Live) {
            menu.origin.with_scope(self, activate);
        } else {
            activate(self);
        }
    }

    /// Shows or hides one integration card, keyed by
    /// [`crate::integrations::surface_key`].
    pub fn toggle_integration(
        &mut self,
        key: crate::integrations::SurfaceKey,
        cx: &mut Context<Self>,
    ) {
        if !self.dismissed_surfaces.remove(&key) {
            self.dismissed_surfaces.insert(key);
        }
        cx.notify();
    }

    pub fn channel_url(&self, server: usize, channel: u32) -> Option<String> {
        let server = self.client.servers.get(server)?;
        let mut path = Vec::new();
        let mut current = Some(channel);
        while let Some(id) = current.filter(|id| *id != 0) {
            let view = server
                .channels
                .iter()
                .find(|entry| entry.target == TextTarget::Channel(id))?;
            path.push(nuschl_core::percent::encode(&view.name, ""));
            current = view.parent;
            if path.len() > 64 {
                return None;
            }
        }
        path.reverse();
        let host = if server.bookmark.host.contains(':') {
            format!("[{}]", server.bookmark.host)
        } else {
            server.bookmark.host.clone()
        };
        Some(format!(
            "mumble://{host}:{}/{}",
            server.bookmark.port,
            path.join("/")
        ))
    }

    pub fn open_channel_editor(
        &mut self,
        server: usize,
        channel: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.client.servers.get(server).and_then(|server| {
            server
                .channels
                .iter()
                .find(|entry| entry.target == TextTarget::Channel(channel))
        }) else {
            return;
        };
        let name = cx.new(|cx| {
            crate::state::accessible(
                InputState::new(window, cx).default_value(view.name.clone()),
                "Channel name",
                false,
                false,
            )
        });
        let description = cx.new(|cx| {
            crate::state::accessible(
                InputState::new(window, cx)
                    .default_value(nuschl_legacy_html::html_text(&view.description_raw, '\n')),
                "Channel description",
                false,
                false,
            )
        });
        let position = cx.new(|cx| {
            crate::state::accessible(
                InputState::new(window, cx).default_value(view.position.to_string()),
                "Position",
                false,
                false,
            )
        });
        let max_users = cx.new(|cx| {
            crate::state::accessible(
                InputState::new(window, cx).default_value(view.max_users.to_string()),
                "Maximum users",
                false,
                false,
            )
        });
        if !view.description_raw.is_empty()
            || !self
                .ui_of(server)
                .is_some_and(|ui| ui.descriptions.contains_key(&channel))
        {
            let origin =
                ActionOrigin::capture(&self.client.servers[server], MenuTarget::Channel(channel));
            origin.with_scope(self, |this| {
                let id = this.client.servers[server].id.clone();
                let _ = this.driver.command(
                    &id,
                    Command::RequestBlobs {
                        textures: vec![],
                        comments: vec![],
                        descriptions: vec![channel],
                    },
                );
            });
        }
        self.panel = Some(Panel::ChannelEdit {
            origin: ActionOrigin::capture(
                &self.client.servers[server],
                MenuTarget::Channel(channel),
            ),
            server,
            channel,
            name,
            description,
            position,
            max_users,
        });
    }

    pub fn save_channel_editor(&mut self, cx: &mut Context<Self>) {
        let Some(Panel::ChannelEdit {
            origin,
            server: _,
            channel,
            name,
            description,
            position,
            max_users,
        }) = self.panel.take()
        else {
            return;
        };
        let Some(server) = origin.live_index(self) else {
            return;
        };
        origin.with_scope(self, |this| {
            let read = |input: &Entity<InputState>| input.read(cx).value().to_string();
            let edit = ChannelEdit {
                name: Some(read(&name).trim().to_string()).filter(|name| !name.is_empty()),
                description: Some(read(&description)),
                position: read(&position).trim().parse().ok(),
                max_users: read(&max_users).trim().parse().ok(),
                parent: None,
            };
            this.run(server, Command::EditChannel { channel, edit });
        });
    }

    pub fn render_comment(&mut self, server: usize, session: u32) {
        let cached_source = self
            .client
            .servers
            .get(server)
            .and_then(|view| self.ui.get(&view.id))
            .and_then(|ui| ui.comments.get(&session))
            .map(|(source, _)| source.as_str());
        self.client.open_comment(server, session, cached_source);
    }
}

fn push_context_actions(
    entries: &mut Vec<Entry>,
    server: &crate::state::ServerView,
    _index: usize,
    applies: impl Fn(&nuschl_session::model::ContextAction) -> bool,
    session: Option<u32>,
    channel: Option<u32>,
) {
    let actions: Vec<_> = server
        .context_actions
        .iter()
        .filter(|action| applies(action))
        .cloned()
        .collect();
    if actions.is_empty() {
        return;
    }
    entries.push(Entry::Separator);
    for action in actions {
        let name = action.action.clone();
        let key = format!("context:{name}:{session:?}:{channel:?}");
        entries.push(
            item(action.text.clone(), "more", move |this, index, _, _| {
                this.run(
                    index,
                    Command::ContextAction {
                        action: name.clone(),
                        session,
                        channel,
                    },
                );
            })
            .key(key),
        );
    }
}

/// The volume slider and "Mute for me" for `user` (see [`UserKey`]).
fn volume_entries(
    entries: &mut Vec<Entry>,
    (settings, presences): (&nuschl_session::config::ClientSettings, &Presences),
    user: &User,
) {
    let key = UserKey::of(user);
    let (volume, muted) = key.playback(settings, presences);
    entries.push(Entry::Volume {
        of: Volume::User(key.clone()),
        volume,
    });
    entries.push(
        local_item(
            if muted {
                "Unmute for me"
            } else {
                "Mute for me"
            },
            if muted { "volume" } else { "mic-off" },
            move |this, _index, _, _| this.toggle_local_mute(&key),
        )
        .key(format!("local-mute:{}:{muted}", user.session)),
    );
}

/// Friend and nickname entries for someone with a certificate, as in
/// Mumble: both are kept by certificate hash, only on this device.
fn people_entries(
    entries: &mut Vec<Entry>,
    settings: &nuschl_session::config::ClientSettings,
    user: &User,
    hash: String,
) {
    let nickname = settings
        .nickname(Some(&hash))
        .unwrap_or_default()
        .to_string();
    if settings.friend(Some(&hash)).is_some() {
        let hash = hash.clone();
        entries.push(local_item(
            "Remove friend",
            "heart",
            move |this, _index, _, _| {
                this.update_settings(|settings| {
                    settings.friends.remove(&hash);
                })
            },
        ));
    } else {
        let (hash, name) = (hash.clone(), user.name.clone());
        entries.push(local_item(
            "Add friend",
            "heart",
            move |this, _index, _, _| {
                let mut full = false;
                this.update_settings(|settings| full = !settings.add_friend(&hash, &name));
                if full {
                    this.error = Some(format!(
                        "You have {} friends, the most Nuschl keeps.",
                        nuschl_session::config::MAX_PEOPLE
                    ));
                }
            },
        ));
    }
    let name = user.name.clone();
    entries.push(local_item(
        "Set nickname…",
        "pencil",
        move |this, _index, window, cx| {
            this.open_prompt(
                usize::MAX,
                PromptAction::Nickname { hash: hash.clone() },
                format!("Nickname for {name}"),
                format!(
                    "Shown instead of “{name}”, only to you. Leave empty to show their own name."
                ),
                nickname.clone(),
                window,
                cx,
            );
        },
    ));
}

struct MenuSurfaceState {
    scroll: ScrollHandle,
    focus: FocusHandle,
    /// The focus trap's container; never focused itself.
    trap: FocusHandle,
}

/// Every context menu shares one finite, zoom-aware scroll and focus boundary.
/// Retained handles survive ordinary re-renders; opening a different menu gets
/// a fresh scope. No row measures its label or starts a scrolling timer.
#[derive(IntoElement)]
struct MenuSurface {
    identity: SharedString,
    anchor: MenuAnchor,
    quality: bool,
    /// The menu's name, for assistive technology and, when `heading` is set,
    /// the line drawn above its rows.
    label: &'static str,
    heading: bool,
    max_width: Pixels,
    rows: Vec<AnyElement>,
}

impl RenderOnce for MenuSurface {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.identity, cx, |window, cx| {
            let focus = cx.focus_handle();
            focus.focus(window, cx);
            MenuSurfaceState {
                scroll: ScrollHandle::new(),
                focus,
                trap: cx.focus_handle(),
            }
        });
        let state = state.read(cx);
        let viewport = window.viewport_size();
        // Anchored's snapping adds the native client inset only after an
        // overflow. Keep a real paint gutter in its measured child as well,
        // so a popup just inside a rounded/resize edge is still fully visible.
        let inset = window.client_inset().unwrap_or(px(0.));
        let gutter = inset + px(8.);
        let reserve = (gutter + inset) * 2.;
        let width = (viewport.width - reserve).max(px(0.)).min(self.max_width);
        let height = match self.anchor {
            MenuAnchor::Point(_) => viewport.height - reserve,
            MenuAnchor::Above(trigger) => {
                // On a short viewport the choices scroll within the space
                // beside the trigger.
                trigger.top().max(viewport.height - trigger.bottom()) - gutter - QUALITY_MENU_GAP
            }
        }
        .max(px(0.));
        let content_height = (height - px(10.)).max(px(0.));
        // The scrollbar gutter exists only while the actions overflow, so
        // highlighted rows span the menu otherwise. The handle reports the
        // previous layout; a changed overflow lays out once more.
        let overflows = state.scroll.max_offset().y > px(0.);
        let scroll = state.scroll.clone();
        let content = div()
            .id("context-menu")
            .role(Role::Menu)
            .aria_label(self.label)
            .occlude()
            .w(width)
            .max_h(height)
            .p_1()
            .relative()
            .when(!self.quality, |menu| menu.rounded(px(8.)))
            .bg(if self.quality {
                tint(APP, 0xeb)
            } else {
                color(APP)
            })
            .border_1()
            .border_color(color(BORDER_STRONG))
            .shadow_lg()
            // A named menu says what it is: the picker's rows alone read as an
            // unlabelled list.
            .when(self.heading, |menu| {
                menu.child(
                    div()
                        .flex_shrink_0()
                        .px_2()
                        .pt_1()
                        .pb(px(5.))
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(TEXT_FAINT))
                        .child(text!(self.label).with_id("menu-heading")),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .mx_1()
                        .mb_1()
                        .h(px(1.))
                        .bg(color(BORDER_STRONG)),
                )
            })
            .child(
                crate::layout::scrollable(
                    "context-menu-actions",
                    "Context menu actions",
                    state.scroll.clone(),
                )
                .track_focus(&state.focus)
                .tab_stop(false)
                .max_h(content_height)
                .when(!overflows, |actions| actions.pr(px(0.)))
                .child(
                    canvas(
                        move |_, window, _| {
                            if (scroll.max_offset().y > px(0.)) != overflows {
                                window.request_animation_frame();
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size(px(0.)),
                )
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .children(self.rows),
                ),
            )
            .child(crate::layout::scrollbar(
                "context-menu-scrollbar",
                &state.scroll,
                APP,
            ))
            // A handle of its own: focus_trap tracks it, and each
            // focused handle may be tracked once per frame.
            .focus_trap("context-menu-focus", &state.trap);
        let popup = match self.anchor {
            MenuAnchor::Point(position) => anchored()
                .position(position - point(gutter, gutter))
                .snap_to_window()
                .child(div().p(gutter).child(content))
                .into_any_element(),
            MenuAnchor::Above(trigger) => Positioner::side(trigger)
                .placement(Placement::Top)
                .align(Align::Start)
                .offset(QUALITY_MENU_GAP)
                .margin(px(8.))
                .child(content)
                .into_any_element(),
        };
        deferred(popup)
    }
}

pub fn menu(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let menu = this.menu.as_ref()?;
    let entries = this.menu_entries(menu);
    if entries.is_empty() {
        return None;
    }
    let quality = matches!(menu.target, MenuTarget::VideoQuality(..));
    let (label, width) = match menu.target {
        MenuTarget::Integrations => ("Integrations", px(224.)),
        MenuTarget::VideoFeeds(_) => ("Video sources", px(224.)),
        _ if quality => ("Video quality", px(224.)),
        _ => ("Context menu", px(320.)),
    };
    let snapshot = Rc::new(menu.clone());
    let rows = entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| match entry {
            Entry::Separator => div()
                .flex_shrink_0()
                .my_1()
                .h(px(1.))
                .bg(color(BORDER_STRONG))
                .into_any_element(),
            Entry::Note(note) => div()
                .id(("menu-note", index))
                .role(Role::Label)
                .aria_label(note)
                .min_w_0()
                .flex_shrink_0()
                .px_2()
                .py(px(6.))
                .text_xs()
                .text_color(color(TEXT_FAINT))
                .child(text!(note).with_id(("menu-note-text", index)))
                .into_any_element(),
            Entry::Item {
                key,
                access: _,
                label,
                icon: name,
                danger,
                choice,
                action,
            } => {
                let _ = action;
                let element_key = format!("{}:{key}", menu.invocation);
                let pointer_key = key.clone();
                let keyboard_key = key.clone();
                let origin_menu = snapshot.clone();
                let key_menu = snapshot.clone();
                let foreground = if danger { DANGER_HOVER } else { TEXT };
                let icon_color = if quality {
                    TEXT
                } else if danger {
                    DANGER_HOVER
                } else {
                    TEXT_MUTED
                };
                let accent = color(if danger { DANGER } else { ACCENT });
                let (highlight, highlight_text) = if quality {
                    (accent.opacity(0.18), color(TEXT))
                } else {
                    (accent, color(WHITE))
                };
                let control = div()
                    .id(SharedString::from(format!("menu-item:{element_key}")))
                    .group(SharedString::from(format!("menu-item:{element_key}")))
                    .role(if choice.is_some() {
                        Role::MenuItemRadio
                    } else {
                        Role::MenuItem
                    })
                    .aria_label(label.clone())
                    .when_some(choice, |control, selected| {
                        control
                            .aria_toggled(selected.into())
                            .when(selected, |control| control.bg(color(SELECTED)))
                    })
                    .tab_index(0)
                    .min_w_0()
                    .flex_shrink_0()
                    .px_2()
                    .py(px(6.))
                    .when(!quality, |row| row.rounded(px(5.)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(color(foreground))
                    .hover(move |style| style.bg(highlight).text_color(highlight_text))
                    .focus(move |style| style.bg(highlight).text_color(highlight_text))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_menu(&origin_menu, &pointer_key, window, cx);
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if !event.keystroke.modifiers.modified()
                            && (event.keystroke.key == "enter" || event.keystroke.key == "space")
                        {
                            this.activate_menu(&key_menu, &keyboard_key, window, cx);
                            cx.notify();
                            cx.stop_propagation();
                        }
                    }))
                    .child(if choice == Some(false) {
                        div().w(px(15.)).flex_shrink_0().into_any_element()
                    } else {
                        icon(name, 15., color(icon_color))
                            .group_hover(
                                SharedString::from(format!("menu-item:{element_key}")),
                                move |style| style.text_color(highlight_text),
                            )
                            .into_any_element()
                    })
                    .child(
                        crate::layout::flexible()
                            .whitespace_normal()
                            .child(text!(label).with_id(("menu-label", index))),
                    );
                crate::layout::focusable(
                    SharedString::from(format!("menu-focus:{element_key}")),
                    control,
                )
            }
            Entry::Volume { of, volume } => {
                let what = match &of {
                    Volume::User(_) => "Volume",
                    Volume::Server(_) => "Server volume",
                };
                // The slider acts only while this menu (this invocation) is
                // open, and only on whom it was opened for.
                let target = crate::slider::Target {
                    owner: crate::slider::Owner::Menu(menu.invocation),
                    setting: of.setting(),
                };
                let readout = target.setting.readout(volume);
                let read_only = this.managed.contains(&target.setting.managed_key());
                let element_key = format!("{}:{of:?}", menu.invocation);
                div()
                    .min_w_0()
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_sm()
                    // A press here, the slider's included, keeps the menu open;
                    // the slider has seen it first.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(icon("volume", 15., color(TEXT_MUTED)))
                            .child(
                                crate::layout::flexible()
                                    .child(text!(what).with_id(("volume-label", index))),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(color(TEXT_MUTED))
                                    .child(text!(readout).with_id(("volume-value", index))),
                            ),
                    )
                    .child(crate::slider::slider(
                        crate::slider::Spec {
                            id: format!("volume:{element_key}").into(),
                            name: what.into(),
                            target,
                            value: volume,
                            read_only,
                        },
                        cx,
                    ))
                    .into_any_element()
            }
        })
        .collect::<Vec<_>>();
    let trigger: Option<Bounds<Pixels>> = match menu.anchor {
        MenuAnchor::Point(_) => None,
        MenuAnchor::Above(trigger) => Some(trigger),
    };
    Some(
        div()
            .id("menu-layer")
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if trigger.is_some_and(|bounds| bounds.contains(&event.position)) {
                        return;
                    }
                    this.menu = None;
                    window.focus(&this.window_focus, cx);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, window, cx| {
                    this.menu = None;
                    window.focus(&this.window_focus, cx);
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.menu = None;
                    window.focus(&this.window_focus, cx);
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .child(MenuSurface {
                identity: format!("context-menu:{}", menu.invocation).into(),
                anchor: menu.anchor,
                quality,
                label,
                heading: matches!(menu.target, MenuTarget::Integrations),
                max_width: width,
                rows,
            })
            .into_any_element(),
    )
}

/// The channel type to create a channel with (#341), and the password for
/// the Password type.
fn channel_kind(this: &Desktop, prompt: &Prompt, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    use nuschl_core::access::Kind;
    let PromptAction::CreateChannel { kind, password, .. } = &prompt.action else {
        return None;
    };
    let hidden = prompt
        .origin
        .as_ref()
        .and_then(|origin| origin.local_index(this))
        .is_some_and(|index| {
            this.client.servers[index].has(nuschl_protocol::nuschl::capability::HIDDEN_CHANNELS)
        });
    let options = Kind::ALL
        .into_iter()
        .map(|choice| {
            let action: crate::settings::Action = Rc::new(move |this, _, _| {
                if let Some(Prompt {
                    action: PromptAction::CreateChannel { kind, .. },
                    ..
                }) = this.prompt.as_mut()
                {
                    *kind = choice;
                }
            });
            (choice.label(), choice == *kind, action)
        })
        .collect();
    Some(
        div()
            .mt_2()
            .flex()
            .flex_col()
            .gap_2()
            .child(crate::settings::segmented(
                "create-channel-kind",
                "Channel type",
                options,
                cx,
            ))
            .child(
                div()
                    .text_sm()
                    .text_color(color(TEXT_MUTED))
                    .child(text!(kind.help(hidden)).with_id("create-channel-kind-help")),
            )
            .when(*kind == Kind::Password, |content| {
                content.child(
                    div()
                        .px_3()
                        .py(px(9.))
                        .rounded(px(6.))
                        .bg(color(APP))
                        .border_1()
                        .border_color(color(BORDER))
                        .child(Input::new(password)),
                )
            })
            .into_any_element(),
    )
}

pub fn prompt(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let prompt = this.prompt.as_ref()?;
    // A question without text to enter (deleting a message).
    let confirm_only = matches!(prompt.action, PromptAction::DeleteMessage { .. });
    let content = div()
        .flex()
        .flex_col()
        .child(
            div()
                .px_6()
                .pb_4()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(color(TEXT_MUTED))
                        .child(text!(prompt.detail.clone()).with_id("prompt-detail")),
                )
                .when(!confirm_only, |content| {
                    content.child(
                        div()
                            .mt_2()
                            .px_3()
                            .py(px(9.))
                            .rounded(px(6.))
                            .bg(color(APP))
                            .border_1()
                            .border_color(color(BORDER))
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                if event.keystroke.key == "enter" {
                                    this.confirm_prompt(cx);
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                            }))
                            .child(Input::new(&prompt.input)),
                    )
                })
                .when_some(channel_kind(this, prompt, cx), |content, kind| {
                    content.child(kind)
                }),
        )
        .child(
            div()
                .px_6()
                .py_4()
                .bg(color(SIDEBAR))
                .rounded_b(px(12.))
                .flex()
                .gap_3()
                .justify_end()
                .child(styled_button(
                    "prompt-cancel",
                    "Cancel",
                    Tone::Ghost,
                    |this, _, cx| {
                        this.prompt = None;
                        cx.notify();
                    },
                    cx,
                ))
                .child(styled_button(
                    "prompt-confirm",
                    prompt.confirm,
                    if prompt.danger {
                        Tone::Danger
                    } else {
                        Tone::Primary
                    },
                    |this, _, cx| {
                        this.confirm_prompt(cx);
                        cx.notify();
                    },
                    cx,
                )),
        )
        .into_any_element();
    Some(modal(
        this,
        "prompt-dialog",
        if confirm_only { "Confirm" } else { "Input" },
        prompt.title.clone(),
        0,
        // The channel types' labels fit on one line.
        if matches!(prompt.action, PromptAction::CreateChannel { .. }) {
            640.
        } else {
            460.
        },
        content,
        cx,
        |this| this.prompt = None,
    ))
}

#[cfg(test)]
mod target_and_focus_tests {
    use super::{
        ActionOrigin, Entry, MenuTarget, Volume, item, push_context_actions, volume_entries,
    };
    use crate::state::{ServerView, Status};
    use nuschl_client::conversation::ConversationState;
    use nuschl_client::native::{Presences, UserKey};
    use nuschl_session::{
        config::{ClientSettings, ServerBookmark},
        connection::User,
        model::ContextAction,
    };

    fn server(id: &str) -> ServerView {
        let mut bookmark = ServerBookmark::new(id, "localhost", 64738, "reader").unwrap();
        bookmark.id = id.into();
        let mut server = ServerView::new(bookmark);
        server.status = Status::Connected;
        server.generation = 7;
        server.own_session = Some(1);
        for (session, instance) in [(1, 11), (2, 22)] {
            server.users.insert(
                session,
                User {
                    session,
                    instance,
                    channel: 5,
                    ..User::default()
                },
            );
        }
        server.channels = [(0, 40), (5, 45), (6, 46)]
            .into_iter()
            .map(|(id, instance)| ConversationState {
                instance,
                ..ConversationState::channel(id, None, format!("Channel {id}"), 0)
            })
            .collect();
        server
    }
    #[test]
    fn original_server_resolves_after_reordering_but_never_to_a_replacement_slot() {
        let mut servers = vec![server("a"), server("b")];
        let origin = ActionOrigin::capture(&servers[1], MenuTarget::User(2));
        assert_eq!(origin.live_index_in(&servers, None), Some(1));
        servers.remove(0);
        assert_eq!(origin.live_index_in(&servers, None), Some(0));
        servers[0] = server("replacement");
        assert_eq!(origin.local_index_in(&servers), None);
        assert_eq!(origin.live_index_in(&servers, None), None);
    }
    #[test]
    fn reconnect_session_and_channel_reuse_revoke_while_harmless_updates_do_not() {
        let mut servers = vec![server("a")];
        let user = ActionOrigin::capture(&servers[0], MenuTarget::User(2));
        let channel = ActionOrigin::capture(&servers[0], MenuTarget::Channel(5));
        servers[0].users.get_mut(&2).unwrap().name = "renamed".into();
        servers[0].users.get_mut(&2).unwrap().channel = 6;
        servers[0].channels.retain(|channel| channel.id != 6);
        assert_eq!(user.live_index_in(&servers, None), Some(0));
        assert_eq!(channel.live_index_in(&servers, None), Some(0));
        servers[0].users.get_mut(&2).unwrap().instance = 222;
        assert_eq!(user.live_index_in(&servers, None), None);
        servers[0]
            .channels
            .iter_mut()
            .find(|channel| channel.id == 5)
            .unwrap()
            .instance = 450;
        assert_eq!(channel.live_index_in(&servers, None), None);
        servers[0].users.get_mut(&1).unwrap().instance = 111;
        let mut plain = ActionOrigin::capture(&server("a"), MenuTarget::Server);
        assert_eq!(plain.live_index_in(&servers, None), None);
        servers[0].users.get_mut(&1).unwrap().instance = 11;
        servers[0].generation = 8;
        assert_eq!(plain.live_index_in(&servers, None), None);
        plain.generation = 8;
        assert_eq!(plain.live_index_in(&servers, None), Some(0));
    }
    #[test]
    fn local_bookmark_configuration_survives_disconnect_without_retargeting_endpoint() {
        let mut servers = vec![server("a")];
        let origin = ActionOrigin::capture(&servers[0], MenuTarget::Server);
        servers[0].status = Status::Disconnected;
        servers[0].users.clear();
        assert_eq!(origin.local_index_in(&servers), Some(0));
        assert_eq!(origin.live_index_in(&servers, None), None);
        servers[0].bookmark.name = "renamed bookmark".into();
        assert_eq!(origin.local_index_in(&servers), Some(0));
        servers[0].bookmark.host = "different.invalid".into();
        assert_eq!(origin.local_index_in(&servers), None);
    }
    #[test]
    fn a_user_without_a_certificate_gets_a_volume_and_mute_for_their_presence() {
        let settings = ClientSettings::default();
        let shown = |presences: &Presences, user: &User| {
            let mut entries = vec![];
            volume_entries(&mut entries, (&settings, presences), user);
            match &entries[..] {
                [
                    Entry::Volume {
                        of: Volume::User(key),
                        volume,
                    },
                    Entry::Item { label, .. },
                ] => (key.clone(), *volume, label.clone()),
                _ => panic!("expected a volume slider and a mute entry"),
            }
        };
        let grace = User {
            session: 2,
            instance: 22,
            ..User::default()
        };
        let mut presences = Presences::default();
        assert_eq!(
            shown(&presences, &grace),
            (UserKey::Presence(22), 1., "Mute for me".into())
        );
        presences.volumes.insert(22, 0.5);
        presences.toggle_mute(22);
        assert_eq!(
            shown(&presences, &grace),
            (UserKey::Presence(22), 0.5, "Unmute for me".into())
        );
        let ada = User {
            certificate_hash: Some("abc".into()),
            ..grace.clone()
        };
        assert_eq!(
            shown(&presences, &ada),
            (UserKey::Certificate("abc".into()), 1., "Mute for me".into())
        );
    }
    fn profile(label: &str) -> Entry {
        item(label, "info", |_, _, _, _| {})
    }
    fn key(entry: &Entry) -> &str {
        match entry {
            Entry::Item { key, .. } => key,
            _ => panic!("expected action"),
        }
    }
    #[test]
    fn builtin_focus_identity_survives_label_and_permission_row_changes() {
        let before = [Entry::Separator, profile("Profile old name")];
        let after = [profile("Profile renamed"), Entry::Separator];
        assert_eq!(key(&before[1]), key(&after[0]));
    }
    #[test]
    fn duplicate_context_labels_keep_distinct_protocol_action_keys_when_reordered() {
        let mut server = server("a");
        server.context_actions = vec!["first", "second"]
            .into_iter()
            .map(|id| ContextAction {
                action: id.into(),
                text: "Same label".into(),
                user: true,
                channel: false,
                server: false,
            })
            .collect();
        let mut before = vec![];
        push_context_actions(&mut before, &server, 0, |action| action.user, Some(2), None);
        server.context_actions.reverse();
        let mut after = vec![];
        push_context_actions(&mut after, &server, 0, |action| action.user, Some(2), None);
        let keys = |entries: &[Entry]| {
            entries
                .iter()
                .filter_map(|entry| match entry {
                    Entry::Item { key, .. } => Some(key.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let before = keys(&before);
        let after = keys(&after);
        assert_ne!(before[0], before[1]);
        assert_eq!(before[0], after[1]);
        assert_eq!(before[1], after[0]);
    }
}
