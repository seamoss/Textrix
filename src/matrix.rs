//! One logged-in Matrix account per dotfile server. Everything here runs on
//! background tasks and reports back to the UI through [`AppEvent`]s.

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use matrix_sdk::{
    Client, Room, RoomMemberships, RoomState,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::{
        BackupDownloadStrategy, EncryptionSettings,
        verification::{SasState, SasVerification, VerificationRequest, VerificationRequestState},
    },
    room::{IncludeRelations, MessagesOptions, Receipts, RelationsOptions},
    ruma::{
        OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedServerName, OwnedTransactionId, OwnedUserId, RoomOrAliasId,
        UInt,
        api::Direction,
        directory::Filter as DirectoryFilter,
        events::relation::RelationType,
        room::RoomType,
        serde::Raw,
        presence::PresenceState,
        api::{
            client::{
                directory::get_public_rooms_filtered,
                filter::{FilterDefinition, LazyLoadOptions},
                presence::{get_presence, set_presence},
                room::create_room::{
                    self,
                    v3::{CreationContent, RoomPreset},
                },
                space::get_hierarchy,
                threads::get_threads,
                sync::sync_events::v3::Filter,
            },
            error::ErrorKind,
        },
        events::{
            AnyStateEvent, AnySyncEphemeralRoomEvent, InitialStateEvent, MessageLikeEventType, StateEvent,
            StateEventType, SyncStateEvent,
            presence::PresenceEvent,
            fully_read::FullyReadEventContent,
            space::child::SpaceChildEventContent,
            ignored_user_list::IgnoredUserListEventContent,
            key::verification::request::ToDeviceKeyVerificationRequestEvent,
            reaction::ReactionEventContent,
            receipt::{ReceiptThread, ReceiptType as EventReceiptType},
            relation::Annotation,
            room::{
                MediaSource, encryption::RoomEncryptionEventContent, message::RoomMessageEventContent,
                power_levels::UserPowerLevel,
            },
        },
    },
    deserialized_responses::RawSyncOrStrippedState,
    sliding_sync::{SlidingSyncList, SlidingSyncListLoadingState, SlidingSyncMode, Version as SlidingVersion},
    sync::RoomUpdates,
    TransmissionProgress,
    attachment::{AttachmentConfig, AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo, BaseVideoInfo},
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
};
use ratatui::layout::Size;
use ratatui_image::{Resize, picker::Picker, protocol::Protocol};
use eyeball::SharedObservable;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Mutex, mpsc::UnboundedSender},
    task::JoinHandle,
};

use crate::{
    config::{Paths, ReceiptMode, SyncMode, create_private_dir},
    convert::Converter,
    model::{AccountName, Item, Line, Media, Member, Permissions, Presence, PresenceKind, RoomMeta, SpaceInfo},
    servers::{EnvFile, Secret, ServerDef, write_private},
};

pub type Tx = UnboundedSender<AppEvent>;

pub enum AppEvent {
    Connected(Account),
    ConnectFailed { account: AccountName, error: String },
    /// A sync succeeded (`first` on the initial one after connecting).
    Synced { account: AccountName, first: bool },
    SyncError { account: AccountName, error: String },
    Rooms { account: AccountName, rooms: Vec<RoomMeta> },
    RoomGone { account: AccountName, room_id: OwnedRoomId },
    Timeline { account: AccountName, room_id: OwnedRoomId, items: Vec<Item>, live: bool },
    History { account: AccountName, room_id: OwnedRoomId, items: Vec<Item>, end: Option<String>, failed: bool },
    Typing { account: AccountName, room_id: OwnedRoomId, nicks: Vec<String> },
    /// Other members' read receipts: (user, nick, event they read up to).
    Receipts { account: AccountName, room_id: OwnedRoomId, receipts: Vec<(OwnedUserId, String, OwnedEventId)>, live: bool },
    UploadProgress { account: AccountName, room_id: OwnedRoomId, txn_id: OwnedTransactionId, percent: u8 },
    Presence { account: AccountName, updates: Vec<(OwnedUserId, Presence)> },
    Spaces { account: AccountName, spaces: Vec<SpaceInfo> },
    /// A thread's root and replies (chronological) for its window.
    Thread { account: AccountName, room_id: OwnedRoomId, root: OwnedEventId, items: Vec<Item> },
    /// `/threads`: the room's thread roots, newest first.
    Threads { account: AccountName, room_id: OwnedRoomId, roots: Vec<Line> },
    /// Matches from `/search -deep` in one room, newest first.
    SearchResults { account: AccountName, room_id: OwnedRoomId, hits: Vec<Line> },
    /// A window-list avatar, encoded for the terminal (`None` on failure).
    Avatar { account: AccountName, room_id: OwnedRoomId, url: OwnedMxcUri, protocol: Option<Protocol> },
    /// An inline image preview, encoded for the terminal (`None` on failure).
    ImagePreview { event_id: OwnedEventId, protocol: Option<Protocol> },
    Members { account: AccountName, room_id: OwnedRoomId, members: Vec<Member>, show: bool },
    Permissions { account: AccountName, room_id: OwnedRoomId, perms: Permissions },
    Focus { account: AccountName, room_id: OwnedRoomId },
    Sent { account: AccountName, room_id: OwnedRoomId, txn_id: OwnedTransactionId, event_id: OwnedEventId },
    SendFailed { account: AccountName, room_id: OwnedRoomId, txn_id: OwnedTransactionId, error: String },
    KeysReceived { account: AccountName, room_id: OwnedRoomId, sessions: Vec<String> },
    Recovered { account: AccountName },
    Redecrypted { account: AccountName, room_id: OwnedRoomId, item: Item },
    Profile { account: AccountName, nick: String },
    /// Whether this device is cross-signed (trusted by your other sessions).
    Trust { account: AccountName, verified: bool },
    LoggedOut { account: AccountName },
    Away { account: AccountName, message: Option<String> },
    /// Drop an ignored user's lines from the account's windows.
    Ignored { account: AccountName, user_id: OwnedUserId },
    /// A line for a room window (or the status window when `room_id` is None).
    Notice { account: Option<AccountName>, room_id: Option<OwnedRoomId>, line: Line },
}

#[derive(Serialize, Deserialize)]
struct SavedSession {
    homeserver: String,
    session: MatrixSession,
}

/// Sends notices back to the UI; cheap to clone into handlers.
#[derive(Clone)]
struct Notifier {
    name: AccountName,
    tx: Tx,
}

impl Notifier {
    fn send(&self, ev: AppEvent) {
        let _ = self.tx.send(ev);
    }

    fn line(&self, room_id: Option<OwnedRoomId>, line: Line) {
        self.send(AppEvent::Notice { account: Some(self.name.clone()), room_id, line });
    }

    fn info(&self, text: impl Into<String>) {
        self.line(None, Line::info(format!("{}: {}", self.name, text.into())));
    }

    fn error(&self, text: impl Into<String>) {
        self.line(None, Line::error(format!("{}: {}", self.name, text.into())));
    }
}

#[derive(Default)]
struct VerifySlot {
    request: Option<VerificationRequest>,
    sas: Option<SasVerification>,
}

#[derive(Clone)]
pub struct Account {
    pub name: AccountName,
    pub client: Client,
    pub user_id: OwnedUserId,
    pub homeserver: String,
    notify: Notifier,
    env: EnvFile,
    dir: PathBuf,
    verify: Arc<Mutex<VerifySlot>>,
    tasks: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,
    /// Presence sent with every sync; set by `/away`.
    away: Arc<AtomicBool>,
    sync_mode: SyncMode,
    /// This connection logged in afresh rather than restoring a session.
    fresh_login: bool,
}

