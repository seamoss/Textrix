//! One logged-in Matrix account per dotfile server. Everything here runs on
//! background tasks and reports back to the UI through [`AppEvent`]s.

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::picture::{Painter, Picture};
use anyhow::{Context, Result, bail};
use eyeball::SharedObservable;
use futures_util::StreamExt;
use matrix_sdk::{
    Client, Room, RoomMemberships, RoomState, TransmissionProgress,
    attachment::{
        AttachmentConfig, AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo, BaseVideoInfo, Thumbnail,
    },
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    deserialized_responses::RawSyncOrStrippedState,
    encryption::{
        BackupDownloadStrategy, EncryptionSettings,
        verification::{SasState, SasVerification, VerificationRequest, VerificationRequestState},
    },
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    notification_settings::{NotificationSettings, RoomNotificationMode},
    room::{IncludeRelations, MessagesOptions, Receipts, RelationsOptions},
    ruma::{
        OwnedDeviceId, OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedServerName, OwnedTransactionId, OwnedUserId,
        RoomOrAliasId, RoomVersionId, UInt,
        api::Direction,
        api::{
            client::{
                directory::get_public_rooms_filtered,
                discovery::get_capabilities,
                filter::{FilterDefinition, LazyLoadOptions},
                presence::{get_presence, set_presence},
                push::set_pushrule,
                room::create_room::{
                    self,
                    v3::{CreationContent, RoomPreset},
                },
                room::{Visibility as DirectoryVisibility, upgrade_room},
                space::get_hierarchy,
                sync::sync_events::v3::Filter,
                threads::get_threads,
                uiaa::{AuthData, AuthType, MatrixUserIdentifier, Password as UiaaPassword, UserIdentifier},
            },
            error::ErrorKind,
        },
        directory::Filter as DirectoryFilter,
        events::relation::RelationType,
        events::{
            AnyStateEvent, AnySyncEphemeralRoomEvent, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            GlobalAccountDataEvent, InitialStateEvent, MessageLikeEventType, StateEvent, StateEventType,
            SyncMessageLikeEvent, SyncStateEvent,
            fully_read::FullyReadEventContent,
            ignored_user_list::IgnoredUserListEventContent,
            key::verification::request::ToDeviceKeyVerificationRequestEvent,
            presence::PresenceEvent,
            push_rules::PushRulesEventContent,
            reaction::ReactionEventContent,
            receipt::{ReceiptThread, ReceiptType as EventReceiptType},
            relation::Annotation,
            room::{
                MediaSource, avatar,
                encryption::RoomEncryptionEventContent,
                guest_access::{GuestAccess, RoomGuestAccessEventContent},
                history_visibility::HistoryVisibility,
                message::RoomMessageEventContent,
                power_levels::UserPowerLevel,
            },
            space::child::SpaceChildEventContent,
        },
        presence::PresenceState,
        push::{Action, HighlightTweakValue, NewPatternedPushRule, NewPushRule, SoundTweakValue, Tweak},
        room::{JoinRule, RoomType},
        serde::Raw,
    },
    sliding_sync::{SlidingSyncList, SlidingSyncListLoadingState, SlidingSyncMode, Version as SlidingVersion},
    sync::RoomUpdates,
};
use ratatui::layout::Size;
use ruma_html::{HtmlSanitizerMode, RemoveReplyFallback};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Mutex, mpsc::UnboundedSender},
    task::JoinHandle,
};

use crate::{
    config::{BellKind, Paths, ReceiptMode, SyncMode, create_private_dir},
    convert::Converter,
    home::Invitation,
    index::{Crawl, Entry, ROOM_CAP, SearchIndex},
    model::{
        AccountName, Item, Line, LineKind, Media, MediaKind, Member, Permissions, Presence, PresenceKind, RoomMeta,
        SpaceInfo,
    },
    servers::{EnvFile, Secret, ServerDef, write_private},
    vault,
};

pub type Tx = UnboundedSender<AppEvent>;

pub enum AppEvent {
    Connected(Account),
    /// Something that may ring the bell (per `bell_for`).
    Bell(BellKind),
    /// Rooms that have their own notification mode (others use the default).
    PushRules {
        account: AccountName,
        modes: HashMap<OwnedRoomId, RoomNotificationMode>,
    },
    ConnectFailed {
        account: AccountName,
        error: String,
    },
    /// A sync succeeded (`first` on the initial one after connecting).
    Synced {
        account: AccountName,
        first: bool,
    },
    SyncError {
        account: AccountName,
        error: String,
    },
    Rooms {
        account: AccountName,
        rooms: Vec<RoomMeta>,
    },
    RoomGone {
        account: AccountName,
        room_id: OwnedRoomId,
    },
    Timeline {
        account: AccountName,
        room_id: OwnedRoomId,
        items: Vec<Item>,
        live: bool,
        /// The server skipped older events: anything shown from the cache
        /// may no longer join up with these.
        limited: bool,
    },
    /// Messages from the local cache at startup, oldest first.
    Cached {
        account: AccountName,
        room_id: OwnedRoomId,
        items: Vec<Item>,
    },
    History {
        account: AccountName,
        room_id: OwnedRoomId,
        items: Vec<Item>,
        end: Option<String>,
        failed: bool,
    },
    Typing {
        account: AccountName,
        room_id: OwnedRoomId,
        nicks: Vec<(OwnedUserId, String)>,
    },
    /// Other members' read receipts: (user, nick, event they read up to).
    /// Where people have read up to: (who, their name, the event, when).
    Receipts {
        account: AccountName,
        room_id: OwnedRoomId,
        receipts: Vec<(OwnedUserId, String, OwnedEventId, Option<chrono::DateTime<chrono::Local>>)>,
        live: bool,
    },
    UploadProgress {
        account: AccountName,
        room_id: OwnedRoomId,
        txn_id: OwnedTransactionId,
        percent: u8,
    },
    Presence {
        account: AccountName,
        updates: Vec<(OwnedUserId, Presence)>,
    },
    Spaces {
        account: AccountName,
        spaces: Vec<SpaceInfo>,
    },
    /// A thread's root and replies (chronological) for its window.
    Thread {
        account: AccountName,
        room_id: OwnedRoomId,
        root: OwnedEventId,
        items: Vec<Item>,
    },
    /// `/threads`: the room's thread roots, newest first.
    /// A newer release than this build: (version, its page). `asked` when
    /// /version check asked, so "you're up to date" is worth saying.
    Update {
        latest: Option<(String, String)>,
        asked: bool,
    },
    /// A link's title (`None`: it has none, or the preview failed), and
    /// its preview picture, drawn for under it.
    LinkTitle {
        url: String,
        title: Option<String>,
        picture: Option<Picture>,
    },
    /// The server doesn't offer link previews.
    LinkTitlesOff {
        account: AccountName,
    },
    /// A terminal attached to the background Textrix, drawing images
    /// with this (if it can).
    Attached {
        picker: Option<ratatui_image::picker::Picker>,
    },
    /// The attached terminal left.
    Detached,
    /// Asked to quit from outside (`textrix --stop`).
    Quit,
    /// `/exec` finished: what the command printed and its exit code, for
    /// the window it ran in (the status window when `into` is `None`), or
    /// to send there.
    Exec {
        into: Option<crate::app::WinKey>,
        send: bool,
        command: String,
        output: Result<(String, Option<i32>), String>,
    },
    /// `/changelog`'s releases from GitHub, for the window it was typed in
    /// (the status window when `into` is `None`).
    Changelog {
        into: Option<crate::app::WinKey>,
        releases: Result<Vec<crate::update::Release>, String>,
    },
    /// The attached terminal's editor (Ctrl+X) finished: what it saved.
    Edited(Result<String, String>),
    /// What the profile pane shows about someone.
    UserInfo {
        account: AccountName,
        user: OwnedUserId,
        info: UserInfo,
    },
    /// Their picture, drawn for the profile pane (`None`: they have none,
    /// or it couldn't be shown).
    UserPicture {
        user: OwnedUserId,
        picture: Option<Picture>,
    },
    /// Messages scheduled with `/later` on this server, from where they're
    /// kept.
    Scheduled {
        account: AccountName,
        items: Vec<crate::model::Scheduled>,
    },
    /// Custom emoji from image packs: yours and the ones you've made
    /// global, and each room's own. (shortcode, picture).
    CustomEmoji {
        account: AccountName,
        global: Vec<(String, OwnedMxcUri)>,
        rooms: HashMap<OwnedRoomId, Vec<(String, OwnedMxcUri)>>,
    },
    /// The textrix on disk changed: its version, if it said.
    Installed {
        version: Option<String>,
    },
    /// Audio finished playing (or couldn't play).
    Played {
        event_id: OwnedEventId,
        result: Result<(), String>,
    },
    /// An image decoded for the viewer.
    FullImage {
        event_id: OwnedEventId,
        image: Result<Arc<image::DynamicImage>, String>,
    },
    /// A view of it, drawn for the terminal.
    ViewerFrame {
        event_id: OwnedEventId,
        view: crate::viewer::View,
        protocol: Result<Picture, String>,
    },
    /// The room's pinned messages, for `/pins`.
    Pins {
        account: AccountName,
        room_id: OwnedRoomId,
        lines: Vec<Line>,
    },
    Threads {
        account: AccountName,
        room_id: OwnedRoomId,
        roots: Vec<Line>,
    },
    /// Matches from `/search -deep` in one room, newest first.
    SearchResults {
        account: AccountName,
        room_id: OwnedRoomId,
        hits: Vec<Line>,
    },
    /// A window-list avatar, encoded for the terminal (`None` on failure).
    Avatar {
        account: AccountName,
        room_id: OwnedRoomId,
        url: OwnedMxcUri,
        protocol: Option<Picture>,
    },
    /// An inline image preview, encoded for the terminal (`None` on failure).
    ImagePreview {
        event_id: OwnedEventId,
        protocol: Option<Picture>,
    },
    /// A tile's picture for the media gallery (`None` on failure).
    Tile {
        event_id: OwnedEventId,
        picture: Option<Picture>,
    },
    /// Every room this account is invited to.
    Invites {
        account: AccountName,
        invites: Vec<Invitation>,
    },
    Members {
        account: AccountName,
        room_id: OwnedRoomId,
        members: Vec<Member>,
        show: bool,
    },
    Permissions {
        account: AccountName,
        room_id: OwnedRoomId,
        perms: Permissions,
    },
    Focus {
        account: AccountName,
        room_id: OwnedRoomId,
    },
    Sent {
        account: AccountName,
        room_id: OwnedRoomId,
        txn_id: OwnedTransactionId,
        event_id: OwnedEventId,
    },
    SendFailed {
        account: AccountName,
        room_id: OwnedRoomId,
        txn_id: OwnedTransactionId,
        error: String,
    },
    KeysReceived {
        account: AccountName,
        room_id: OwnedRoomId,
        sessions: Vec<String>,
    },
    Recovered {
        account: AccountName,
    },
    Redecrypted {
        account: AccountName,
        room_id: OwnedRoomId,
        item: Item,
    },
    Profile {
        account: AccountName,
        nick: String,
    },
    /// Whether this device is cross-signed (trusted by your other sessions).
    Trust {
        account: AccountName,
        verified: bool,
    },
    LoggedOut {
        account: AccountName,
    },
    Away {
        account: AccountName,
        message: Option<String>,
    },
    /// Drop an ignored user's lines from the account's windows.
    Ignored {
        account: AccountName,
        user_id: OwnedUserId,
    },
    /// A line for a room window (or the status window when `room_id` is None).
    Notice {
        account: Option<AccountName>,
        room_id: Option<OwnedRoomId>,
        line: Line,
    },
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
    /// Keep synced messages in the store's event cache (`history_cache`).
    history_cache: bool,
    /// Decrypted messages for `/search` (with an encrypted store only).
    index: Arc<std::sync::Mutex<Option<SearchIndex>>>,
    /// Seals files kept for later (scheduled messages), with an encrypted
    /// store only.
    sealer: Option<Arc<crate::sealed::Sealer>>,
}