/// Log in (or restore the saved session) for one dotfile server.
pub async fn connect(def: ServerDef, paths: Paths, env: EnvFile, sync_mode: SyncMode, tx: Tx) -> Result<Account> {
    let name: AccountName = def.name.as_str().into();
    let notify = Notifier { name: name.clone(), tx };
    let dir = paths.account_dir(&def.name);
    create_private_dir(&dir)?;
    let session_path = dir.join("session.json");
    let store = dir.join("store");

    let restored = session_path.exists();
    let client = match load_session(&session_path)? {
        Some(saved) => {
            if !same_user(&def.user, &saved.session.meta.user_id) {
                bail!(
                    "saved session is for {} but the dotfile says {}; run /logout {} first",
                    saved.session.meta.user_id,
                    def.user,
                    def.name
                );
            }
            notify.info(format!("restoring session for {} on {}", saved.session.meta.user_id, saved.homeserver));
            let client = client_builder(&store).homeserver_url(&saved.homeserver).build().await?;
            client.restore_session(saved.session).await?;
            client
        }
        None => {
            let homeserver = def.homeserver_or_server_name().with_context(|| {
                format!("set TEXTRIX_{}_HOMESERVER or use a full @user:server ID", def.name.to_uppercase())
            })?;
            let password = env.secret(&def.name, Secret::Password)?.with_context(|| {
                format!("no saved session and TEXTRIX_{}_PASSWORD is not set", def.name.to_uppercase())
            })?;
            set_aside_orphaned_store(&store)?;
            notify.info(format!("logging in to {homeserver} as {}", def.user));
            let login = async {
                let client = client_builder(&store).server_name_or_homeserver_url(homeserver).build().await?;
                client
                    .matrix_auth()
                    .login_username(&def.user, password.expose_secret())
                    .initial_device_display_name("Textrix")
                    .await?;
                anyhow::Ok(client)
            };
            let result = login.await;
            drop(password);
            let client = match result {
                Ok(client) => client,
                Err(e) => {
                    // The store was created for a device that never existed.
                    let _ = std::fs::remove_dir_all(&store);
                    return Err(e);
                }
            };
            let session = client.matrix_auth().session().context("login returned no session")?;
            let saved = SavedSession { homeserver: client.homeserver().to_string(), session };
            write_private(&session_path, &serde_json::to_vec_pretty(&saved)?)?;
            client
        }
    };

    let user_id = client.user_id().context("client has no user id")?.to_owned();
    Ok(Account {
        name,
        homeserver: client.homeserver().to_string(),
        client,
        user_id,
        notify,
        env,
        dir,
        verify: Default::default(),
        tasks: Default::default(),
        away: Default::default(),
        sync_mode,
        fresh_login: !restored,
    })
}

fn client_builder(store: &std::path::Path) -> matrix_sdk::ClientBuilder {
    Client::builder().sqlite_store(store, None).with_encryption_settings(EncryptionSettings {
        auto_enable_cross_signing: true,
        backup_download_strategy: BackupDownloadStrategy::OneShot,
        auto_enable_backups: true,
    })
}

fn load_session(path: &std::path::Path) -> Result<Option<SavedSession>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// `@me:server` matches exactly; a bare localpart matches any server.
fn same_user(configured: &str, saved: &OwnedUserId) -> bool {
    if configured.starts_with('@') { configured == saved.as_str() } else { configured == saved.localpart() }
}

/// A store without a session file belongs to a device we can no longer log
/// in as. Keep it (it may hold keys) but out of the way of a fresh login.
fn set_aside_orphaned_store(store: &std::path::Path) -> Result<()> {
    if store.exists() {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let aside = store.with_file_name(format!("store.orphaned-{stamp}"));
        std::fs::rename(store, &aside)?;
        tracing::warn!("moved orphaned store to {}", aside.display());
    }
    Ok(())
}

pub async fn room_meta(room: &Room) -> RoomMeta {
    let name = match room.display_name().await {
        Ok(name) => name.to_string(),
        Err(_) => room.room_id().to_string(),
    };
    let counts = room.unread_notification_counts();
    let fully_read = match room.account_data_static::<FullyReadEventContent>().await {
        Ok(Some(raw)) => raw.deserialize().ok().map(|ev| ev.content.event_id),
        _ => None,
    };
    RoomMeta {
        room_id: room.room_id().to_owned(),
        name,
        topic: room.topic().filter(|t| !t.is_empty()),
        encrypted: room.encryption_state().is_encrypted(),
        is_dm: room.is_direct().await.unwrap_or(false),
        dm_user: dm_user(room),
        is_space: room.is_space(),
        avatar: avatar_of(room).await,
        fully_read,
        unread: counts.notification_count,
        highlights: counts.highlight_count,
    }
}

/// Joined spaces and the rooms they list (`m.space.child` with a `via`).
async fn joined_spaces(client: &Client) -> Vec<SpaceInfo> {
    let mut spaces = Vec::new();
    for room in client.joined_rooms().into_iter().filter(|r| r.is_space()) {
        let mut children = Vec::new();
        if let Ok(events) = room.get_state_events_static::<SpaceChildEventContent>().await {
            for event in events {
                if let RawSyncOrStrippedState::Sync(raw) = event
                    && let Ok(SyncStateEvent::Original(ev)) = raw.deserialize()
                    && !ev.content.via.is_empty()
                {
                    children.push(ev.state_key);
                }
            }
        }
        let name = room.display_name().await.map(|n| n.to_string()).unwrap_or_else(|_| room.room_id().to_string());
        spaces.push(SpaceInfo { room_id: room.room_id().to_owned(), name, children });
    }
    spaces
}

/// The room's avatar, falling back to the other person's for a DM.
async fn avatar_of(room: &Room) -> Option<OwnedMxcUri> {
    if let Some(url) = room.avatar_url() {
        return Some(url);
    }
    let user = dm_user(room)?;
    let member = room.get_member_no_sync(&user).await.ok()??;
    member.avatar_url().map(ToOwned::to_owned)
}

fn dm_user(room: &Room) -> Option<OwnedUserId> {
    room.direct_targets().iter().find_map(|target| target.as_user_id().map(ToOwned::to_owned))
}

fn presence_from(state: &PresenceState, status: Option<String>, last_active_ago: Option<Duration>) -> Presence {
    let kind = match state {
        PresenceState::Online => PresenceKind::Online,
        PresenceState::Unavailable => PresenceKind::Away,
        _ => PresenceKind::Offline,
    };
    let last_active = last_active_ago.and_then(|ago| chrono::TimeDelta::from_std(ago).ok()).map(|ago| chrono::Local::now() - ago);
    Presence { kind, status, last_active }
}

impl Account {
    /// Announce known rooms and start syncing.
    pub fn start(&self) {
        self.register_verification_handler();

        let acct = self.clone();
        self.track(tokio::spawn(async move {
            let mut rooms = Vec::new();
            for room in acct.client.joined_rooms() {
                rooms.push(room_meta(&room).await);
            }
            acct.notify.send(AppEvent::Rooms { account: acct.name.clone(), rooms });
            acct.notify.send(AppEvent::Spaces { account: acct.name.clone(), spaces: joined_spaces(&acct.client).await });
            acct.watch_room_keys().await;
        }));

        let acct = self.clone();
        self.track(tokio::spawn(async move {
            // A session keeps the sync method it started with: Synapse can't
            // continue classic sync's encryption stream position over
            // sliding sync, so only fresh logins start on sliding sync.
            let mode_file = acct.dir.join("sync-mode");
            let remembered = std::fs::read_to_string(&mode_file).ok().map(|s| s.trim() == "sliding");
            let sliding = match acct.sync_mode {
                SyncMode::Classic => false,
                SyncMode::Sliding => true,
                SyncMode::Auto => match remembered {
                    Some(sliding) => sliding,
                    None => acct.fresh_login && !acct.client.available_sliding_sync_versions().await.is_empty(),
                },
            };
            let remember = |sliding: bool| {
                if acct.sync_mode == SyncMode::Auto && remembered != Some(sliding) {
                    let _ = std::fs::write(&mode_file, if sliding { "sliding\n" } else { "classic\n" });
                }
            };
            if sliding {
                remember(true);
                match acct.sliding_loop().await {
                    Ok(()) => return,
                    Err(e) if acct.sync_mode == SyncMode::Sliding => {
                        acct.notify.error(format!(
                            "sliding sync failed ({e:#}). A session that started on classic sync can't switch \
                             on Synapse; /logout {} and /connect to start a fresh session on sliding sync",
                            acct.name
                        ));
                        return;
                    }
                    Err(e) => acct.notify.info(format!("sliding sync unavailable ({e:#}); using classic sync")),
                }
            }
            remember(false);
            acct.sync_loop().await
        }));
    }

    /// Stop syncing. The session stays on disk for the next `/connect`.
    pub fn stop(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }

    fn track(&self, handle: JoinHandle<()>) {
        self.tasks.lock().unwrap().push(handle);
    }

    async fn sync_loop(self) {
        let settings = SyncSettings::default()
            .timeout(Duration::from_secs(30))
            .filter(Filter::FilterDefinition(FilterDefinition::with_lazy_loading()));
        let mut first = true;
        let mut trusted = false;
        let mut failing = false;
        let mut backoff = Duration::from_secs(1);
        loop {
            let presence = if self.away.load(Ordering::Relaxed) { PresenceState::Unavailable } else { PresenceState::Online };
            match self.client.sync_once(settings.clone().set_presence(presence)).await {
                Ok(response) => {
                    self.process_presence(&response.presence);
                    self.process_rooms(response.rooms, !first).await;
                    if first || failing {
                        self.notify.send(AppEvent::Synced { account: self.name.clone(), first });
                    }
                    if first {
                        first = false;
                        trusted = self.after_first_sync().await;
                    } else if !trusted {
                        // Verification by another session lands asynchronously.
                        trusted = self.own_device_trusted().await;
                        if trusted {
                            self.notify.send(AppEvent::Trust { account: self.name.clone(), verified: true });
                            self.notify.info("this device is now verified");
                        }
                    }
                    failing = false;
                    backoff = Duration::from_secs(1);
                }
                Err(e) => {
                    if let Some(ErrorKind::UnknownToken(_)) = e.client_api_error_kind() {
                        self.notify.send(AppEvent::ConnectFailed {
                            account: self.name.clone(),
                            error: format!("session is no longer valid ({e}); run /logout {} and reconnect", self.name),
                        });
                        return;
                    }
                    failing = true;
                    self.notify.send(AppEvent::SyncError { account: self.name.clone(), error: e.to_string() });
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }
    }

    /// Sliding sync (MSC4186): rooms arrive in batches; their updates come
    /// through the client's room-update broadcast like classic sync's.
    /// Returns an error only if it can't get going, so the caller can fall
    /// back to classic sync.
    async fn sliding_loop(&self) -> Result<()> {
        let state = |kind: StateEventType, key: &str| (kind, key.to_owned());
        let list = SlidingSyncList::builder("all")
            .sync_mode(SlidingSyncMode::new_growing(50))
            .timeline_limit(20)
            .required_state(vec![
                state(StateEventType::RoomCreate, ""),
                state(StateEventType::RoomName, ""),
                state(StateEventType::RoomTopic, ""),
                state(StateEventType::RoomCanonicalAlias, ""),
                state(StateEventType::RoomEncryption, ""),
                state(StateEventType::RoomPowerLevels, ""),
                state(StateEventType::RoomJoinRules, ""),
                state(StateEventType::RoomMember, "$LAZY"),
                state(StateEventType::RoomMember, "$ME"),
                state(StateEventType::SpaceChild, "*"),
            ]);
        let sync = self
            .client
            .sliding_sync("textrix")?
            .version(SlidingVersion::Native)
            .with_all_extensions()
            .poll_timeout(Duration::from_secs(30))
            .network_timeout(Duration::from_secs(60))
            .add_list(list)
            .build()
            .await?;

        // Messages count as new (activity, bells) once every room is loaded.
        let live = Arc::new(AtomicBool::new(false));
        let mut updates = self.client.subscribe_to_all_room_updates();
        let (acct, live_now) = (self.clone(), live.clone());
        self.track(tokio::spawn(async move {
            loop {
                match updates.recv().await {
                    Ok(rooms) => acct.process_rooms(rooms, live_now.load(Ordering::Relaxed)).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!("missed {missed} room updates");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }));

        let (mut first, mut trusted, mut failing) = (true, false, false);
        let mut backoff = Duration::from_secs(1);
        loop {
            let stream = sync.sync();
            futures_util::pin_mut!(stream);
            // The stream ends after an error; restart it after a pause.
            while let Some(result) = stream.next().await {
                match result {
                    Ok(_) => {
                        if first || failing {
                            self.notify.send(AppEvent::Synced { account: self.name.clone(), first });
                        }
                        if first {
                            first = false;
                            self.notify.info("receiving updates with sliding sync");
                            trusted = self.after_first_sync().await;
                        } else if !trusted {
                            trusted = self.own_device_trusted().await;
                            if trusted {
                                self.notify.send(AppEvent::Trust { account: self.name.clone(), verified: true });
                                self.notify.info("this device is now verified");
                            }
                        }
                        if !live.load(Ordering::Relaxed) {
                            let state = sync.on_list("all", |list| std::future::ready(list.state())).await;
                            if state == Some(SlidingSyncListLoadingState::FullyLoaded) {
                                live.store(true, Ordering::Relaxed);
                            }
                        }
                        failing = false;
                        backoff = Duration::from_secs(1);
                    }
                    Err(e) if first => return Err(e.into()),
                    Err(e) => {
                        if let Some(ErrorKind::UnknownToken(_)) = e.client_api_error_kind() {
                            self.notify.send(AppEvent::ConnectFailed {
                                account: self.name.clone(),
                                error: format!("session is no longer valid ({e}); run /logout {} and reconnect", self.name),
                            });
                            return Ok(());
                        }
                        failing = true;
                        self.notify.send(AppEvent::SyncError { account: self.name.clone(), error: e.to_string() });
                    }
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    fn process_presence(&self, events: &[Raw<PresenceEvent>]) {
        let updates: Vec<(OwnedUserId, Presence)> = events
            .iter()
            .filter_map(|raw| raw.deserialize().ok())
            .map(|ev| {
                let ago = ev.content.last_active_ago.map(|ms| Duration::from_millis(ms.into()));
                (ev.sender, presence_from(&ev.content.presence, ev.content.status_msg, ago))
            })
            .collect();
        if !updates.is_empty() {
            self.notify.send(AppEvent::Presence { account: self.name.clone(), updates });
        }
    }

    async fn process_rooms(&self, rooms: RoomUpdates, live: bool) {
        // Announce rooms before their events so the UI has windows for them.
        let mut metas = Vec::new();
        for room_id in rooms.joined.keys() {
            if let Some(room) = self.client.get_room(room_id) {
                metas.push(room_meta(&room).await);
            }
        }
        let spaces_changed = metas.iter().any(|m| m.is_space) || rooms.left.keys().next().is_some();
        if !metas.is_empty() {
            self.notify.send(AppEvent::Rooms { account: self.name.clone(), rooms: metas });
        }
        if spaces_changed {
            self.notify.send(AppEvent::Spaces { account: self.name.clone(), spaces: joined_spaces(&self.client).await });
        }

        for (room_id, update) in rooms.joined {
            let Some(room) = self.client.get_room(&room_id) else { continue };
            let mut conv = Converter::new(&room, &self.user_id);
            let mut items = Vec::new();
            for ev in &update.timeline.events {
                if let Some(item) = conv.convert(ev).await {
                    items.push(item);
                }
            }
            if !items.is_empty() {
                self.notify.send(AppEvent::Timeline {
                    account: self.name.clone(),
                    room_id: room_id.clone(),
                    items,
                    live,
                });
            }

            for raw in &update.ephemeral {
                match raw.deserialize() {
                    Ok(AnySyncEphemeralRoomEvent::Typing(typing)) => {
                        let mut nicks = Vec::new();
                        for user in typing.content.user_ids.iter().filter(|u| **u != self.user_id) {
                            nicks.push(conv.nick(user).await);
                        }
                        self.notify.send(AppEvent::Typing { account: self.name.clone(), room_id: room_id.clone(), nicks });
                    }
                    Ok(AnySyncEphemeralRoomEvent::Receipt(receipt)) => {
                        let mut receipts = Vec::new();
                        for (event_id, by_type) in &receipt.content.0 {
                            let Some(users) = by_type.get(&EventReceiptType::Read) else { continue };
                            for (user, r) in users {
                                let main = matches!(r.thread, ReceiptThread::Unthreaded | ReceiptThread::Main);
                                if *user != self.user_id && main {
                                    receipts.push((user.clone(), conv.nick(user).await, event_id.clone()));
                                }
                            }
                        }
                        if !receipts.is_empty() {
                            self.notify.send(AppEvent::Receipts {
                                account: self.name.clone(),
                                room_id: room_id.clone(),
                                receipts,
                                live: true,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        for room_id in rooms.left.into_keys() {
            self.notify.send(AppEvent::RoomGone { account: self.name.clone(), room_id });
        }

        for room_id in rooms.invited.into_keys() {
            let Some(room) = self.client.get_room(&room_id) else { continue };
            let name = room.display_name().await.map(|n| n.to_string()).unwrap_or_else(|_| room_id.to_string());
            let inviter = match room.invite_details().await {
                Ok(details) => details.inviter.map(|m| format!("{} [{}]", m.name(), m.user_id())),
                Err(_) => None,
            };
            self.notify.info(format!(
                "{} invited you to {name}. Type /join {room_id} to accept",
                inviter.as_deref().unwrap_or("someone"),
            ));
        }
    }

    /// Returns whether this device is already trusted.
    async fn after_first_sync(&self) -> bool {
        if let Ok(Some(nick)) = self.client.account().get_display_name().await {
            self.notify.send(AppEvent::Profile { account: self.name.clone(), nick });
        }

        if self.report_trust().await {
            return true;
        }
        match self.env.secret(&self.name, Secret::RecoveryKey) {
            Ok(Some(_)) => self.recover(),
            Ok(None) => self.notify.info(format!(
                "this device is not verified, so older encrypted messages can't be read. \
                 Set TEXTRIX_{}_RECOVERY_KEY in the dotfile and run /recover, or run /verify \
                 and accept on another session",
                self.name.to_uppercase()
            )),
            Err(e) => self.notify.error(format!("{e:#}")),
        }
        false
    }

    async fn own_device_trusted(&self) -> bool {
        match self.client.encryption().get_own_device().await {
            Ok(Some(device)) => device.is_cross_signed_by_owner(),
            _ => false,
        }
    }

    async fn report_trust(&self) -> bool {
        let verified = self.own_device_trusted().await;
        self.notify.send(AppEvent::Trust { account: self.name.clone(), verified });
        verified
    }

    /// Re-announce rooms whose keys just arrived so the UI can retry them.
    async fn watch_room_keys(&self) {
        let Some(stream) = self.client.encryption().room_keys_received_stream().await else { return };
        futures_util::pin_mut!(stream);
        while let Some(batch) = stream.next().await {
            let Ok(infos) = batch else { continue };
            let mut by_room: BTreeMap<OwnedRoomId, Vec<String>> = BTreeMap::new();
            for info in infos {
                by_room.entry(info.room_id).or_default().push(info.session_id);
            }
            for (room_id, sessions) in by_room {
                self.notify.send(AppEvent::KeysReceived { account: self.name.clone(), room_id, sessions });
            }
        }
    }

    fn register_verification_handler(&self) {
        let notify = self.notify.clone();
        let slot = self.verify.clone();
        self.client.add_event_handler(move |ev: ToDeviceKeyVerificationRequestEvent, client: Client| {
            let notify = notify.clone();
            let slot = slot.clone();
            async move {
                let Some(request) =
                    client.encryption().get_verification_request(&ev.sender, &ev.content.transaction_id).await
                else {
                    return;
                };
                let who = if request.is_self_verification() {
                    "another of your sessions".to_owned()
                } else {
                    ev.sender.to_string()
                };
                notify.info(format!(
                    "verification request from {who} (device {}). Type /verify accept",
                    ev.content.from_device
                ));
                slot.lock().await.request = Some(request);
            }
        });
    }

    fn room(&self, room_id: &OwnedRoomId) -> Result<Room> {
        self.client.get_room(room_id).with_context(|| format!("unknown room {room_id}"))
    }

    /// Room metadata from the local store only (no awaits), for reopening a
    /// closed window.
    pub fn quick_meta(&self, room_id: &OwnedRoomId) -> Option<RoomMeta> {
        let room = self.client.get_room(room_id)?;
        let counts = room.unread_notification_counts();
        Some(RoomMeta {
            room_id: room_id.clone(),
            name: room.cached_display_name().map(|n| n.to_string()).unwrap_or_else(|| room_id.to_string()),
            topic: room.topic().filter(|t| !t.is_empty()),
            encrypted: room.encryption_state().is_encrypted(),
            is_dm: room.direct_targets_length() > 0,
            dm_user: dm_user(&room),
            is_space: room.is_space(),
            avatar: room.avatar_url(),
            fully_read: None,
            unread: counts.notification_count,
            highlights: counts.highlight_count,
        })
    }

    /// Run an operation in the background, reporting failures in `room_id`'s
    /// window (or the status window).
    fn spawn_op<F>(&self, room_id: Option<OwnedRoomId>, op: F)
    where
        F: Future<Output = Result<()>> + Send + 'static,
    {
        let notify = self.notify.clone();
        tokio::spawn(async move {
            if let Err(e) = op.await {
                notify.line(room_id, Line::error(format!("{e:#}")));
            }
        });
    }

    pub fn send(&self, room_id: OwnedRoomId, content: RoomMessageEventContent, txn_id: OwnedTransactionId) {
        let acct = self.clone();
        tokio::spawn(async move {
            let result = match acct.room(&room_id) {
                Ok(room) => room.send(content).with_transaction_id(txn_id.clone()).await.map_err(anyhow::Error::from),
                Err(e) => Err(e),
            };
            let account = acct.name.clone();
            acct.notify.send(match result {
                Ok(sent) => AppEvent::Sent { account, room_id, txn_id, event_id: sent.response.event_id },
                Err(e) => AppEvent::SendFailed { account, room_id, txn_id, error: format!("{e:#}") },
            });
        });
    }

    /// `/msg`: send to a user's DM (created if needed) or a joined room.
    pub fn send_to(&self, target: String, content: RoomMessageEventContent) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            let room = if target.starts_with('@') {
                let user = OwnedUserId::try_from(target.as_str())?;
                match acct.client.get_dm_room(&user) {
                    Some(room) => room,
                    None => {
                        let room = acct.client.create_dm(&user).await?;
                        acct.notify.send(AppEvent::Rooms { account: acct.name.clone(), rooms: vec![room_meta(&room).await] });
                        room
                    }
                }
            } else {
                let id = <&RoomOrAliasId>::try_from(target.as_str()).context("expected @user, #alias or !room")?;
                let room_id = match <&matrix_sdk::ruma::RoomId>::try_from(id) {
                    Ok(room_id) => room_id.to_owned(),
                    Err(alias) => acct.client.resolve_room_alias(alias).await?.room_id,
                };
                acct.room(&room_id).context("you are not in that room")?
            };
            room.send(content).await?;
            acct.notify.line(None, Line::info(format!("message sent to {target}")));
            Ok(())
        });
    }

    /// Upload a file as an attachment (encrypted in encrypted rooms). The UI
    /// already shows a pending line keyed by `txn_id`.
    pub fn upload(&self, room_id: OwnedRoomId, path: PathBuf, txn_id: OwnedTransactionId) {
        let acct = self.clone();
        tokio::spawn(async move {
            let result = acct.upload_file(&room_id, &path, &txn_id).await;
            let account = acct.name.clone();
            acct.notify.send(match result {
                Ok(event_id) => AppEvent::Sent { account, room_id, txn_id, event_id },
                Err(e) => AppEvent::SendFailed { account, room_id, txn_id, error: format!("{e:#}") },
            });
        });
    }

    async fn upload_file(&self, room_id: &OwnedRoomId, path: &Path, txn_id: &OwnedTransactionId) -> Result<OwnedEventId> {
        let room = self.room(room_id)?;
        let data = tokio::fs::read(path).await.with_context(|| format!("reading {}", path.display()))?;
        if let Ok(max) = self.client.load_or_fetch_max_upload_size().await
            && data.len() as u64 > u64::from(max)
        {
            bail!("{} is {}, over the server's {} upload limit", path.display(), human_size(data.len() as u64), human_size(max.into()));
        }
        let filename = path.file_name().map(|n| n.to_string_lossy().into_owned()).context("not a file path")?;
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let size = Some(UInt::try_from(data.len() as u64).unwrap_or(UInt::MAX));
        let info = match mime.type_() {
            mime::IMAGE => {
                let dims = imagesize::blob_size(&data).ok();
                let px = |n: usize| UInt::try_from(n as u64).ok();
                AttachmentInfo::Image(BaseImageInfo {
                    width: dims.as_ref().and_then(|d| px(d.width)),
                    height: dims.as_ref().and_then(|d| px(d.height)),
                    size,
                    ..Default::default()
                })
            }
            mime::VIDEO => AttachmentInfo::Video(BaseVideoInfo { size, ..Default::default() }),
            mime::AUDIO => AttachmentInfo::Audio(BaseAudioInfo { size, ..Default::default() }),
            _ => AttachmentInfo::File(BaseFileInfo { size }),
        };
        let config = AttachmentConfig::new().txn_id(txn_id.clone()).info(info);

        let progress = SharedObservable::new(TransmissionProgress::default());
        let mut updates = progress.subscribe();
        let notify = self.notify.clone();
        let (progress_room, progress_txn) = (room_id.clone(), txn_id.clone());
        let reporter = tokio::spawn(async move {
            let mut last = None;
            while let Some(p) = updates.next().await {
                if p.total == 0 {
                    continue;
                }
                let percent = (p.current.min(p.total) * 100 / p.total) as u8;
                if last != Some(percent) {
                    last = Some(percent);
                    notify.send(AppEvent::UploadProgress {
                        account: notify.name.clone(),
                        room_id: progress_room.clone(),
                        txn_id: progress_txn.clone(),
                        percent,
                    });
                }
            }
        });
        let sent = room.send_attachment(filename, &mime, data, config).with_send_progress_observable(progress).await;
        reporter.abort();
        Ok(sent?.event_id)
    }

    /// Fetch an image (preferably a thumbnail) and encode it for the
    /// terminal to fit in `cells`.
    pub fn fetch_preview(&self, event_id: OwnedEventId, media: Media, picker: Picker, cells: Size) {
        let acct = self.clone();
        tokio::spawn(async move {
            let protocol = match acct.load_preview(&media, picker, cells).await {
                Ok(protocol) => Some(protocol),
                Err(e) => {
                    tracing::debug!("no preview for {event_id}: {e:#}");
                    None
                }
            };
            acct.notify.send(AppEvent::ImagePreview { event_id, protocol });
        });
    }

    async fn load_preview(&self, media: &Media, picker: Picker, cells: Size) -> Result<Protocol> {
        const MAX_FULL_DOWNLOAD: u64 = 20 * 1024 * 1024;
        let font = picker.font_size();
        let width = u32::from(cells.width) * u32::from(font.width);
        let height = u32::from(cells.height) * u32::from(font.height);
        let media_api = self.client.media();
        let file = |source: &MediaSource| MediaRequestParameters { source: source.clone(), format: MediaFormat::File };
        let full = async {
            if media.size.is_some_and(|s| s > MAX_FULL_DOWNLOAD) {
                bail!("{} is too large to preview", media.filename);
            }
            Ok(media_api.get_media_content(&file(&media.source), true).await?)
        };
        let bytes = match (&media.thumbnail, &media.source) {
            // Encrypted images can only have sender-made thumbnails.
            (Some(thumbnail), _) => media_api.get_media_content(&file(thumbnail), true).await?,
            (None, MediaSource::Plain(_)) => {
                let settings = MediaThumbnailSettings::new(UInt::from(width.max(96)), UInt::from(height.max(96)));
                let request = MediaRequestParameters { source: media.source.clone(), format: MediaFormat::Thumbnail(settings) };
                match media_api.get_media_content(&request, true).await {
                    Ok(bytes) => bytes,
                    Err(_) => full.await?,
                }
            }
            (None, _) => full.await?,
        };
        encode_image(bytes, picker, cells).await
    }

    /// A small avatar for the window list, from a server thumbnail.
    pub fn fetch_avatar(&self, room_id: OwnedRoomId, url: OwnedMxcUri, picker: Picker, cells: Size) {
        let acct = self.clone();
        tokio::spawn(async move {
            let settings = MediaThumbnailSettings::new(UInt::from(64u32), UInt::from(64u32));
            let request =
                MediaRequestParameters { source: MediaSource::Plain(url.clone()), format: MediaFormat::Thumbnail(settings) };
            let protocol = match acct.client.media().get_media_content(&request, true).await {
                Ok(bytes) => encode_image(bytes, picker, cells).await.ok(),
                Err(e) => {
                    tracing::debug!("no avatar for {room_id}: {e}");
                    None
                }
            };
            acct.notify.send(AppEvent::Avatar { account: acct.name.clone(), room_id, url, protocol });
        });
    }

    /// Save an attachment into the Downloads folder.
    pub fn download(&self, room_id: OwnedRoomId, media: Media) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let request = MediaRequestParameters { source: media.source, format: MediaFormat::File };
            let data = acct.client.media().get_media_content(&request, true).await?;
            let dir = dirs::download_dir().or_else(dirs::home_dir).context("no Downloads folder")?;
            let (path, mut file) = create_unique(&dir, &safe_filename(&media.filename))?;
            use std::io::Write;
            file.write_all(&data).with_context(|| format!("writing {}", path.display()))?;
            acct.notify.line(
                Some(room_id),
                Line::info(format!("saved {} ({}) to {}", media.filename, human_size(data.len() as u64), path.display())),
            );
            Ok(())
        });
    }

    pub fn react(&self, room_id: OwnedRoomId, event_id: OwnedEventId, key: String) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let content = ReactionEventContent::new(Annotation::new(event_id, key));
            acct.room(&room_id)?.send(content).await?;
            Ok(())
        });
    }

    pub fn redact(&self, room_id: OwnedRoomId, event_id: OwnedEventId) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            acct.room(&room_id)?.redact(&event_id, None, None).await?;
            Ok(())
        });
    }

    pub fn paginate(&self, room_id: OwnedRoomId, from: Option<String>, limit: u16) {
        let acct = self.clone();
        tokio::spawn(async move {
            let result: Result<(Vec<Item>, Option<String>)> = async {
                let room = acct.room(&room_id)?;
                let mut options = MessagesOptions::backward().from(from.as_deref());
                options.limit = limit.into();
                options.filter.lazy_load_options = LazyLoadOptions::Enabled { include_redundant_members: false };
                let messages = room.messages(options).await?;
                let mut conv = Converter::new(&room, &acct.user_id);
                for raw in &messages.state {
                    if let Ok(AnyStateEvent::RoomMember(StateEvent::Original(member))) = raw.deserialize()
                        && let Some(nick) = member.content.displayname
                    {
                        conv.seed_nick(member.state_key, nick);
                    }
                }
                let mut items = Vec::new();
                for ev in messages.chunk.iter().rev() {
                    if let Some(item) = conv.convert(ev).await {
                        items.push(item);
                    }
                }
                Ok((items, messages.end))
            }
            .await;
            let account = acct.name.clone();
            match result {
                Ok((items, end)) => {
                    acct.notify.send(AppEvent::History { account, room_id, items, end, failed: false })
                }
                Err(e) => {
                    acct.notify.line(Some(room_id.clone()), Line::error(format!("loading history: {e:#}")));
                    acct.notify.send(AppEvent::History { account, room_id, items: vec![], end: from, failed: true });
                }
            }
        });
    }

    /// `/search -deep`: page back through a room's history (decrypting as
    /// we go — the server can't search encrypted rooms) looking for `needle`.
    pub fn search_history(&self, room_id: OwnedRoomId, needle: String, pages: usize) {
        let acct = self.clone();
        tokio::spawn(async move {
            let result: Result<Vec<Line>> = async {
                let room = acct.room(&room_id)?;
                let mut conv = Converter::new(&room, &acct.user_id);
                let (mut hits, mut from) = (Vec::new(), None::<String>);
                for _ in 0..pages {
                    let mut options = MessagesOptions::backward().from(from.as_deref());
                    options.limit = UInt::from(100u32);
                    let messages = room.messages(options).await?;
                    for ev in &messages.chunk {
                        if let Some(Item::Line(line)) = conv.convert(ev).await
                            && line.is_message()
                            && line.text().to_lowercase().contains(&needle)
                        {
                            hits.push(line);
                        }
                    }
                    match messages.end {
                        Some(end) => from = Some(end),
                        None => break,
                    }
                }
                Ok(hits)
            }
            .await;
            let hits = result.unwrap_or_else(|e| {
                tracing::debug!("search in {room_id} failed: {e:#}");
                Vec::new()
            });
            acct.notify.send(AppEvent::SearchResults { account: acct.name.clone(), room_id, hits });
        });
    }

    /// Load a thread for its window: the root, then up to 500 replies.
    pub fn load_thread(&self, room_id: OwnedRoomId, root: OwnedEventId) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let room = acct.room(&room_id)?;
            let mut conv = Converter::new(&room, &acct.user_id);
            let mut items = Vec::new();
            if let Ok(ev) = room.event(&root, None).await
                && let Some(item) = conv.convert(&ev).await
            {
                items.push(item);
            }
            let mut replies = Vec::new();
            let mut from = None;
            for _ in 0..5 {
                let options = RelationsOptions {
                    from,
                    dir: Direction::Backward,
                    limit: Some(UInt::from(100u32)),
                    include_relations: IncludeRelations::RelationsOfType(RelationType::Thread),
                    ..Default::default()
                };
                let page = room.relations(root.clone(), options).await?;
                for ev in &page.chunk {
                    if let Some(item) = conv.convert(ev).await {
                        replies.push(item);
                    }
                }
                match page.next_batch_token {
                    Some(token) => from = Some(token),
                    None => break,
                }
            }
            // Pages come newest first.
            items.extend(replies.into_iter().rev());
            acct.notify.send(AppEvent::Thread { account: acct.name.clone(), room_id, root, items });
            Ok(())
        });
    }