/// Settings [`connect`] takes from the config.
#[derive(Clone, Copy, Debug)]
pub struct ConnectOptions {
    pub sync_mode: SyncMode,
    pub history_cache: bool,
    pub encrypt_store: bool,
    pub search_index: bool,
}

/// What the keychain holds for an account with an encrypted store.
#[derive(Serialize, Deserialize)]
struct Vaulted {
    passphrase: String,
    session: SavedSession,
}

/// The keychain item for an account (its directory keeps separate homes
/// apart).
fn vault_account(name: &str, dir: &Path) -> String {
    format!("{name} {}", dir.display())
}

/// Log in (or restore the saved session) for one dotfile server.
///
/// Stores are encrypted at rest when a keychain is available: the store
/// key and the session live in the keychain, the store in `store-enc/`. An
/// older unencrypted `store/` + `session.json` is migrated once by signing
/// in as a new session and carrying the room keys over.
pub async fn connect(def: ServerDef, paths: Paths, env: EnvFile, opts: ConnectOptions, tx: Tx) -> Result<Account> {
    let name: AccountName = def.name.as_str().into();
    let notify = Notifier { name: name.clone(), tx };
    let dir = paths.account_dir(&def.name);
    create_private_dir(&dir)?;
    let legacy_session = dir.join("session.json");
    let legacy_store = dir.join("store");
    let enc_store = dir.join("store-enc");
    let vault_name = vault_account(&def.name, &dir);
    let keychain = vault::available();
    if !keychain && enc_store.exists() {
        bail!("this account's local store is encrypted, but the system keychain can't be reached to unlock it");
    }
    let vaulted = match keychain.then(|| vault::get(&vault_name)).transpose()?.flatten() {
        Some(json) => Some(serde_json::from_str::<Vaulted>(&json).context("reading the keychain item")?),
        None => None,
    };

    let (client, restored) = match vaulted {
        Some(v) if enc_store.exists() => {
            (restore(&def, &enc_store, Some(&v.passphrase), v.session, &notify).await?, true)
        }
        Some(_) => {
            notify.info("the encrypted local store is missing; signing in as a new session");
            vault::delete(&vault_name)?;
            (login_encrypted(&def, &env, &enc_store, &vault_name, &notify).await?, false)
        }
        None => match load_session(&legacy_session)? {
            Some(saved) => {
                let old = restore(&def, &legacy_store, None, saved, &notify).await?;
                if !(opts.encrypt_store && keychain) {
                    (old, true)
                } else {
                    match migrate(old, &def, &env, &dir, &vault_name, &notify).await {
                        Ok(new) => (new, false),
                        Err((old, e)) => {
                            notify
                                .error(format!("couldn't encrypt the local store ({e:#}); using it as it is for now"));
                            (old, true)
                        }
                    }
                }
            }
            None if opts.encrypt_store && keychain => {
                (login_encrypted(&def, &env, &enc_store, &vault_name, &notify).await?, false)
            }
            None => {
                if opts.encrypt_store {
                    notify.info("no system keychain found, so the local store isn't encrypted");
                }
                let client = login(&def, &env, &legacy_store, None, &notify).await?;
                write_private(&legacy_session, &serde_json::to_vec_pretty(&saved_session(&client)?)?)?;
                (client, false)
            }
        },
    };

    let sealer = if keychain && enc_store.exists() { open_sealer(&dir, &vault_name).await } else { None };
    let index = if opts.search_index && keychain && enc_store.exists() {
        open_index(&dir, &vault_name, &notify).await
    } else {
        // Off, or no encrypted store to keep it as safe as: none at all.
        let _ = std::fs::remove_dir_all(dir.join("search-index"));
        None
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
        sync_mode: opts.sync_mode,
        fresh_login: !restored,
        history_cache: opts.history_cache,
        index: Arc::new(std::sync::Mutex::new(index)),
        sealer: sealer.map(Arc::new),
    })
}

/// The account's sealer for files kept for later, unlocked like the index.
async fn open_sealer(dir: &Path, vault_name: &str) -> Option<crate::sealed::Sealer> {
    let item = vault::get(vault_name).ok().flatten()?;
    let passphrase = serde_json::from_str::<Vaulted>(&item).ok()?.passphrase;
    let dir = dir.to_owned();
    let opened = tokio::task::spawn_blocking(move || crate::sealed::Sealer::open(&dir, passphrase.as_bytes())).await;
    match opened {
        Ok(Ok(sealer)) => Some(sealer),
        Ok(Err(e)) => {
            tracing::warn!("sealed files: {e:#}");
            None
        }
        Err(_) => None,
    }
}

/// The account's search index, unlocked with its store's keychain secret.
async fn open_index(dir: &Path, vault_name: &str, notify: &Notifier) -> Option<SearchIndex> {
    let item = vault::get(vault_name).ok().flatten()?;
    let passphrase = serde_json::from_str::<Vaulted>(&item).ok()?.passphrase;
    let dir = dir.join("search-index");
    match tokio::task::spawn_blocking(move || SearchIndex::open(&dir, passphrase.as_bytes())).await {
        Ok(Ok(index)) => Some(index),
        Ok(Err(e)) => {
            notify.error(format!("the search index couldn't be opened: {e:#}"));
            None
        }
        Err(_) => None,
    }
}

async fn restore(
    def: &ServerDef,
    store: &Path,
    passphrase: Option<&str>,
    saved: SavedSession,
    notify: &Notifier,
) -> Result<Client> {
    if !same_user(&def.user, &saved.session.meta.user_id) {
        bail!(
            "saved session is for {} but the dotfile says {}; run /logout {} first",
            saved.session.meta.user_id,
            def.user,
            def.name
        );
    }
    notify.info(format!("restoring session for {} on {}", saved.session.meta.user_id, saved.homeserver));
    let client = client_builder(store, passphrase).homeserver_url(&saved.homeserver).build().await?;
    client.restore_session(saved.session).await?;
    Ok(client)
}

/// Log in with the dotfile's password, into a new store.
async fn login(
    def: &ServerDef,
    env: &EnvFile,
    store: &Path,
    passphrase: Option<&str>,
    notify: &Notifier,
) -> Result<Client> {
    let homeserver = def
        .homeserver_or_server_name()
        .with_context(|| format!("set TEXTRIX_{}_HOMESERVER or use a full @user:server ID", def.name.to_uppercase()))?;
    let password = env
        .secret(&def.name, Secret::Password)?
        .with_context(|| format!("no saved session and TEXTRIX_{}_PASSWORD is not set", def.name.to_uppercase()))?;
    set_aside_orphaned_store(store)?;
    notify.info(format!("logging in to {homeserver} as {}", def.user));
    let login = async {
        let client = client_builder(store, passphrase).server_name_or_homeserver_url(homeserver).build().await?;
        client
            .matrix_auth()
            .login_username(&def.user, password.expose_secret())
            .initial_device_display_name("Textrix")
            .await?;
        anyhow::Ok(client)
    };
    let result = login.await;
    drop(password);
    if result.is_err() {
        // The store was created for a device that never existed.
        let _ = std::fs::remove_dir_all(store);
    }
    result
}

/// Log in into a new encrypted store, keeping its key and the session in
/// the keychain.
async fn login_encrypted(
    def: &ServerDef,
    env: &EnvFile,
    store: &Path,
    vault_name: &str,
    notify: &Notifier,
) -> Result<Client> {
    let passphrase = vault::random_key()?;
    let client = login(def, env, store, Some(&passphrase), notify).await?;
    let vaulted = Vaulted { passphrase: passphrase.to_string(), session: saved_session(&client)? };
    let json = zeroize::Zeroizing::new(serde_json::to_string(&vaulted)?);
    drop(vaulted);
    if let Err(e) = vault::set(vault_name, &json) {
        // Without the key the store could never be opened again.
        let _ = client.matrix_auth().logout().await;
        drop(client);
        let _ = std::fs::remove_dir_all(store);
        return Err(e.context("saving the store key in the keychain"));
    }
    Ok(client)
}

/// Move an unencrypted session to an encrypted store: export its room
/// keys, sign in as a new session, import them, sign the old one out and
/// delete its files. Until the new session exists nothing is changed, and
/// the old client comes back on failure.
async fn migrate(
    old: Client,
    def: &ServerDef,
    env: &EnvFile,
    dir: &Path,
    vault_name: &str,
    notify: &Notifier,
) -> Result<Client, (Client, anyhow::Error)> {
    notify.info("encrypting the local store (once): signing in as a new session and bringing the room keys along");
    let keys_file = dir.join("migrating-keys.txt");
    let enc_store = dir.join("store-enc");
    let prepared = async {
        let passphrase = vault::random_key()?;
        let _ = std::fs::remove_file(&keys_file);
        create_private_file(&keys_file)?;
        old.encryption().export_room_keys(keys_file.clone(), &passphrase, |_| true).await?;
        // Leftovers of an interrupted attempt.
        if enc_store.exists() {
            std::fs::remove_dir_all(&enc_store)?;
        }
        let new = login_encrypted(def, env, &enc_store, vault_name, notify).await?;
        anyhow::Ok((new, passphrase))
    };
    let (new, passphrase) = match prepared.await {
        Ok(ready) => ready,
        Err(e) => {
            let _ = std::fs::remove_file(&keys_file);
            return Err((old, e));
        }
    };

    // From here the new session is the one in use.
    let old_device = old.device_id().map(|d| d.to_string()).unwrap_or_default();
    let keys_moved = match new.encryption().import_room_keys(keys_file.clone(), &passphrase).await {
        Ok(result) => {
            notify.info(format!("carried over {} room keys", result.total_count));
            true
        }
        Err(e) => {
            notify
                .error(format!("couldn't carry the room keys over ({e}); the old store is kept as store.unencrypted"));
            false
        }
    };
    let _ = std::fs::remove_file(&keys_file);
    if let Err(e) = old.matrix_auth().logout().await {
        notify.error(format!("couldn't sign the old session out ({e}); /devices delete {old_device} removes it"));
    }
    drop(old);
    let legacy_store = dir.join("store");
    if keys_moved {
        let _ = std::fs::remove_dir_all(&legacy_store);
    } else {
        let _ = std::fs::rename(&legacy_store, dir.join("store.unencrypted"));
    }
    let _ = std::fs::remove_file(dir.join("session.json"));
    notify.info("the local store is now encrypted, with its key in the system keychain");
    Ok(new)
}

fn saved_session(client: &Client) -> Result<SavedSession> {
    let session = client.matrix_auth().session().context("login returned no session")?;
    Ok(SavedSession { homeserver: client.homeserver().to_string(), session })
}