    /// `/threads`: ask the server for the room's threads.
    pub fn list_threads(&self, room_id: OwnedRoomId) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let room = acct.room(&room_id)?;
            let mut request = get_threads::v1::Request::new(room_id.clone());
            request.limit = Some(UInt::from(20u32));
            let response = acct.client.send(request).await?;
            let mut conv = Converter::new(&room, &acct.user_id);
            let mut roots = Vec::new();
            for raw in response.chunk {
                let Ok(Some(event_id)) = raw.get_field::<OwnedEventId>("event_id") else { continue };
                // Fetching each root decrypts it.
                if let Ok(ev) = room.event(&event_id, None).await
                    && let Some(Item::Line(line)) = conv.convert(&ev).await
                {
                    roots.push(line);
                }
            }
            acct.notify.send(AppEvent::Threads { account: acct.name.clone(), room_id, roots });
            Ok(())
        });
    }

    /// Fetch events again after their keys arrived.
    pub fn redecrypt(&self, room_id: OwnedRoomId, event_ids: Vec<OwnedEventId>) {
        let acct = self.clone();
        tokio::spawn(async move {
            let Ok(room) = acct.room(&room_id) else { return };
            let mut conv = Converter::new(&room, &acct.user_id);
            for event_id in event_ids {
                let Ok(ev) = room.event(&event_id, None).await else { continue };
                if let Some(item) = conv.convert(&ev).await {
                    acct.notify.send(AppEvent::Redecrypted { account: acct.name.clone(), room_id: room_id.clone(), item });
                }
            }
        });
    }

    pub fn join(&self, target: String) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            let id = <&RoomOrAliasId>::try_from(target.as_str()).context("expected #alias:server or !room:server")?;
            let room = acct.client.join_room_by_id_or_alias(id, &[]).await?;
            acct.announce_and_focus(&room).await;
            Ok(())
        });
    }

    /// `/away [message]`: unavailable with a status message, or back online.
    pub fn set_away(&self, message: Option<String>) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            let state = if message.is_some() { PresenceState::Unavailable } else { PresenceState::Online };
            let mut request = set_presence::v3::Request::new(acct.user_id.clone(), state);
            request.status_msg = message.clone();
            acct.client.send(request).await?;
            acct.away.store(message.is_some(), Ordering::Relaxed);
            acct.notify.send(AppEvent::Away { account: acct.name.clone(), message });
            Ok(())
        });
    }

    /// Hide a user everywhere (server-side ignore list), or undo it.
    pub fn ignore(&self, out: Option<OwnedRoomId>, user: OwnedUserId, ignore: bool) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            if ignore {
                acct.client.account().ignore_user(&user).await?;
                acct.notify.send(AppEvent::Ignored { account: acct.name.clone(), user_id: user.clone() });
                acct.notify.line(out, Line::info(format!("ignoring {user}")));
            } else {
                acct.client.account().unignore_user(&user).await?;
                acct.notify.line(out, Line::info(format!("no longer ignoring {user}")));
            }
            Ok(())
        });
    }

    pub fn list_ignored(&self, out: Option<OwnedRoomId>) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            let ignored = match acct.client.account().account_data::<IgnoredUserListEventContent>().await? {
                Some(raw) => raw.deserialize()?.ignored_users.into_keys().collect(),
                None => Vec::new(),
            };
            if ignored.is_empty() {
                acct.notify.line(out.clone(), Line::info("you are not ignoring anyone"));
            }
            for user in ignored {
                acct.notify.line(out.clone(), Line::info(format!("ignoring {user}")));
            }
            Ok(())
        });
    }

    /// `/list [search]`: the server's public room directory.
    pub fn list_public(&self, out: Option<OwnedRoomId>, search: Option<String>) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            let mut request = get_public_rooms_filtered::v3::Request::new();
            request.limit = Some(UInt::from(50u32));
            request.filter = DirectoryFilter::new();
            request.filter.generic_search_term = search.clone();
            let response = acct.client.public_rooms_filtered(request).await?;
            let what = search.map(|s| format!(" matching \"{s}\"")).unwrap_or_default();
            let total = response.total_room_count_estimate.map(|n| format!(" of ~{n}")).unwrap_or_default();
            acct.notify.line(
                out.clone(),
                Line::info(format!("{}: {} public room(s){what}{total}", acct.name, response.chunk.len())),
            );
            for room in response.chunk {
                let address = room.canonical_alias.map(|a| a.to_string()).unwrap_or_else(|| room.room_id.to_string());
                let name = room.name.unwrap_or_else(|| address.clone());
                let topic = room.topic.map(|t| format!(" — {}", t.lines().next().unwrap_or(""))).unwrap_or_default();
                acct.notify.line(
                    out.clone(),
                    Line::info(format!("  {name} [{address}] {} user(s){topic}", room.num_joined_members)),
                );
            }
            acct.notify.line(out, Line::info("/join <address> to join one"));
            Ok(())
        });
    }

    /// Create a room and switch to it: invite-only and end-to-end encrypted,
    /// or joinable by anyone who has its address when `public`.
    pub fn create_room(&self, name: String, public: bool, space: bool) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            let mut request = create_room::v3::Request::new();
            request.name = Some(name);
            if space {
                let mut creation = CreationContent::new();
                creation.room_type = Some(RoomType::Space);
                request.creation_content = Some(Raw::new(&creation)?);
                request.preset = Some(if public { RoomPreset::PublicChat } else { RoomPreset::PrivateChat });
                let room = acct.client.create_room(request).await?;
                acct.notify.info(format!("created space {}; add rooms with /space <name> add", room.room_id()));
                acct.notify.send(AppEvent::Spaces { account: acct.name.clone(), spaces: joined_spaces(&acct.client).await });
                return Ok(());
            }
            if public {
                request.preset = Some(RoomPreset::PublicChat);
            } else {
                request.preset = Some(RoomPreset::PrivateChat);
                request.initial_state = vec![
                    InitialStateEvent::with_empty_state_key(RoomEncryptionEventContent::with_recommended_defaults())
                        .to_raw_any(),
                ];
            }
            let room = acct.client.create_room(request).await?;
            acct.announce_and_focus(&room).await;
            Ok(())
        });
    }

    /// Put a room into a space (or take it out), as `m.space.child` state
    /// in the space.
    pub fn space_child(&self, out: Option<OwnedRoomId>, space_id: OwnedRoomId, child: OwnedRoomId, add: bool) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            let space = acct.room(&space_id)?;
            // Our own server is in both rooms, so it's a safe way in; an
            // empty `via` removes the child.
            let via: Vec<OwnedServerName> = if add { vec![acct.user_id.server_name().to_owned()] } else { Vec::new() };
            space.send_state_event_for_key(&child, SpaceChildEventContent::new(via)).await?;
            let what = if add { "added to" } else { "removed from" };
            acct.notify.line(out, Line::info(format!("{what} the space")));
            acct.notify.send(AppEvent::Spaces { account: acct.name.clone(), spaces: joined_spaces(&acct.client).await });
            Ok(())
        });
    }

    /// `/space <name>`: everything the space lists, joined or not.
    pub fn space_hierarchy(&self, out: Option<OwnedRoomId>, space_id: OwnedRoomId) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            let mut request = get_hierarchy::v1::Request::new(space_id.clone());
            request.max_depth = Some(UInt::from(2u32));
            request.limit = Some(UInt::from(100u32));
            let response = acct.client.send(request).await?;
            let rooms: Vec<_> = response.rooms.into_iter().filter(|r| r.summary.room_id != space_id).collect();
            acct.notify.line(out.clone(), Line::info(format!("{} room(s) in this space:", rooms.len())));
            for chunk in rooms {
                let s = chunk.summary;
                let joined = acct.client.get_room(&s.room_id).is_some_and(|r| r.state() == RoomState::Joined);
                let address = s.canonical_alias.map(|a| a.to_string()).unwrap_or_else(|| s.room_id.to_string());
                let name = s.name.unwrap_or_else(|| address.clone());
                let kind = if s.room_type == Some(RoomType::Space) { " (space)" } else { "" };
                let state = if joined { "joined" } else { "/join to enter" };
                acct.notify.line(
                    out.clone(),
                    Line::info(format!("  {name}{kind} [{address}] {} member(s), {state}", s.num_joined_members)),
                );
            }
            Ok(())
        });
    }

    pub fn rename(&self, room_id: OwnedRoomId, name: String) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            acct.room(&room_id)?.set_name(name).await?;
            Ok(())
        });
    }

    /// Leave (if still joined) and drop the room from your room list.
    pub fn forget(&self, room_id: OwnedRoomId) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let room = acct.room(&room_id)?;
            let name = room.display_name().await.map(|n| n.to_string()).unwrap_or_else(|_| room_id.to_string());
            if matches!(room.state(), RoomState::Joined | RoomState::Invited | RoomState::Knocked) {
                room.leave().await?;
            }
            room.forget().await?;
            acct.notify.send(AppEvent::RoomGone { account: acct.name.clone(), room_id });
            acct.notify.info(format!("forgot {name}"));
            Ok(())
        });
    }

    /// Reject an invitation.
    pub fn decline(&self, out: Option<OwnedRoomId>, room_id: OwnedRoomId) {
        let acct = self.clone();
        self.spawn_op(out, async move {
            let room = acct.room(&room_id)?;
            if room.state() != RoomState::Invited {
                bail!("there is no pending invitation to {room_id}");
            }
            let name = room.display_name().await.map(|n| n.to_string()).unwrap_or_else(|_| room_id.to_string());
            room.leave().await?;
            acct.notify.info(format!("declined the invitation to {name}"));
            Ok(())
        });
    }

    pub fn list_invites(&self, out: Option<OwnedRoomId>) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            let rooms = acct.client.invited_rooms();
            if rooms.is_empty() {
                acct.notify.line(out, Line::info("no pending invitations"));
                return Ok(());
            }
            for room in rooms {
                let name = room.display_name().await.map(|n| n.to_string()).unwrap_or_default();
                let inviter = match room.invite_details().await {
                    Ok(details) => details.inviter.map(|m| m.name().to_owned()),
                    Err(_) => None,
                };
                acct.notify.line(
                    out.clone(),
                    Line::info(format!(
                        "{name} [{}] from {} — /join or /decline it",
                        room.room_id(),
                        inviter.as_deref().unwrap_or("someone")
                    )),
                );
            }
            Ok(())
        });
    }

    /// Every joined room, including ones whose window is closed.
    pub fn list_rooms(&self, out: Option<OwnedRoomId>) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            let mut rows = Vec::new();
            for room in acct.client.joined_rooms() {
                let meta = room_meta(&room).await;
                let flags = [(meta.encrypted, "E2E"), (meta.is_dm, "DM")]
                    .iter()
                    .filter(|(on, _)| *on)
                    .map(|(_, f)| *f)
                    .collect::<Vec<_>>()
                    .join(",");
                rows.push((
                    meta.name.to_lowercase(),
                    format!("{} [{}] {} member(s) {flags}", meta.name, meta.room_id, room.joined_members_count()),
                ));
            }
            rows.sort();
            acct.notify.line(out.clone(), Line::info(format!("{}: {} joined room(s)", acct.name, rows.len())));
            for (_, row) in rows {
                acct.notify.line(out.clone(), Line::info(format!("  {row}")));
            }
            Ok(())
        });
    }

    pub fn room_info(&self, room_id: OwnedRoomId) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let room = acct.room(&room_id)?;
            let meta = room_meta(&room).await;
            let power = match room.get_member_no_sync(&acct.user_id).await? {
                Some(member) => match member.power_level() {
                    UserPowerLevel::Infinite => "creator".to_owned(),
                    UserPowerLevel::Int(level) => level.to_string(),
                    _ => "?".to_owned(),
                },
                None => "?".to_owned(),
            };
            let aliases: Vec<String> =
                room.canonical_alias().into_iter().chain(room.alt_aliases()).map(|a| a.to_string()).collect();
            let rows = [
                format!("{} [{}]", meta.name, room_id),
                format!(" aliases  : {}", if aliases.is_empty() { "none".into() } else { aliases.join(", ") }),
                format!(" topic    : {}", meta.topic.as_deref().unwrap_or("none")),
                format!(" members  : {} joined, {} invited", room.joined_members_count(), room.invited_members_count()),
                format!(" encrypted: {}", if meta.encrypted { "yes" } else { "no" }),
                format!(
                    " joining  : {}",
                    room.join_rule().map(|r| r.as_str().to_owned()).unwrap_or_else(|| "unknown".into())
                ),
                format!(" version  : {}", room.version().map(|v| v.to_string()).unwrap_or_else(|| "?".into())),
                format!(" your power level: {power}"),
            ];
            for row in rows {
                acct.notify.line(Some(room_id.clone()), Line::info(row));
            }
            Ok(())
        });
    }

    /// Open (or create) the direct-message room with `user`.
    pub fn query(&self, user: OwnedUserId) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            let room = match acct.client.get_dm_room(&user) {
                Some(room) => room,
                None => acct.client.create_dm(&user).await?,
            };
            acct.announce_and_focus(&room).await;
            Ok(())
        });
    }

    async fn announce_and_focus(&self, room: &Room) {
        let meta = room_meta(room).await;
        let room_id = meta.room_id.clone();
        self.notify.send(AppEvent::Rooms { account: self.name.clone(), rooms: vec![meta] });
        self.notify.send(AppEvent::Focus { account: self.name.clone(), room_id });
    }

    pub fn leave(&self, room_id: OwnedRoomId) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            acct.room(&room_id)?.leave().await?;
            acct.notify.send(AppEvent::RoomGone { account: acct.name.clone(), room_id });
            Ok(())
        });
    }

    pub fn set_topic(&self, room_id: OwnedRoomId, topic: String) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            acct.room(&room_id)?.set_room_topic(&topic).await?;
            Ok(())
        });
    }

    pub fn set_nick(&self, nick: String) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            acct.client.account().set_display_name(Some(&nick)).await?;
            acct.notify.send(AppEvent::Profile { account: acct.name.clone(), nick });
            Ok(())
        });
    }

    pub fn moderate(&self, room_id: OwnedRoomId, action: Moderation, user: OwnedUserId, reason: Option<String>) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let room = acct.room(&room_id)?;
            let reason = reason.as_deref();
            match action {
                Moderation::Invite => room.invite_user_by_id(&user).await?,
                Moderation::Kick => room.kick_user(&user, reason).await?,
                Moderation::Ban => room.ban_user(&user, reason).await?,
                Moderation::Unban => room.unban_user(&user, reason).await?,
            }
            Ok(())
        });
    }

    pub fn members(&self, room_id: OwnedRoomId, show: bool) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let room = acct.room(&room_id)?;
            if let Some(me) = room.get_member_no_sync(&acct.user_id).await? {
                let perms = Permissions {
                    can_send: me.can_send_message(MessageLikeEventType::RoomMessage),
                    can_redact_own: me.can_redact_own(),
                    can_redact_other: me.can_redact_other(),
                };
                acct.notify.send(AppEvent::Permissions { account: acct.name.clone(), room_id: room_id.clone(), perms });
            }
            let mut members: Vec<Member> = room
                .members(RoomMemberships::JOIN)
                .await?
                .into_iter()
                .map(|m| {
                    let prefix = match m.power_level() {
                        UserPowerLevel::Infinite => '@',
                        UserPowerLevel::Int(level) if i64::from(level) >= 100 => '@',
                        UserPowerLevel::Int(level) if i64::from(level) >= 50 => '+',
                        _ => ' ',
                    };
                    Member { name: m.name().to_owned(), user_id: m.user_id().to_owned(), prefix }
                })
                .collect();
            members.sort_by_cached_key(|m| (m.prefix != '@', m.prefix != '+', m.name.to_lowercase()));

            // Where everyone has read up to, for rooms small enough to ask.
            let mut receipts = Vec::new();
            if !show && members.len() <= 100 {
                for m in members.iter().filter(|m| m.user_id != acct.user_id) {
                    if let Ok(Some((event_id, _))) =
                        room.load_user_receipt(EventReceiptType::Read, &ReceiptThread::Unthreaded, &m.user_id).await
                    {
                        receipts.push((m.user_id.clone(), m.name.clone(), event_id));
                    }
                }
            }
            acct.notify.send(AppEvent::Members { account: acct.name.clone(), room_id: room_id.clone(), members, show });
            if !receipts.is_empty() {
                acct.notify.send(AppEvent::Receipts { account: acct.name.clone(), room_id, receipts, live: false });
            }
            Ok(())
        });
    }

    pub fn whois(&self, room_id: Option<OwnedRoomId>, user: OwnedUserId) {
        let acct = self.clone();
        self.spawn_op(room_id.clone(), async move {
            let profile = acct.client.account().fetch_user_profile_of(&user).await.ok();
            let display = profile
                .as_ref()
                .and_then(|p| p.get("displayname"))
                .and_then(|v| v.as_str())
                .unwrap_or(user.localpart())
                .to_owned();
            let enc = acct.client.encryption();
            let trust = match enc.get_user_identity(&user).await? {
                Some(identity) if identity.is_verified() => "verified",
                Some(_) => "not verified",
                None => "no cross-signing identity",
            };
            let mut lines = vec![format!("{display} [{user}]")];
            if let Ok(p) = acct.client.send(get_presence::v3::Request::new(user.clone())).await {
                let presence = presence_from(&p.presence, p.status_msg.clone(), p.last_active_ago);
                lines.push(format!(" presence : {}", presence.describe()));
                acct.notify.send(AppEvent::Presence { account: acct.name.clone(), updates: vec![(user.clone(), presence)] });
            }
            lines.push(format!(" identity : {trust}"));
            for device in enc.get_user_devices(&user).await?.devices() {
                lines.push(format!(
                    " device   : {} {} ({})",
                    device.device_id(),
                    device.display_name().unwrap_or(""),
                    if device.is_verified() { "verified" } else { "unverified" }
                ));
            }
            lines.push("End of WHOIS".into());
            for text in lines {
                acct.notify.line(room_id.clone(), Line::info(text));
            }
            Ok(())
        });
    }

    /// Move your read marker (shared with your other clients) and, unless
    /// receipts are off, the receipt other members see.
    pub fn mark_read(&self, room_id: OwnedRoomId, event_id: OwnedEventId, mode: ReceiptMode) {
        let Ok(room) = self.room(&room_id) else { return };
        let receipts = Receipts::new().fully_read_marker(event_id.clone());
        let receipts = match mode {
            ReceiptMode::Public => receipts.public_read_receipt(event_id),
            ReceiptMode::Private => receipts.private_read_receipt(event_id),
            ReceiptMode::Off => receipts,
        };
        tokio::spawn(async move {
            if let Err(e) = room.send_multiple_receipts(receipts).await {
                tracing::debug!("read marker failed: {e}");
            }
        });
    }

    pub fn typing(&self, room_id: OwnedRoomId, typing: bool) {
        let Ok(room) = self.room(&room_id) else { return };
        tokio::spawn(async move {
            if let Err(e) = room.typing_notice(typing).await {
                tracing::debug!("typing notice failed: {e}");
            }
        });
    }

    /// Unlock secret storage with the dotfile's recovery key: imports the
    /// cross-signing keys (verifying this device) and enables key backup.
    pub fn recover(&self) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            let key = acct.env.secret(&acct.name, Secret::RecoveryKey)?.with_context(|| {
                format!("TEXTRIX_{}_RECOVERY_KEY is not set in {}", acct.name.to_uppercase(), acct.env.path().display())
            })?;
            acct.notify.info("unlocking secret storage with the recovery key…");
            acct.client.encryption().recovery().recover(key.expose_secret()).await?;
            drop(key);
            acct.notify.info("recovery complete: this device is verified and key backup is enabled");
            acct.notify.send(AppEvent::Recovered { account: acct.name.clone() });
            acct.report_trust().await;
            Ok(())
        });
    }

    /// `/verify [user]`: ask another session (or user) to verify us.
    pub fn verify_request(&self, user: Option<OwnedUserId>) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            let user = user.unwrap_or_else(|| acct.user_id.clone());
            let identity = acct
                .client
                .encryption()
                .get_user_identity(&user)
                .await?
                .with_context(|| format!("{user} has no cross-signing identity"))?;
            let request = identity.request_verification().await?;
            acct.notify.info(format!("verification requested from {user}; accept it on the other side"));
            acct.verify.lock().await.request = Some(request.clone());
            acct.watch_request(request);
            Ok(())
        });
    }

    pub fn verify_accept(&self) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            let request = acct.verify.lock().await.request.clone().context("no pending verification request")?;
            request.accept().await?;
            acct.notify.info("verification accepted; start emoji verification on the other side");
            acct.watch_request(request);
            Ok(())
        });
    }

    pub fn verify_answer(&self, answer: VerifyAnswer) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            let (sas, request) = {
                let slot = acct.verify.lock().await;
                (slot.sas.clone(), slot.request.clone())
            };
            match (answer, sas, request) {
                (VerifyAnswer::Confirm, Some(sas), _) => sas.confirm().await?,
                (VerifyAnswer::Mismatch, Some(sas), _) => sas.mismatch().await?,
                (VerifyAnswer::Cancel, Some(sas), _) => sas.cancel().await?,
                (VerifyAnswer::Cancel, None, Some(request)) => request.cancel().await?,
                _ => bail!("no verification in progress"),
            }
            Ok(())
        });
    }

    fn watch_request(&self, request: VerificationRequest) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            let mut changes = request.changes();
            while let Some(state) = changes.next().await {
                match state {
                    VerificationRequestState::Ready { .. } if request.we_started() => {
                        if let Some(sas) = request.start_sas().await? {
                            acct.watch_sas(sas);
                            return Ok(());
                        }
                    }
                    VerificationRequestState::Transitioned { verification, .. } => {
                        if let Some(sas) = verification.sas() {
                            acct.watch_sas(sas);
                        } else {
                            acct.notify.error("the other side chose QR verification; pick emoji instead");
                        }
                        return Ok(());
                    }
                    VerificationRequestState::Done => return Ok(()),
                    VerificationRequestState::Cancelled(info) => {
                        acct.notify.info(format!("verification cancelled: {}", info.reason()));
                        return Ok(());
                    }
                    _ => {}
                }
            }
            Ok(())
        });
    }

    fn watch_sas(&self, sas: SasVerification) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            acct.verify.lock().await.sas = Some(sas.clone());
            let mut changes = sas.changes();
            let mut state = Some(sas.state());
            while let Some(current) = state {
                match current {
                    SasState::Started { .. } if !sas.we_started() => sas.accept().await?,
                    SasState::KeysExchanged { emojis, decimals } => {
                        let code = match emojis {
                            Some(emojis) => emojis
                                .emojis
                                .iter()
                                .map(|e| format!("{} {}", e.symbol, e.description))
                                .collect::<Vec<_>>()
                                .join("  "),
                            None => format!("{} {} {}", decimals.0, decimals.1, decimals.2),
                        };
                        acct.notify.info(format!("compare with the other device:  {code}"));
                        acct.notify.info("type /verify confirm if they match, /verify mismatch if not");
                    }
                    SasState::Done { .. } => {
                        acct.notify.info(format!(
                            "verified {} ({})",
                            sas.other_user_id(),
                            sas.other_device().device_id()
                        ));
                        acct.report_trust().await;
                        break;
                    }
                    SasState::Cancelled(info) => {
                        acct.notify.info(format!("verification cancelled: {}", info.reason()));
                        break;
                    }
                    _ => {}
                }
                state = changes.next().await;
            }
            acct.verify.lock().await.sas = None;
            Ok(())
        });
    }

    /// Invalidate the session server-side and delete it locally.
    pub fn logout(&self) {
        let acct = self.clone();
        self.spawn_op(None, async move {
            acct.stop();
            if let Err(e) = acct.client.matrix_auth().logout().await {
                acct.notify.error(format!("server logout failed ({e}); removing the local session anyway"));
            }
            let _ = std::fs::remove_file(acct.dir.join("session.json"));
            let _ = std::fs::remove_dir_all(acct.dir.join("store"));
            acct.notify.send(AppEvent::LoggedOut { account: acct.name.clone() });
            Ok(())
        });
    }
}

/// Decode an untrusted image (capped in size and memory, off the UI
/// thread) and encode it for the terminal to fit `cells`.
async fn encode_image(bytes: Vec<u8>, picker: Picker, cells: Size) -> Result<Protocol> {
    tokio::task::spawn_blocking(move || -> Result<Protocol> {
        let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(12_000);
        limits.max_image_height = Some(12_000);
        limits.max_alloc = Some(256 * 1024 * 1024);
        reader.limits(limits);
        let decoded = reader.decode()?;
        picker.new_protocol(decoded, cells, Resize::Fit(None)).map_err(|e| anyhow::anyhow!("{e}"))
    })
    .await?
}

/// A sender-chosen file name reduced to a plain name: no directories, no
/// control characters, not hidden.
fn safe_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim().trim_start_matches('.');
    if cleaned.is_empty() { "download".into() } else { cleaned.to_owned() }
}

/// Create `dir/name`, or `name (1)`, `name (2)`… without overwriting.
fn create_unique(dir: &Path, name: &str) -> Result<(PathBuf, std::fs::File)> {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_owned(), format!(".{ext}")),
        _ => (name.to_owned(), String::new()),
    };
    for n in 0..1000 {
        let candidate = if n == 0 { name.to_owned() } else { format!("{stem} ({n}){ext}") };
        let path = dir.join(candidate);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
    bail!("too many files named {name} in {}", dir.display())
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

#[derive(Clone, Copy, Debug)]
pub enum Moderation {
    Invite,
    Kick,
    Ban,
    Unban,
}

#[derive(Clone, Copy, Debug)]
pub enum VerifyAnswer {
    Confirm,
    Mismatch,
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_filenames_cannot_escape_downloads() {
        assert_eq!(safe_filename("../../.zshrc"), "zshrc");
        assert_eq!(safe_filename("C:\\evil\\run.bat"), "run.bat");
        assert_eq!(safe_filename("photo\u{1b}[2J.png"), "photo[2J.png");
        assert_eq!(safe_filename(".."), "download");
    }

    #[test]
    fn unique_names_do_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("textrix-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, _) = create_unique(&dir, "cat.png").unwrap();
        let (b, _) = create_unique(&dir, "cat.png").unwrap();
        assert_eq!(a.file_name().unwrap(), "cat.png");
        assert_eq!(b.file_name().unwrap(), "cat (1).png");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sizes_are_human() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