/// A new file only you can read (never an existing one).
fn create_private_file(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).with_context(|| format!("creating {}", path.display()))
}

fn client_builder(store: &Path, passphrase: Option<&str>) -> matrix_sdk::ClientBuilder {
    Client::builder().sqlite_store(store, passphrase).with_encryption_settings(EncryptionSettings {
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
        pinned: room.pinned_event_ids().unwrap_or_default(),
    }
}

/// What alerts you in a room with this notification mode.
pub fn describe_notify(mode: Option<RoomNotificationMode>, is_dm: bool) -> &'static str {
    match (mode, is_dm) {
        (Some(RoomNotificationMode::AllMessages), _) => "every message (your setting)",
        (Some(RoomNotificationMode::MentionsAndKeywordsOnly), _) => {
            "mentions and highlight words; other messages show quietly (your setting)"
        }
        (Some(RoomNotificationMode::Mute), _) => "muted (your setting)",
        (None, true) => "every message (the default for direct messages)",
        (None, false) => "mentions and highlight words; other messages show as activity (the default)",
    }
}

/// "Title — Site" from a preview's OpenGraph data.
fn link_title(json: &str) -> Option<String> {
    let og: serde_json::Value = serde_json::from_str(json).ok()?;
    let field = |name: &str| {
        og.get(name)?.as_str().map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|s| !s.is_empty())
    };
    let title = field("og:title")?;
    Some(match field("og:site_name").filter(|site| !title.contains(site.as_str())) {
        Some(site) => format!("{title} — {site}"),
        None => title,
    })
}

/// A preview's picture: `og:image`, which the homeserver has copied to
/// its own media (an `mxc://` address).
fn link_image(json: &str) -> Option<OwnedMxcUri> {
    let og: serde_json::Value = serde_json::from_str(json).ok()?;
    og.get("og:image")?.as_str().filter(|u| u.starts_with("mxc://")).map(OwnedMxcUri::from)
}

/// The verification Textrix does: comparing emoji (SAS). The SDK would
/// offer QR codes too, which Textrix can't show or scan.
const EMOJI_ONLY: [matrix_sdk::ruma::events::key::verification::VerificationMethod; 1] =
    [matrix_sdk::ruma::events::key::verification::VerificationMethod::SasV1];

/// Rooms with their own notification mode, for [`AppEvent::PushRules`].
async fn push_state(settings: &NotificationSettings, account: AccountName) -> AppEvent {
    let mut modes = HashMap::new();
    for room in settings.get_rooms_with_user_defined_rules(Some(true)).await {
        if let Ok(room_id) = OwnedRoomId::try_from(room)
            && let Some(mode) = settings.get_user_defined_room_notification_mode(&room_id).await
        {
            modes.insert(room_id, mode);
        }
    }
    AppEvent::PushRules { account, modes }
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

pub fn describe_history(visibility: &HistoryVisibility) -> &'static str {
    match visibility {
        HistoryVisibility::Invited => "members since they were invited",
        HistoryVisibility::Joined => "members since they joined",
        HistoryVisibility::Shared => "all members",
        HistoryVisibility::WorldReadable => "anyone",
        _ => "an unknown set of people",
    }
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
    let last_active =
        last_active_ago.and_then(|ago| chrono::TimeDelta::from_std(ago).ok()).map(|ago| chrono::Local::now() - ago);
    Presence { kind, status, last_active }
}

impl Account {
    /// Announce known rooms and start syncing.
    pub fn start(&self) {
        self.register_verification_handler();
        self.register_push_rules_handler();
        // Before syncing, so everything synced from now on is kept.
        if self.history_cache
            && let Err(e) = self.client.event_cache().subscribe()
        {
            tracing::warn!("event cache: {e}");
        }

        let acct = self.clone();
        self.track(tokio::spawn(async move {
            let mut rooms = Vec::new();
            for room in acct.client.joined_rooms() {
                rooms.push(room_meta(&room).await);
            }
            acct.notify.send(AppEvent::Rooms { account: acct.name.clone(), rooms });
            acct.load_cached_history().await;
            acct.notify
                .send(AppEvent::Spaces { account: acct.name.clone(), spaces: joined_spaces(&acct.client).await });
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

        if self.index.lock().unwrap().is_some() {
            self.track(tokio::spawn(self.clone().crawl_history()));
        }
    }

    /// Stop syncing. The session stays on disk for the next `/connect`.
    pub fn stop(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }

    /// Keep new messages, edits and deletions in the search index.
    pub fn index_items(&self, room_id: &matrix_sdk::ruma::RoomId, items: &[Item]) {
        let mut guard = self.index.lock().unwrap();
        let Some(index) = guard.as_mut() else { return };
        let room = room_id.as_str();
        let (mut add, mut edits, mut remove) = (Vec::new(), Vec::new(), Vec::new());
        for item in items {
            match item {
                Item::Line(l) if l.redacted => remove.extend(l.event_id.as_ref().map(ToString::to_string)),
                Item::Line(l) if l.is_message() && l.kind != LineKind::Utd && !l.pending => {
                    let Some(event) = &l.event_id else { continue };
                    add.push(Entry {
                        room: room.to_owned(),
                        event: event.to_string(),
                        ts: l.ts.timestamp_millis(),
                        sender: l.sender.as_ref().map(ToString::to_string).unwrap_or_default(),
                        nick: l.nick.clone(),
                        body: l.text(),
                    });
                }
                Item::Edit { target, body, .. } => edits.push((target.to_string(), body.clone())),
                Item::Redact { target } => remove.push(target.to_string()),
                _ => {}
            }
        }
        let result = index
            .add(add)
            .and_then(|()| edits.into_iter().try_for_each(|(event, body)| index.edit(&event, body)))
            .and_then(|()| index.remove(remove));
        if let Err(e) = result {
            tracing::warn!("search index: {e:#}");
        }
    }

    /// `/search` in the index: hits (newest first), how many messages it
    /// holds, and whether older history is still being read into it.
    pub fn search_index(&self, matches: impl Fn(&Entry) -> bool, limit: usize) -> Option<(Vec<Entry>, usize, bool)> {
        let guard = self.index.lock().unwrap();
        let index = guard.as_ref()?;
        let rooms = self.client.joined_rooms();
        let reading = rooms.iter().any(|r| !r.is_space() && !index.crawl(r.room_id().as_str()).done);
        Some((index.search(matches, limit), index.len(), reading))
    }

    /// Read rooms' history back into the search index, gently (a page
    /// every few seconds), so `/search` finds older messages too.
    async fn crawl_history(self) {
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            let next = {
                let guard = self.index.lock().unwrap();
                let Some(index) = guard.as_ref() else { return };
                self.client.joined_rooms().into_iter().filter(|r| !r.is_space()).find_map(|room| {
                    let crawl = index.crawl(room.room_id().as_str());
                    (!crawl.done).then_some((room, crawl))
                })
            };
            let Some((room, crawl)) = next else {
                // All read; look again later for rooms joined since.
                tokio::time::sleep(Duration::from_secs(600)).await;
                continue;
            };
            let pause = match self.crawl_page(&room, crawl).await {
                Ok(()) => 3,
                Err(e) => {
                    tracing::debug!("indexing {}: {e:#}", room.room_id());
                    60
                }
            };
            tokio::time::sleep(Duration::from_secs(pause)).await;
        }
    }

    async fn crawl_page(&self, room: &Room, crawl: Crawl) -> Result<()> {
        let mut options = MessagesOptions::backward().from(crawl.from.as_deref());
        options.limit = 100u32.into();
        options.filter.lazy_load_options = LazyLoadOptions::Enabled { include_redundant_members: false };
        let messages = room.messages(options).await?;
        let mut conv = Converter::new(room, &self.user_id);
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
        self.index_items(room.room_id(), &items);
        let room_id = room.room_id().as_str();
        let mut guard = self.index.lock().unwrap();
        let Some(index) = guard.as_mut() else { return Ok(()) };
        let done = messages.end.is_none() || messages.chunk.is_empty() || index.room_count(room_id) >= ROOM_CAP;
        index.set_crawl(room_id, Crawl { from: messages.end, done })
    }

    /// Fill each room's window from the event cache, so there's something
    /// to read before the server answers. Turning the cache off clears it.
    async fn load_cached_history(&self) {
        if !self.history_cache {
            if let Err(e) = self.client.event_cache().clear_all_rooms().await {
                tracing::warn!("clearing the event cache: {e}");
            }
            return;
        }
        for room in self.client.joined_rooms().into_iter().filter(|r| !r.is_space()) {
            let events = match room.event_cache().await {
                Ok((cache, _handles)) => cache.events().await.unwrap_or_default(),
                Err(e) => {
                    tracing::warn!("event cache for {}: {e}", room.room_id());
                    continue;
                }
            };
            if events.is_empty() {
                continue;
            }
            let mut conv = Converter::new(&room, &self.user_id);
            let mut items = Vec::new();
            for ev in &events {
                match conv.convert(ev).await {
                    Some(Item::Line(mut line)) => {
                        line.cached = true;
                        items.push(Item::Line(line));
                    }
                    Some(other) => items.push(other),
                    None => {}
                }
            }
            self.notify.send(AppEvent::Cached {
                account: self.name.clone(),
                room_id: room.room_id().to_owned(),
                items,
            });
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
            let presence =
                if self.away.load(Ordering::Relaxed) { PresenceState::Unavailable } else { PresenceState::Online };
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
                                error: format!(
                                    "session is no longer valid ({e}); run /logout {} and reconnect",
                                    self.name
                                ),
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
            self.notify
                .send(AppEvent::Spaces { account: self.name.clone(), spaces: joined_spaces(&self.client).await });
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
            if !items.is_empty() || update.timeline.limited {
                self.notify.send(AppEvent::Timeline {
                    account: self.name.clone(),
                    room_id: room_id.clone(),
                    items,
                    live,
                    limited: update.timeline.limited,
                });
            }

            for raw in &update.ephemeral {
                match raw.deserialize() {
                    Ok(AnySyncEphemeralRoomEvent::Typing(typing)) => {
                        let mut nicks = Vec::new();
                        for user in typing.content.user_ids.iter().filter(|u| **u != self.user_id) {
                            nicks.push((user.clone(), conv.nick(user).await));
                        }
                        self.notify.send(AppEvent::Typing {
                            account: self.name.clone(),
                            room_id: room_id.clone(),
                            nicks,
                        });
                    }
                    Ok(AnySyncEphemeralRoomEvent::Receipt(receipt)) => {
                        let mut receipts = Vec::new();
                        for (event_id, by_type) in &receipt.content.0 {
                            let Some(users) = by_type.get(&EventReceiptType::Read) else { continue };
                            for (user, r) in users {
                                let main = matches!(r.thread, ReceiptThread::Unthreaded | ReceiptThread::Main);
                                if *user != self.user_id && main {
                                    let ts = r.ts.map(crate::convert::to_local);
                                    receipts.push((user.clone(), conv.nick(user).await, event_id.clone(), ts));
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

        if !rooms.invited.is_empty() {
            self.fetch_invites();
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
            if live {
                self.notify.send(AppEvent::Bell(BellKind::Invites));
            }
        }
    }

    /// Returns whether this device is already trusted.
    async fn after_first_sync(&self) -> bool {
        self.notify.send(push_state(&self.client.notification_settings().await, self.name.clone()).await);
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

    /// Push rules changed (here or in another client): refresh the rooms'
    /// notification modes.
    fn register_push_rules_handler(&self) {
        let (notify, name) = (self.notify.clone(), self.name.clone());
        self.client.add_event_handler(move |_: GlobalAccountDataEvent<PushRulesEventContent>, client: Client| {
            let (notify, name) = (notify.clone(), name.clone());
            async move { notify.send(push_state(&client.notification_settings().await, name).await) }
        });
    }

    /// `/notify`: a room's notification mode (`None` goes back to the
    /// default), stored as push rules so every client follows it.
    pub fn set_notify_mode(&self, out: Option<OwnedRoomId>, room_id: OwnedRoomId, mode: Option<RoomNotificationMode>) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            let settings = acct.client.notification_settings().await;
            match mode {
                Some(mode) => settings.set_room_notification_mode(&room_id, mode).await?,
                None => settings.delete_user_defined_room_rules(&room_id).await?,
            }
            acct.notify.send(push_state(&settings, acct.name.clone()).await);
            let what = match mode {
                Some(RoomNotificationMode::AllMessages) => "every message alerts you here",
                Some(RoomNotificationMode::MentionsAndKeywordsOnly) => {
                    "only mentions and highlight words alert you here; other messages show quietly"
                }
                Some(RoomNotificationMode::Mute) => "muted: nothing here alerts you",
                None => "back to the default notifications here",
            };
            acct.notify.line(out, Line::info(what));
            Ok(())
        });
    }

    /// `/hilight` and `/dehilight`: words that highlight a message in any
    /// room, kept as push rules so your other clients notify too.
    pub fn set_highlight(&self, out: Option<OwnedRoomId>, word: String, on: bool) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            if on {
                // The SDK's own keyword rules only notify; these highlight
                // as well, like a mention.
                let actions = vec![
                    Action::Notify,
                    Action::SetTweak(Tweak::Sound(SoundTweakValue::Default)),
                    Action::SetTweak(Tweak::Highlight(HighlightTweakValue::Yes)),
                ];
                let rule = NewPatternedPushRule::new(word.clone(), word.clone(), actions);
                acct.client.send(set_pushrule::v3::Request::new(NewPushRule::Content(rule))).await?;
                acct.notify.line(out, Line::info(format!("highlighting “{word}” in every room")));
            } else {
                let settings = acct.client.notification_settings().await;
                if !settings.enabled_keywords().await.contains(&word) {
                    bail!("“{word}” isn't a highlight word; /hilight lists them");
                }
                settings.remove_keyword(&word).await?;
                acct.notify.line(out, Line::info(format!("no longer highlighting “{word}”")));
            }
            Ok(())
        });
    }

    pub fn list_highlights(&self, out: Option<OwnedRoomId>) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            let words: Vec<String> =
                acct.client.notification_settings().await.enabled_keywords().await.into_iter().collect();
            let text = if words.is_empty() {
                "no highlight words (mentions of you always highlight); /hilight <word> adds one".to_owned()
            } else {
                format!("highlight words: {}", words.join(", "))
            };
            acct.notify.line(out, Line::info(text));
            Ok(())
        });
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
                notify.send(AppEvent::Bell(BellKind::Verification));
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
            pinned: room.pinned_event_ids().unwrap_or_default(),
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
                        acct.notify
                            .send(AppEvent::Rooms { account: acct.name.clone(), rooms: vec![room_meta(&room).await] });
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
            if result.is_ok() {
                acct.notify.send(AppEvent::Bell(BellKind::Transfers));
            }
            let account = acct.name.clone();
            acct.notify.send(match result {
                Ok(event_id) => AppEvent::Sent { account, room_id, txn_id, event_id },
                Err(e) => AppEvent::SendFailed { account, room_id, txn_id, error: format!("{e:#}") },
            });
        });
    }

    async fn upload_file(
        &self,
        room_id: &OwnedRoomId,
        path: &Path,
        txn_id: &OwnedTransactionId,
    ) -> Result<OwnedEventId> {
        let room = self.room(room_id)?;
        let data = tokio::fs::read(path).await.with_context(|| format!("reading {}", path.display()))?;
        if let Ok(max) = self.client.load_or_fetch_max_upload_size().await
            && data.len() as u64 > u64::from(max)
        {
            bail!(
                "{} is {}, over the server's {} upload limit",
                path.display(),
                human_size(data.len() as u64),
                human_size(max.into())
            );
        }
        let filename = path.file_name().map(|n| n.to_string_lossy().into_owned()).context("not a file path")?;
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let size = Some(UInt::try_from(data.len() as u64).unwrap_or(UInt::MAX));
        let px = |n: Option<u32>| n.map(UInt::from);
        // Size, blurhash and thumbnail, as Element sends them.
        let extras = match mime.type_() {
            mime::IMAGE => {
                let bytes = data.clone();
                let mut extras =
                    tokio::task::spawn_blocking(move || crate::upload::image_extras(&bytes)).await.unwrap_or_default();
                // Too big to decode here: still say its size.
                if extras.width.is_none()
                    && let Ok(dims) = imagesize::blob_size(&data)
                {
                    (extras.width, extras.height) = (u32::try_from(dims.width).ok(), u32::try_from(dims.height).ok());
                }
                extras
            }
            mime::VIDEO => crate::upload::video_extras(path, &data).await,
            _ => crate::upload::Extras::default(),
        };
        let (width, height, blurhash) = (px(extras.width), px(extras.height), extras.blurhash);
        let info = match mime.type_() {
            mime::IMAGE => AttachmentInfo::Image(BaseImageInfo { width, height, size, blurhash, ..Default::default() }),
            mime::VIDEO => {
                AttachmentInfo::Video(BaseVideoInfo { duration: extras.duration, width, height, size, blurhash })
            }
            mime::AUDIO => AttachmentInfo::Audio(BaseAudioInfo { size, ..Default::default() }),
            _ => AttachmentInfo::File(BaseFileInfo { size }),
        };
        let thumbnail = extras.thumbnail.map(|(data, content_type, width, height)| Thumbnail {
            size: UInt::try_from(data.len() as u64).unwrap_or(UInt::MAX),
            data,
            content_type,
            width: UInt::from(width),
            height: UInt::from(height),
        });
        let config = AttachmentConfig::new().txn_id(txn_id.clone()).info(info).thumbnail(thumbnail);

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
    pub fn fetch_preview(&self, event_id: OwnedEventId, media: Media, picker: Painter, cells: Size) {
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

    /// A gallery tile's picture, `cells` at most.
    pub fn fetch_tile(&self, event_id: OwnedEventId, media: Media, picker: Painter, cells: Size) {
        let acct = self.clone();
        tokio::spawn(async move {
            let picture = acct.load_preview(&media, picker, cells).await.ok();
            acct.notify.send(AppEvent::Tile { event_id, picture });
        });
    }

    async fn load_preview(&self, media: &Media, picker: Painter, cells: Size) -> Result<Picture> {
        const MAX_FULL_DOWNLOAD: u64 = 20 * 1024 * 1024;
        let font = picker.picker.font_size();
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
            // A video's picture is its poster frame or nothing.
            (None, _) if media.kind == MediaKind::Video => bail!("{} has no poster frame", media.filename),
            (None, MediaSource::Plain(_)) => {
                let settings = MediaThumbnailSettings::new(UInt::from(width.max(96)), UInt::from(height.max(96)));
                let request =
                    MediaRequestParameters { source: media.source.clone(), format: MediaFormat::Thumbnail(settings) };
                match media_api.get_media_content(&request, true).await {
                    Ok(bytes) => bytes,
                    Err(_) => full.await?,
                }
            }
            (None, _) => full.await?,
        };
        encode_image(bytes, picker, cells).await
    }

    /// `/avatar`: your profile picture (`None` removes it), seen by everyone
    /// who shares a room with you.
    pub fn set_avatar(&self, out: Option<OwnedRoomId>, image: Option<PathBuf>) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            let text = match image {
                Some(path) => {
                    let (mime, data, _) = read_avatar(&path)?;
                    acct.client.account().upload_avatar(&mime, data).await?;
                    format!("your picture is now {}", path.display())
                }
                None => {
                    acct.client.account().set_avatar_url(None).await?;
                    "your picture is removed".to_owned()
                }
            };
            acct.notify.line(out, Line::info(text));
            Ok(())
        });
    }

    /// `/roomavatar`: the room's picture (`None` removes it).
    pub fn set_room_avatar(&self, room_id: OwnedRoomId, image: Option<PathBuf>) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let room = acct.room(&room_id)?;
            let text = match image {
                Some(path) => {
                    let (mime, data, info) = read_avatar(&path)?;
                    room.upload_avatar(&mime, data, Some(info)).await?;
                    format!("the room's picture is now {}", path.display())
                }
                None => {
                    room.remove_avatar().await?;
                    "the room's picture is removed".to_owned()
                }
            };
            acct.notify.line(Some(room_id), Line::info(text));
            Ok(())
        });
    }

    /// The original image for the viewer, decoded in memory only.
    pub fn fetch_full_image(&self, event_id: OwnedEventId, media: Media) {
        const MAX_DOWNLOAD: u64 = 50 * 1024 * 1024;
        let acct = self.clone();
        tokio::spawn(async move {
            let image = async {
                if media.size.is_some_and(|s| s > MAX_DOWNLOAD) {
                    bail!("{} is over 50 MB", media.filename);
                }
                let request = MediaRequestParameters { source: media.source.clone(), format: MediaFormat::File };
                let bytes = acct.client.media().get_media_content(&request, true).await?;
                let image = tokio::task::spawn_blocking(move || decode_image(&bytes)).await??;
                anyhow::Ok(Arc::new(image))
            }
            .await
            .map_err(|e| format!("{e:#}"));
            acct.notify.send(AppEvent::FullImage { event_id, image });
        });
    }

    /// Play audio or video through `player` (the file goes on the end of
    /// the command). The decrypted file is on disk, in a private file in
    /// `dir`: audio only while it plays (aborting the task stops the player
    /// and deletes it); video until Textrix closes, as video players often
    /// hand the file to an app and return straight away.
    pub fn play(
        &self,
        event_id: OwnedEventId,
        media: Media,
        player: Vec<String>,
        dir: PathBuf,
    ) -> tokio::task::AbortHandle {
        let video = media.kind == MediaKind::Video;
        let limit: u64 = if video { 200 } else { 50 };
        let acct = self.clone();
        tokio::spawn(async move {
            let result = async {
                if media.size.is_some_and(|s| s > limit * 1024 * 1024) {
                    bail!("{} is over {limit} MB", media.filename);
                }
                let request = MediaRequestParameters { source: media.source.clone(), format: MediaFormat::File };
                // Videos stay out of the store's media cache: they're big.
                let data = acct.client.media().get_media_content(&request, !video).await?;
                create_private_dir(&dir)?;
                let stamp =
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
                use std::io::Write;
                let opus = crate::audio::is_ogg_opus(&data);
                // Players go by the extension.
                let fallback = if video { "mp4" } else { "audio" };
                let ext = if opus { "ogg".to_owned() } else { media.extension().unwrap_or_else(|| fallback.to_owned()) };
                let mut file = crate::audio::TempFile(dir.join(format!("play-{stamp}.{ext}")));
                create_private_file(&file.0)?.write_all(&data)?;
                // Element's voice messages are Ogg Opus: most players read it
                // (macOS's own afplay does since macOS 15); for the rest,
                // Textrix decodes it into a WAV.
                if opus && !crate::audio::plays_ogg_opus(&player, &file.0).await {
                    let wav = match tokio::task::spawn_blocking(move || crate::audio::ogg_opus_to_wav(&data)).await {
                        Ok(Ok(wav)) => wav,
                        Ok(Err(e)) => bail!("couldn't decode {}: {e:#}; audio_player = \"mpv\" plays it", media.filename),
                        Err(_) => bail!("couldn't decode {}; audio_player = \"mpv\" plays it", media.filename),
                    };
                    let wav_file = crate::audio::TempFile(file.0.with_extension("wav"));
                    create_private_file(&wav_file.0)?.write_all(&wav)?;
                    file = wav_file;
                }
                let ext = file.0.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
                let (program, args) = player.split_first().context("no player")?;
                let status = tokio::process::Command::new(program)
                    .args(args)
                    .arg(&file.0)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .status()
                    .await
                    .with_context(|| format!("running {program}"))?;
                if video {
                    std::mem::forget(file);
                }
                if !status.success() {
                    let hint = if video {
                        format!(" ({status}); if nothing here plays .{ext} files, video_player in config.toml can name a player, e.g. \"mpv\"")
                    } else {
                        format!(" ({status})")
                    };
                    bail!("{program} couldn't play {}{hint}", media.filename);
                }
                anyhow::Ok(())
            }
            .await
            .map_err(|e| format!("{e:#}"));
            acct.notify.send(AppEvent::Played { event_id, result });
        })
        .abort_handle()
    }

    /// A link's page title (and site), from the homeserver's preview of it.
    /// A link's title from the homeserver's preview, and, given a painter,
    /// its picture (`og:image`, which the homeserver keeps) at `cells`.
    pub fn fetch_link_title(&self, url: String, picture: Option<(Painter, Size)>) {
        let acct = self.clone();
        tokio::spawn(async move {
            let data = match acct.client.media().get_media_preview(&url, None).await {
                Ok(data) => data,
                Err(e) => {
                    // Synapse leaves previews off unless url_preview_enabled.
                    let off = e.as_client_api_error().is_some_and(|e| {
                        e.status_code.as_u16() == 404 || matches!(e.error_kind(), Some(ErrorKind::Unrecognized))
                    });
                    if off {
                        acct.notify.send(AppEvent::LinkTitlesOff { account: acct.name.clone() });
                    }
                    tracing::debug!("no preview for {url}: {e}");
                    None
                }
            };
            let title = data.as_ref().and_then(|raw| link_title(raw.get()));
            let image = data.as_ref().and_then(|raw| link_image(raw.get())).filter(|_| title.is_some());
            let picture = match (image, picture) {
                (Some(mxc), Some((painter, cells))) => {
                    let settings = MediaThumbnailSettings::new(UInt::from(320u32), UInt::from(240u32));
                    let request = MediaRequestParameters {
                        source: MediaSource::Plain(mxc),
                        format: MediaFormat::Thumbnail(settings),
                    };
                    match acct.client.media().get_media_content(&request, true).await {
                        Ok(bytes) => encode_image(bytes, painter, cells).await.ok(),
                        Err(e) => {
                            tracing::debug!("no preview picture for {url}: {e}");
                            None
                        }
                    }
                }
                _ => None,
            };
            acct.notify.send(AppEvent::LinkTitle { url, title, picture });
        });
    }

    /// A small avatar for the window list, from a server thumbnail.
    pub fn fetch_avatar(&self, room_id: OwnedRoomId, url: OwnedMxcUri, picker: Painter, cells: Size) {
        let acct = self.clone();
        tokio::spawn(async move {
            let settings = MediaThumbnailSettings::new(UInt::from(64u32), UInt::from(64u32));
            let request = MediaRequestParameters {
                source: MediaSource::Plain(url.clone()),
                format: MediaFormat::Thumbnail(settings),
            };
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
                Line::info(format!(
                    "saved {} ({}) to {}",
                    media.filename,
                    human_size(data.len() as u64),
                    path.display()
                )),
            );
            acct.notify.send(AppEvent::Bell(BellKind::Transfers));
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

    /// Start a poll, in Element's events so Element shows it: one answer
    /// each, results visible while it's open.
    pub fn start_poll(&self, room_id: OwnedRoomId, question: String, answers: Vec<String>) {
        use matrix_sdk::ruma::events::poll::{
            start::PollKind,
            unstable_start::{
                NewUnstablePollStartEventContent, UnstablePollAnswer, UnstablePollAnswers,
                UnstablePollStartContentBlock,
            },
        };
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let options: Vec<UnstablePollAnswer> = answers
                .iter()
                .enumerate()
                .map(|(n, text)| UnstablePollAnswer::new(format!("a{}", n + 1), text))
                .collect();
            let options = UnstablePollAnswers::try_from(options).map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut block = UnstablePollStartContentBlock::new(question.clone(), options);
            block.kind = PollKind::Disclosed;
            let numbered: Vec<String> = answers.iter().enumerate().map(|(n, a)| format!("{}. {a}", n + 1)).collect();
            let fallback = format!("{question}\n{}", numbered.join("\n"));
            let content = NewUnstablePollStartEventContent::plain_text(fallback, block);
            acct.room(&room_id)?.send(content).await?;
            Ok(())
        });
    }

    /// Whether scheduled messages can be kept on disk (sealed), not only
    /// until Textrix closes.
    pub fn keeps_later(&self) -> bool {
        self.sealer.is_some()
    }

    /// Read the messages scheduled with `/later` back from disk.
    pub fn load_later(&self) {
        let Some(sealer) = self.sealer.clone() else { return };
        let (path, name, notify) = (self.dir.join("later"), self.name.clone(), self.notify.clone());
        tokio::task::spawn_blocking(move || {
            let items: Vec<crate::model::Scheduled> =
                sealer.read(&path).and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
            if !items.is_empty() {
                notify.send(AppEvent::Scheduled { account: name, items });
            }
        });
    }

    /// Keep `items` (this server's scheduled messages) on disk, sealed.
    pub fn save_later(&self, items: Vec<crate::model::Scheduled>) {
        let Some(sealer) = self.sealer.clone() else { return };
        let path = self.dir.join("later");
        tokio::task::spawn_blocking(move || {
            let result = if items.is_empty() {
                std::fs::remove_file(&path)
                    .or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
                    .map_err(Into::into)
            } else {
                serde_json::to_vec(&items).map_err(anyhow::Error::from).and_then(|bytes| sealer.write(&path, &bytes))
            };
            if let Err(e) = result {
                tracing::warn!("keeping scheduled messages: {e:#}");
            }
        });
    }

    /// The custom emoji in image packs (MSC2545: `im.ponies.*`, and the
    /// stable `m.room.image_pack` / `m.image_pack.rooms`), from what's
    /// stored locally: your own pack, packs you've made global, and each
    /// room's.
    pub fn load_custom_emoji(&self) {
        let acct = self.clone();
        tokio::spawn(async move {
            let account = acct.client.account();
            let data = |kind: &'static str| {
                let account = account.clone();
                async move {
                    let raw = account.account_data_raw(kind.into()).await.ok().flatten()?;
                    serde_json::from_str::<serde_json::Value>(raw.json().get()).ok()
                }
            };
            let mut global = data("im.ponies.user_emotes").await.map(|c| pack_emoji(&c)).unwrap_or_default();
            let mut rooms: HashMap<OwnedRoomId, Vec<(String, OwnedMxcUri)>> = HashMap::new();
            for room in acct.client.joined_rooms() {
                let mut emoji = Vec::new();
                for kind in ["im.ponies.room_emotes", "m.room.image_pack"] {
                    for state in room.get_state_events(kind.into()).await.unwrap_or_default() {
                        let matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState::Sync(raw) = state else {
                            continue;
                        };
                        if let Ok(event) = serde_json::from_str::<serde_json::Value>(raw.json().get()) {
                            emoji.extend(pack_emoji(&event["content"]));
                        }
                    }
                }
                if !emoji.is_empty() {
                    rooms.insert(room.room_id().to_owned(), emoji);
                }
            }
            // Rooms whose packs you've made global.
            for kind in ["im.ponies.emote_rooms", "m.image_pack.rooms"] {
                let Some(content) = data(kind).await else { continue };
                let listed =
                    content["rooms"].as_object().map(|r| r.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
                for room_id in listed {
                    if let Some(emoji) = OwnedRoomId::try_from(room_id).ok().and_then(|id| rooms.get(&id)) {
                        global.extend(emoji.iter().cloned());
                    }
                }
            }
            if !global.is_empty() || !rooms.is_empty() {
                acct.notify.send(AppEvent::CustomEmoji { account: acct.name.clone(), global, rooms });
            }
        });
    }

    /// React with a custom emoji: its picture is the key, and its
    /// shortcode goes along for clients to show.
    pub fn react_custom(&self, room_id: OwnedRoomId, event_id: OwnedEventId, picture: OwnedMxcUri, shortcode: String) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let content = serde_json::json!({
                "m.relates_to": { "rel_type": "m.annotation", "event_id": event_id, "key": picture },
                "shortcode": shortcode,
                "com.beeper.reaction.shortcode": shortcode,
            });
            acct.room(&room_id)?.send_raw("m.reaction", content).await?;
            Ok(())
        });
    }

    /// Vote in the poll `poll` starts, in the events it was started with.
    pub fn vote(&self, room_id: OwnedRoomId, poll: OwnedEventId, answers: Vec<String>, stable: bool) {
        use matrix_sdk::ruma::events::poll::{
            response::PollResponseEventContent, unstable_response::UnstablePollResponseEventContent,
        };
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let room = acct.room(&room_id)?;
            if stable {
                room.send(PollResponseEventContent::new(answers.into(), poll)).await?;
            } else {
                room.send(UnstablePollResponseEventContent::new(answers, poll)).await?;
            }
            Ok(())
        });
    }

    /// End the poll `poll` starts (yours), saying how it came out.
    pub fn end_poll(&self, room_id: OwnedRoomId, poll: OwnedEventId, text: String, stable: bool) {
        use matrix_sdk::ruma::events::poll::{end::PollEndEventContent, unstable_end::UnstablePollEndEventContent};
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let room = acct.room(&room_id)?;
            if stable {
                room.send(PollEndEventContent::with_plain_text(text, poll)).await?;
            } else {
                room.send(UnstablePollEndEventContent::new(text, poll)).await?;
            }
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
                Ok((items, end)) => acct.notify.send(AppEvent::History { account, room_id, items, end, failed: false }),
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

    /// `/devices`: every session signed in to this account, most recently
    /// active first, with whether it's verified.
    pub fn list_devices(&self, out: Option<OwnedRoomId>) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            let mut devices = acct.client.devices().await?.devices;
            devices.sort_by_key(|d| std::cmp::Reverse(d.last_seen_ts));
            let keys = acct.client.encryption().get_user_devices(&acct.user_id).await?;
            let this = acct.client.device_id().map(ToOwned::to_owned);
            let mut rows = vec![format!("{} session(s) for {}:", devices.len(), acct.user_id)];
            for d in &devices {
                let trust = match keys.get(&d.device_id) {
                    Some(device) if device.is_verified() => "verified",
                    Some(_) => "unverified",
                    None => "no keys",
                };
                let seen = match d.last_seen_ts {
                    Some(ts) => {
                        format!("active {}", crate::model::ago(chrono::Local::now() - crate::convert::to_local(ts)))
                    }
                    None => "never active".to_owned(),
                };
                let from = d.last_seen_ip.as_deref().map(|ip| format!(" from {ip}")).unwrap_or_default();
                let name: String = d.display_name.as_deref().unwrap_or("(no name)").chars().take(28).collect();
                let marker = if this.as_ref() == Some(&d.device_id) { "  ← this session" } else { "" };
                rows.push(format!("  {:<12} {name:<28} {trust:<10} {seen}{from}{marker}", d.device_id));
            }
            rows.push("/devices rename <id> <name>; /devices delete <id> signs one out".into());
            for row in rows {
                acct.notify.line(out.clone(), Line::info(row));
            }
            Ok(())
        });
    }

    pub fn rename_device(&self, out: Option<OwnedRoomId>, device_id: OwnedDeviceId, name: String) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            acct.client.rename_device(&device_id, &name).await?;
            acct.notify.line(out, Line::info(format!("session {device_id} is now called “{name}”")));
            Ok(())
        });
    }

    /// Sign out another session: shown first, removed only when confirmed.
    /// The server asks for your password again; like at login, it comes
    /// from the dotfile and goes only into the request.
    pub fn delete_device(&self, out: Option<OwnedRoomId>, device_id: OwnedDeviceId, confirmed: bool) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            if acct.client.device_id() == Some(&*device_id) {
                bail!("that's this session; /logout {} signs it out", acct.name);
            }
            let devices = acct.client.devices().await?.devices;
            let Some(device) = devices.iter().find(|d| d.device_id == device_id) else {
                bail!("no session {device_id}; /devices lists them (IDs are case-sensitive)");
            };
            let name = device.display_name.clone().unwrap_or_else(|| "(no name)".into());
            if !confirmed {
                acct.notify.line(
                    out,
                    Line::info(format!(
                        "this signs out {device_id} “{name}”: it stops syncing and can't decrypt new messages. \
                         /devices delete {device_id} confirm to go ahead"
                    )),
                );
                return Ok(());
            }
            let ids = [device_id.clone()];
            let session = match acct.client.delete_devices(&ids, None).await {
                Ok(_) => None,
                Err(e) => {
                    let Some(info) = e.as_uiaa_response() else { return Err(e.into()) };
                    if !info.flows.iter().any(|f| f.stages == [AuthType::Password]) {
                        bail!(
                            "{} wants another way of signing in to remove sessions; use Element for this one",
                            acct.homeserver
                        );
                    }
                    Some(info.session.clone())
                }
            };
            if let Some(session) = session {
                let password = acct.env.secret(&acct.name, Secret::Password)?.with_context(|| {
                    format!(
                        "removing a session needs your password: set TEXTRIX_{}_PASSWORD in the dotfile",
                        acct.name.to_uppercase()
                    )
                })?;
                let user = UserIdentifier::Matrix(MatrixUserIdentifier::new(acct.user_id.to_string()));
                let mut auth = UiaaPassword::new(user, password.expose_secret().to_owned());
                drop(password);
                auth.session = session;
                if let Err(e) = acct.client.delete_devices(&ids, Some(AuthData::Password(auth))).await {
                    if e.as_uiaa_response().is_some() {
                        bail!("the server didn't accept the dotfile's password");
                    }
                    return Err(e.into());
                }
            }
            acct.notify.line(out, Line::info(format!("signed out {device_id} “{name}”")));
            Ok(())
        });
    }

    /// Send a copy of a message to another room: its content as sent
    /// (formatting, attachments), minus reply/thread links and mentions so
    /// nobody is pinged again. An edited message goes as its edited text.
    pub fn forward(
        &self,
        from: OwnedRoomId,
        event_id: OwnedEventId,
        edited: Option<String>,
        to: OwnedRoomId,
        label: String,
    ) {
        let acct = self.clone();
        self.spawn_op(Some(from.clone()), async move {
            let source = acct.room(&from)?;
            let content = match edited {
                Some(text) => RoomMessageEventContent::text_plain(text),
                None => {
                    let event = source.event(&event_id, None).await?;
                    let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                        SyncMessageLikeEvent::Original(message),
                    )) = event.raw().deserialize()?
                    else {
                        bail!("only messages can be forwarded");
                    };
                    let mut content = message.content;
                    content.sanitize(HtmlSanitizerMode::Compat, RemoveReplyFallback::Yes);
                    (content.relates_to, content.mentions) = (None, None);
                    content
                }
            };
            let target = acct.room(&to)?;
            let note = if source.encryption_state().is_encrypted() && !target.encryption_state().is_encrypted() {
                " (that room isn't encrypted)"
            } else {
                ""
            };
            target.send(content).await?;
            acct.notify.line(Some(from), Line::info(format!("forwarded to {label}{note}")));
            Ok(())
        });
    }

    /// `/keys export`: every room key this device holds, encrypted with the
    /// dotfile's passphrase in Element's format, into a new file only you
    /// can read (an existing file is never overwritten).
    pub fn export_keys(&self, out: Option<OwnedRoomId>, path: PathBuf) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            let passphrase = acct.keys_passphrase()?;
            create_private_file(&path)?;
            let mut count = 0;
            let exported = acct
                .client
                .encryption()
                .export_room_keys(path.clone(), passphrase.expose_secret(), |_| {
                    count += 1;
                    true
                })
                .await;
            if let Err(e) = exported {
                let _ = std::fs::remove_file(&path);
                return Err(e.into());
            }
            acct.notify.line(out, Line::info(format!("exported {count} room keys to {}", path.display())));
            Ok(())
        });
    }

    /// `/keys import`: room keys from a file made by `/keys export` or
    /// Element; messages they unlock decrypt as they're shown.
    pub fn import_keys(&self, out: Option<OwnedRoomId>, path: PathBuf) {
        let acct = self.clone();
        self.spawn_op(out.clone(), async move {
            let passphrase = acct.keys_passphrase()?;
            let result = acct
                .client
                .encryption()
                .import_room_keys(path.clone(), passphrase.expose_secret())
                .await
                .with_context(|| {
                    format!("importing {} (is the passphrase the one it was made with?)", path.display())
                })?;
            acct.notify.line(
                out,
                Line::info(format!(
                    "imported {} new room keys of {} in {}",
                    result.imported_count,
                    result.total_count,
                    path.display()
                )),
            );
            Ok(())
        });
    }

    fn keys_passphrase(&self) -> Result<secrecy::SecretString> {
        self.env.secret(&self.name, Secret::KeysPassphrase)?.with_context(|| {
            format!(
                "key files are encrypted with a passphrase: set TEXTRIX_{}_KEYS_PASSPHRASE in {}",
                self.name.to_uppercase(),
                self.env.path().display()
            )
        })
    }

    /// `/pins`: the room's pinned messages, fetched (and decrypted) one by
    /// one, newest pin first.
    pub fn pins(&self, room_id: OwnedRoomId) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let room = acct.room(&room_id)?;
            let mut conv = Converter::new(&room, &acct.user_id);
            let mut lines = Vec::new();
            for event_id in room.pinned_event_ids().unwrap_or_default().into_iter().rev() {
                let line = match room.event(&event_id, None).await {
                    Ok(ev) => match conv.convert(&ev).await {
                        Some(Item::Line(line)) => line,
                        _ => continue,
                    },
                    Err(_) => {
                        let mut line = Line::info("(this message isn't available to you)");
                        line.event_id = Some(event_id);
                        line
                    }
                };
                lines.push(line);
            }
            acct.notify.send(AppEvent::Pins { account: acct.name.clone(), room_id, lines });
            Ok(())
        });
    }

    pub fn set_pinned(&self, room_id: OwnedRoomId, event_id: OwnedEventId, pin: bool) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let room = acct.room(&room_id)?;
            let changed = if pin { room.pin_event(&event_id).await? } else { room.unpin_event(&event_id).await? };
            let text = match (pin, changed) {
                (true, true) => "pinned; /pins lists the room's pinned messages",
                (true, false) => "that message was already pinned",
                (false, true) => "unpinned",
                (false, false) => "that message wasn't pinned",
            };
            acct.notify.line(Some(room_id), Line::info(text));
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
                    acct.notify.send(AppEvent::Redecrypted {
                        account: acct.name.clone(),
                        room_id: room_id.clone(),
                        item,
                    });
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
                acct.notify
                    .send(AppEvent::Spaces { account: acct.name.clone(), spaces: joined_spaces(&acct.client).await });
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
            acct.notify
                .send(AppEvent::Spaces { account: acct.name.clone(), spaces: joined_spaces(&acct.client).await });
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

    pub fn set_join_rule(&self, room_id: OwnedRoomId, rule: JoinRule) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            acct.room(&room_id)?.privacy_settings().update_join_rule(rule).await?;
            acct.notify.line(Some(room_id), Line::info("join rule updated"));
            Ok(())
        });
    }

    pub fn set_guest_access(&self, room_id: OwnedRoomId, allow: bool) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let access = if allow { GuestAccess::CanJoin } else { GuestAccess::Forbidden };
            acct.room(&room_id)?.send_state_event(RoomGuestAccessEventContent::new(access)).await?;
            acct.notify.line(Some(room_id), Line::info(if allow { "guests can join" } else { "guests can't join" }));
            Ok(())
        });
    }

    pub fn set_history_visibility(&self, room_id: OwnedRoomId, visibility: HistoryVisibility) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let text = format!("history is now visible to {}", describe_history(&visibility));
            acct.room(&room_id)?.privacy_settings().update_room_history_visibility(visibility).await?;
            acct.notify.line(Some(room_id), Line::info(text));
            Ok(())
        });
    }

    /// List the room in (or remove it from) the server's room directory.
    pub fn set_published(&self, room_id: OwnedRoomId, publish: bool) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let visibility = if publish { DirectoryVisibility::Public } else { DirectoryVisibility::Private };
            acct.room(&room_id)?.privacy_settings().update_room_visibility(visibility).await?;
            let text = if publish { "listed in the room directory" } else { "removed from the room directory" };
            acct.notify.line(Some(room_id), Line::info(text));
            Ok(())
        });
    }

    /// `/close`: shut a room down as its owner. Unconfirmed, it says what
    /// it would do (or what your power level doesn't allow); confirmed, it
    /// makes the room invite-only, takes it off the directory, removes its
    /// addresses, withdraws invites, removes every other member it can,
    /// then leaves and forgets it. When you're an admin on a Synapse
    /// homeserver, that server also deletes it and blocks it.
    pub fn close_room(&self, room_id: OwnedRoomId, confirmed: bool) {
        let acct = self.clone();
        self.spawn_op(Some(room_id.clone()), async move {
            let room = acct.room(&room_id)?;
            let name = room.display_name().await.map(|n| n.to_string()).unwrap_or_else(|_| room_id.to_string());
            let me = room.get_member(&acct.user_id).await?.context("you aren't in this room")?;
            let level = |p: UserPowerLevel| match p {
                UserPowerLevel::Infinite => i64::MAX,
                UserPowerLevel::Int(n) => n.into(),
                _ => 0,
            };
            let mine = level(me.power_level());
            let everyone = RoomMemberships::JOIN | RoomMemberships::INVITE | RoomMemberships::KNOCK;
            let others: Vec<_> = room.members(everyone).await?.into_iter().filter(|m| m.user_id() != acct.user_id).collect();
            // Only members less powerful than you can be removed.
            let (removable, staying): (Vec<_>, Vec<_>) = others.iter().partition(|m| level(m.power_level()) < mine);
            let invited = removable.iter().filter(|m| *m.membership() == matrix_sdk::ruma::events::room::member::MembershipState::Invite).count();
            let aliases: Vec<matrix_sdk::ruma::OwnedRoomAliasId> = room.canonical_alias().into_iter().chain(room.alt_aliases()).collect();
            let mut missing = Vec::new();
            if !me.can_send_state(StateEventType::RoomJoinRules) {
                missing.push("change who can join");
            }
            if !removable.is_empty() && !me.can_kick() {
                missing.push("remove members");
            }
            if !aliases.is_empty() && !me.can_send_state(StateEventType::RoomCanonicalAlias) {
                missing.push("change its addresses");
            }
            if !missing.is_empty() {
                bail!("you can't close {name}: your power level doesn't let you {}", missing.join(" or "));
            }
            let admin = acct.synapse_admin().await;
            let server = acct.user_id.server_name().to_owned();
            let line = |text: String| acct.notify.line(Some(room_id.clone()), Line::info(text));

            if !confirmed {
                let mut steps = vec!["make it invite-only".to_owned(), "take it off the room directory".to_owned()];
                if !aliases.is_empty() {
                    let list: Vec<String> = aliases.iter().map(ToString::to_string).collect();
                    steps.push(format!("remove its address{} {}", if list.len() == 1 { "" } else { "es" }, list.join(", ")));
                }
                if invited > 0 {
                    steps.push(format!("withdraw {invited} invite{}", if invited == 1 { "" } else { "s" }));
                }
                let members = removable.len() - invited;
                if members > 0 {
                    steps.push(format!("remove {members} member{}", if members == 1 { "" } else { "s" }));
                }
                steps.push("leave and forget it".to_owned());
                if admin {
                    steps.push(format!("then, as an admin on {server}, delete it there and block it from coming back"));
                }
                line(format!("closing {name} will {}. /close confirm does it.", steps.join(", ")));
                if !staying.is_empty() {
                    let names: Vec<String> = staying.iter().map(|m| m.name().to_owned()).collect();
                    line(format!("{} {} as powerful as you, so can't be removed and will keep it", names.join(", "), if names.len() == 1 { "is" } else { "are" }));
                }
                if !admin {
                    line("servers keep their copy of a room until their members leave; only a server's admin can delete it there".to_owned());
                }
                return Ok(());
            }

            line(format!("closing {name}…"));
            room.privacy_settings().update_join_rule(JoinRule::Invite).await?;
            let mut problems = Vec::new();
            if let Err(e) = room.privacy_settings().update_room_visibility(DirectoryVisibility::Private).await {
                problems.push(format!("taking it off the directory: {e}"));
            }
            if !aliases.is_empty() {
                room.privacy_settings().update_canonical_alias(None, Vec::new()).await?;
                // Free the names on your server for other rooms.
                for alias in aliases.iter().filter(|a| a.server_name() == server) {
                    if let Err(e) = room.privacy_settings().remove_room_alias_from_room_directory(alias).await {
                        problems.push(format!("removing {alias}: {e}"));
                    }
                }
            }
            for member in &removable {
                if let Err(e) = room.kick_user(member.user_id(), Some("This room has been closed")).await {
                    problems.push(format!("removing {}: {e}", member.name()));
                }
            }
            room.leave().await?;
            room.forget().await?;
            acct.notify.send(AppEvent::RoomGone { account: acct.name.clone(), room_id: room_id.clone() });
            let purged = if admin {
                match acct.synapse_delete_room(&room_id).await {
                    Ok(()) => format!("; {server} deleted it and blocks it"),
                    Err(e) => {
                        problems.push(format!("deleting it from {server}: {e:#}"));
                        String::new()
                    }
                }
            } else {
                String::new()
            };
            acct.notify.info(format!("closed {name}{purged}"));
            for problem in problems {
                acct.notify.error(format!("closing {name}: {problem}"));
            }
            Ok(())
        });
    }

    /// The homeserver's Synapse admin API, as you (with your sign-in).
    fn admin_request(
        &self,
        method: matrix_sdk::reqwest::Method,
        path: &str,
    ) -> Option<matrix_sdk::reqwest::RequestBuilder> {
        let token = self.client.access_token()?;
        let base = self.client.homeserver().to_string();
        let client = matrix_sdk::reqwest::Client::builder().timeout(Duration::from_secs(30)).build().ok()?;
        Some(client.request(method, format!("{}/_synapse/admin/{path}", base.trim_end_matches('/'))).bearer_auth(token))
    }

    /// Whether you're an admin on your homeserver (Synapse's admin API
    /// answers you; anyone else is turned away).
    async fn synapse_admin(&self) -> bool {
        let path = format!("v1/users/{}/admin", encode_path(self.user_id.as_str()));
        let Some(request) = self.admin_request(matrix_sdk::reqwest::Method::GET, &path) else { return false };
        let Ok(response) = request.send().await else { return false };
        if !response.status().is_success() {
            return false;
        }
        let body = response.text().await.unwrap_or_default();
        serde_json::from_str::<serde_json::Value>(&body).is_ok_and(|v| v["admin"] == serde_json::Value::Bool(true))
    }

    /// Have the homeserver delete a room (its history purged, rejoining
    /// blocked), and wait until it says it's done.
    async fn synapse_delete_room(&self, room_id: &matrix_sdk::ruma::RoomId) -> Result<()> {
        use matrix_sdk::reqwest::Method;
        let path = format!("v2/rooms/{}", encode_path(room_id.as_str()));
        let body = serde_json::json!({ "block": true, "purge": true, "message": "This room has been closed." });
        let request = self.admin_request(Method::DELETE, &path).context("not signed in")?;
        let response = request.body(body.to_string()).header("Content-Type", "application/json").send().await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        anyhow::ensure!(status.is_success(), "{status}: {text}");
        let delete_id = serde_json::from_str::<serde_json::Value>(&text)?["delete_id"].as_str().map(ToOwned::to_owned);
        let Some(delete_id) = delete_id else { return Ok(()) };
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let path = format!("v2/rooms/delete_status/{}", encode_path(&delete_id));
            let Some(request) = self.admin_request(Method::GET, &path) else { break };
            let text = request.send().await?.text().await.unwrap_or_default();
            let status = serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default();
            match status["status"].as_str() {
                Some("complete") => return Ok(()),
                Some("failed") => bail!("{}", status["error"].as_str().unwrap_or("it failed")),
                _ => {}
            }
        }
        bail!("it's still being deleted; the server carries on with it")
    }

    /// `/upgrade`: show versions, or (confirmed) move the room to a new
    /// version and switch to the replacement room.
    pub fn upgrade(&self, room_id: OwnedRoomId, version: Option<String>, confirmed: bool) {
        let acct = self.clone();
        let out = Some(room_id.clone());
        self.spawn_op(out.clone(), async move {
            let room = acct.room(&room_id)?;
            let caps = acct.client.send(get_capabilities::v3::Request::new()).await?.capabilities.room_versions;
            let current = room.version().map(|v| v.to_string()).unwrap_or_else(|| "?".into());
            let Some(target) = version.filter(|_| confirmed) else {
                let mut stable: Vec<String> = caps
                    .available
                    .iter()
                    .filter(|(_, stability)| stability.as_str() == "stable")
                    .map(|(v, _)| v.to_string())
                    .collect();
                stable.sort_by_key(|v| (v.parse::<u32>().unwrap_or(u32::MAX), v.clone()));
                let newest = stable.iter().rev().find(|v| v.parse::<u32>().is_ok()).cloned();
                let mut lines = vec![
                    format!("this room is version {current}; the server's default is {}", caps.default),
                    format!("stable versions: {}", stable.join(", ")),
                ];
                match newest.filter(|n| *n != current) {
                    Some(newest) => {
                        lines.push(
                            "upgrading makes a new room and points this one at it; everyone has to follow to it"
                                .to_owned(),
                        );
                        lines.push(format!("to go ahead: /upgrade {newest} confirm"));
                    }
                    None => lines.push("this room is already on the newest stable version".to_owned()),
                }
                for text in lines {
                    acct.notify.line(out.clone(), Line::info(text));
                }
                return Ok(());
            };
            let new_version = RoomVersionId::try_from(target.as_str())?;
            if !caps.available.contains_key(&new_version) {
                bail!("the server doesn't support room version {new_version}");
            }
            let response =
                acct.client.send(upgrade_room::v3::Request::new(room_id.clone(), new_version.clone())).await?;
            let replacement = response.replacement_room;
            acct.notify.line(out, Line::info(format!("upgraded to version {new_version}: {replacement}")));
            let room = acct.client.join_room_by_id(&replacement).await?;
            acct.announce_and_focus(&room).await;
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

    /// Every pending invitation, for the home window.
    pub fn fetch_invites(&self) {
        let acct = self.clone();
        tokio::spawn(async move {
            let mut invites = Vec::new();
            for room in acct.client.invited_rooms() {
                let name = room.display_name().await.map_or_else(|_| room.room_id().to_string(), |n| n.to_string());
                let inviter = match room.invite_details().await {
                    Ok(details) => details.inviter.map(|m| m.name().to_owned()),
                    Err(_) => None,
                };
                invites.push(Invitation {
                    account: acct.name.to_string(),
                    room_id: room.room_id().to_owned(),
                    name,
                    inviter,
                });
            }
            invites.sort_by_key(|a| a.name.to_lowercase());
            acct.notify.send(AppEvent::Invites { account: acct.name.clone(), invites });
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
            let directory = match room.privacy_settings().get_room_visibility().await {
                Ok(DirectoryVisibility::Public) => "listed",
                Ok(_) => "not listed",
                Err(_) => "unknown",
            };
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
                format!(
                    " guests   : {}",
                    if room.guest_access() == GuestAccess::CanJoin { "can join" } else { "can't join" }
                ),
                format!(" history  : visible to {}", describe_history(&room.history_visibility_or_default())),
                format!(" directory: {directory}"),
                format!(" notify   : {}", describe_notify(room.user_defined_notification_mode().await, meta.is_dm)),
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
                    can_pin: me.can_send_state(StateEventType::RoomPinnedEvents),
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
                    if let Ok(Some((event_id, receipt))) =
                        room.load_user_receipt(EventReceiptType::Read, &ReceiptThread::Unthreaded, &m.user_id).await
                    {
                        let ts = receipt.ts.map(crate::convert::to_local);
                        receipts.push((m.user_id.clone(), m.name.clone(), event_id, ts));
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

    /// Someone's profile, identity and devices, for the profile pane (and
    /// their presence, which updates the dots too).
    pub fn user_info(&self, user: OwnedUserId) {
        let acct = self.clone();
        tokio::spawn(async move {
            let profile = acct.client.account().fetch_user_profile_of(&user).await.ok();
            let field =
                |name: &str| profile.as_ref().and_then(|p| p.get(name)).and_then(|v| v.as_str()).map(str::to_owned);
            let enc = acct.client.encryption();
            let identity = match enc.get_user_identity(&user).await {
                Ok(Some(identity)) => Some(identity.is_verified()),
                _ => None,
            };
            let devices = match enc.get_user_devices(&user).await {
                Ok(devices) => devices
                    .devices()
                    .map(|d| (d.device_id().to_string(), d.display_name().unwrap_or("").to_owned(), d.is_verified()))
                    .collect(),
                Err(_) => Vec::new(),
            };
            if let Ok(p) = acct.client.send(get_presence::v3::Request::new(user.clone())).await {
                let presence = presence_from(&p.presence, p.status_msg.clone(), p.last_active_ago);
                acct.notify
                    .send(AppEvent::Presence { account: acct.name.clone(), updates: vec![(user.clone(), presence)] });
            }
            let info = UserInfo {
                display: field("displayname"),
                avatar: field("avatar_url").filter(|u| u.starts_with("mxc://")).map(OwnedMxcUri::from),
                identity,
                devices,
                fetched: true,
            };
            acct.notify.send(AppEvent::UserInfo { account: acct.name.clone(), user, info });
        });
    }

    /// Someone's picture, drawn at `cells` for the profile pane.
    pub fn fetch_user_picture(&self, user: OwnedUserId, url: OwnedMxcUri, picker: Painter, cells: Size) {
        let acct = self.clone();
        tokio::spawn(async move {
            let settings = MediaThumbnailSettings::new(UInt::from(256u32), UInt::from(256u32));
            let request =
                MediaRequestParameters { source: MediaSource::Plain(url), format: MediaFormat::Thumbnail(settings) };
            let picture = match acct.client.media().get_media_content(&request, true).await {
                Ok(bytes) => encode_image(bytes, picker, cells).await.ok(),
                Err(e) => {
                    tracing::debug!("no picture for {user}: {e}");
                    None
                }
            };
            acct.notify.send(AppEvent::UserPicture { user, picture });
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
                acct.notify
                    .send(AppEvent::Presence { account: acct.name.clone(), updates: vec![(user.clone(), presence)] });
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

    /// `/markread`: read up to `latest`, or to the room's newest event when
    /// nothing of it is loaded.
    pub fn mark_room_read(&self, room_id: OwnedRoomId, latest: Option<OwnedEventId>, mode: ReceiptMode) {
        let acct = self.clone();
        tokio::spawn(async move {
            let latest = match latest {
                Some(id) => Some(id),
                None => {
                    let newest = async {
                        let mut options = MessagesOptions::backward();
                        options.limit = 1u32.into();
                        let messages = acct.room(&room_id).ok()?.messages(options).await.ok()?;
                        messages.chunk.first()?.event_id().map(ToOwned::to_owned)
                    };
                    newest.await
                }
            };
            match latest {
                Some(event_id) => acct.mark_read(room_id, event_id, mode),
                None => tracing::debug!("nothing to mark read in {room_id}"),
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

    /// `/verify [user]`: ask another session (or user) to verify us, by
    /// emoji. (Only emoji: offered QR codes too, Element opens the camera to
    /// scan one, and Textrix shows none.)
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
            let request = identity.request_verification_with_methods(EMOJI_ONLY.to_vec()).await?;
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
            request.accept_with_methods(EMOJI_ONLY.to_vec()).await?;
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
            let _ = std::fs::remove_dir_all(acct.dir.join("store-enc"));
            acct.index.lock().unwrap().take();
            let _ = std::fs::remove_dir_all(acct.dir.join("search-index"));
            if let Err(e) = vault::delete(&vault_account(&acct.name, &acct.dir)) {
                acct.notify.error(format!("{e:#}"));
            }
            acct.notify.send(AppEvent::LoggedOut { account: acct.name.clone() });
            Ok(())
        });
    }
}

/// A Matrix ID or other value as one URL path segment.
fn encode_path(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b => format!("%{b:02X}"),
        })
        .collect()
}

/// Decode an untrusted image (capped in size and memory, off the UI
/// thread) and encode it for the terminal to fit `cells`.
async fn encode_image(bytes: Vec<u8>, painter: Painter, cells: Size) -> Result<Picture> {
    tokio::task::spawn_blocking(move || -> Result<Picture> {
        let decoded = decode_image(&bytes)?;
        painter.fit(decoded, cells)
    })
    .await?
}

/// An image file for an avatar: its type, bytes and size, refused unless
/// it really decodes as an image (and isn't huge).
fn read_avatar(path: &Path) -> Result<(mime::Mime, Vec<u8>, avatar::ImageInfo)> {
    const MAX: u64 = 10 * 1024 * 1024;
    let size = std::fs::metadata(path).with_context(|| format!("{}", path.display()))?.len();
    if size > MAX {
        bail!("{} is over 10 MB; pick a smaller picture", path.display());
    }
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    if mime.type_() != mime::IMAGE {
        bail!("{} isn't an image", path.display());
    }
    let data = std::fs::read(path).with_context(|| format!("{}", path.display()))?;
    let decoded = decode_image(&data).with_context(|| format!("{} doesn't open as an image", path.display()))?;
    let mut info = avatar::ImageInfo::new();
    (info.width, info.height) = (Some(decoded.width().into()), Some(decoded.height().into()));
    (info.mimetype, info.size) = (Some(mime.to_string()), Some(UInt::try_from(size).unwrap_or_default()));
    Ok((mime, data, info))
}

/// Decode an untrusted image with caps on its size and memory (blocking).
pub(crate) fn decode_image(bytes: &[u8]) -> Result<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(12_000);
    limits.max_image_height = Some(12_000);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader.decode()?)
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

/// What the profile pane knows about someone.
#[derive(Clone, Debug, Default)]
pub struct UserInfo {
    pub display: Option<String>,
    pub avatar: Option<OwnedMxcUri>,
    /// Their cross-signing identity: verified by you, or not; `None` when
    /// they have none.
    pub identity: Option<bool>,
    /// (device ID, its name, verified).
    pub devices: Vec<(String, String, bool)>,
    /// The server answered (until then the pane says it's looking).
    pub fetched: bool,
}

/// An image pack's emoji: (shortcode, picture), leaving out images meant
/// only as stickers.
pub fn pack_emoji(content: &serde_json::Value) -> Vec<(String, OwnedMxcUri)> {
    let usage = |v: &serde_json::Value| v["usage"].as_array().map(|u| u.iter().any(|x| x == "emoticon"));
    let pack_ok = usage(&content["pack"]).unwrap_or(true);
    let Some(images) = content["images"].as_object() else { return Vec::new() };
    images
        .iter()
        .filter(|(_, image)| usage(image).unwrap_or(pack_ok))
        .filter_map(|(code, image)| {
            let url = image["url"].as_str().filter(|u| u.starts_with("mxc://"))?;
            Some((code.trim_matches(':').to_owned(), OwnedMxcUri::from(url)))
        })
        .collect()
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
mod image_tests {
    /// The formats chats send all still open (the image crate is built
    /// with only these).
    #[test]
    fn chat_image_formats_decode() {
        let picture = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(8, 6, |x, y| {
            image::Rgb([(x * 30) as u8, (y * 40) as u8, 128])
        }));
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::Gif,
            image::ImageFormat::WebP,
            image::ImageFormat::Bmp,
        ] {
            let mut bytes = std::io::Cursor::new(Vec::new());
            picture.write_to(&mut bytes, format).unwrap();
            let back = super::decode_image(bytes.get_ref()).unwrap_or_else(|e| panic!("{format:?}: {e:#}"));
            assert_eq!((back.width(), back.height()), (8, 6), "{format:?}");
        }
    }
}

#[cfg(test)]
mod pack_tests {
    #[test]
    fn packs_give_their_emoji_not_their_stickers() {
        let pack = serde_json::json!({
            "pack": { "display_name": "Cats" },
            "images": {
                "blobcat": { "url": "mxc://x.org/blob" },
                "bigcat": { "url": "mxc://x.org/big", "usage": ["sticker"] },
                "both": { "url": "mxc://x.org/both", "usage": ["sticker", "emoticon"] },
                "broken": { "url": "https://not.mxc/" }
            }
        });
        let mut emoji: Vec<String> = super::pack_emoji(&pack).into_iter().map(|(code, _)| code).collect();
        emoji.sort();
        assert_eq!(emoji, ["blobcat", "both"]);
        let stickers_only =
            serde_json::json!({ "pack": { "usage": ["sticker"] }, "images": { "a": { "url": "mxc://x/a" } } });
        assert!(super::pack_emoji(&stickers_only).is_empty());
    }
}

#[cfg(test)]
mod avatar_tests {
    use super::read_avatar;

    #[test]
    fn avatars_must_really_be_images() {
        let dir = std::env::temp_dir().join(format!("textrix-avatar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("me.png");
        image::RgbImage::from_pixel(6, 4, image::Rgb([200, 30, 30])).save(&png).unwrap();
        let (mime, data, info) = read_avatar(&png).unwrap();
        assert_eq!(mime.essence_str(), "image/png");
        assert!(!data.is_empty());
        assert_eq!((info.width.map(u64::from), info.height.map(u64::from)), (Some(6), Some(4)));

        let fake = dir.join("fake.png");
        std::fs::write(&fake, "not a picture").unwrap();
        assert!(read_avatar(&fake).unwrap_err().to_string().contains("doesn't open as an image"));
        let text = dir.join("notes.txt");
        std::fs::write(&text, "hello").unwrap();
        assert!(read_avatar(&text).unwrap_err().to_string().contains("isn't an image"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod link_title_tests {
    use super::{link_image, link_title};

    #[test]
    fn titles_come_from_opengraph() {
        assert_eq!(
            link_title(r#"{"og:title": "Rust  (programming\nlanguage)", "og:site_name": "Wikipedia"}"#).as_deref(),
            Some("Rust (programming language) — Wikipedia")
        );
        assert_eq!(
            link_title(r#"{"og:title": "Matrix.org", "og:site_name": "Matrix"}"#).as_deref(),
            Some("Matrix.org")
        );
        assert!(link_title(r#"{"og:description": "no title"}"#).is_none());
        assert!(link_title("not json").is_none());
    }

    #[test]
    fn pictures_are_the_homeservers_copy() {
        assert_eq!(
            link_image(r#"{"og:title": "Docs", "og:image": "mxc://example.org/abc"}"#).map(|u| u.to_string()),
            Some("mxc://example.org/abc".to_owned())
        );
        // Never the site's own address: that would show the site who's looking.
        assert!(link_image(r#"{"og:image": "https://example.org/a.png"}"#).is_none());
        assert!(link_image(r#"{"og:title": "Docs"}"#).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_go_into_paths_encoded() {
        assert_eq!(encode_path("@sam:x.org"), "%40sam%3Ax.org");
        assert_eq!(encode_path("!abc/def:x.org"), "%21abc%2Fdef%3Ax.org");
        assert_eq!(encode_path("delete-id_1.2~"), "delete-id_1.2~");
    }

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
