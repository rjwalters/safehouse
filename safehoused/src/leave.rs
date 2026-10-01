//! Leaving rooms (#201): the `leave` RPC op's shared leave+forget path, and
//! the optional `leave_when_alone` watcher.
//!
//! Two callers, one exit path. [`leave_and_forget`] is what the `leave` socket
//! op (`rpc.rs`) and the watcher below both use, so an operator-driven leave
//! and an automatic one leave the daemon in exactly the same state: not in the
//! room, and not carrying it in the store either (a left-but-remembered room is
//! the #57 stale-entry hazard all over again — it keeps being replayed at boot
//! and stays addressable over RPC).
//!
//! The watcher is deliberately split into a cheap observer and a periodic
//! enforcer:
//!
//! * [`AloneWatch::observe`] only *records* when the daemon first became the
//!   room's last joined member. `main.rs`'s `m.room.member` handler calls it on
//!   every live membership change, and the boot-time pass calls it once per
//!   joined room.
//! * [`sweep_alone_rooms`] is the enforcer, run on a timer. It re-derives
//!   membership from the store before acting, which is what makes the grace
//!   period meaningful: a leave-and-rejoin inside the window clears the
//!   recorded start, so the bot never bounces out behind someone who stepped
//!   away for a minute.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use matrix_sdk::{
    ruma::{
        api::client::membership::leave_room,
        events::tag::{TagName, Tags},
        MilliSecondsSinceUnixEpoch, OwnedUserId, UserId,
    },
    Client, Room, RoomMemberships, RoomState,
};

/// How long a room must have been continuously down to just this daemon before
/// `leave_when_alone` acts on it. Long enough that someone leaving and coming
/// straight back (a client restart, a deliberate re-join to reset a thread)
/// does not push the bot out behind them.
pub const ALONE_GRACE: Duration = Duration::from_secs(600);

/// How often the enforcer re-checks the rooms it is watching. Membership
/// changes are observed live (`main.rs`'s member handler), so this interval
/// only bounds how late a *due* leave happens, not how late it is noticed.
pub const ALONE_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The leave reason recorded on the daemon's own `m.room.member` event when the
/// watcher — not an operator — is what left the room. Anyone who returns to the
/// room's history sees why the bot is gone.
pub const ALONE_LEAVE_REASON: &str = "no other members remained (safehoused leave_when_alone)";

/// Per-room bookkeeping for `leave_when_alone`.
///
/// `since` holds, for each room the daemon is currently alone in, the moment
/// that run of aloneness *started* — not the moment it was noticed. A live
/// membership event contributes its own `origin_server_ts`, and the boot pass
/// contributes the newest leave event in the room, so a daemon restarting into
/// a long-empty room does not restart the grace period from zero.
#[derive(Debug)]
pub struct AloneWatch {
    grace: Duration,
    since: Mutex<HashMap<String, SystemTime>>,
    /// Rooms whose automatic leave was declined (server notices). Tracked only
    /// to log the decision once per room per process instead of every sweep —
    /// the same warn-once shape `Registry::mark_unsupported_surfaced` uses.
    declined: Mutex<HashSet<String>>,
}

impl AloneWatch {
    /// A watcher with the default [`ALONE_GRACE`] window.
    pub fn new() -> Self {
        Self::with_grace(ALONE_GRACE)
    }

    pub fn with_grace(grace: Duration) -> Self {
        Self {
            grace,
            since: Mutex::new(HashMap::new()),
            declined: Mutex::new(HashSet::new()),
        }
    }

    pub fn grace(&self) -> Duration {
        self.grace
    }

    /// Record what is currently true of `room_id`.
    ///
    /// `alone_since` is `Some(t)` when the daemon is the room's last joined
    /// member, where `t` is the best available estimate of when that became
    /// true; `None` when it is not alone. Returns the *stored* start of the
    /// current run — the first estimate seen wins, so repeated observations
    /// never push the deadline out, and a `None` in between resets it.
    pub fn observe(&self, room_id: &str, alone_since: Option<SystemTime>) -> Option<SystemTime> {
        let mut since = self.since.lock().expect("alone-watch mutex poisoned");
        match alone_since {
            Some(candidate) => Some(*since.entry(room_id.to_owned()).or_insert(candidate)),
            None => {
                since.remove(room_id);
                None
            }
        }
    }

    /// The recorded start of `room_id`'s current alone run, if any.
    pub fn alone_since(&self, room_id: &str) -> Option<SystemTime> {
        self.since
            .lock()
            .expect("alone-watch mutex poisoned")
            .get(room_id)
            .copied()
    }

    /// Stop tracking `room_id` (it was left, or is no longer joined).
    pub fn clear(&self, room_id: &str) {
        self.since
            .lock()
            .expect("alone-watch mutex poisoned")
            .remove(room_id);
    }

    /// `true` the first time an automatic leave is declined for `room_id`, so
    /// the reason is logged once rather than once per sweep.
    pub fn mark_declined(&self, room_id: &str) -> bool {
        self.declined
            .lock()
            .expect("alone-watch mutex poisoned")
            .insert(room_id.to_owned())
    }

    /// The recorded alone-run start for `room_id`, but only once it has
    /// outlived the grace period as of `now`. `None` means "not alone, or not
    /// yet due" — the only two states in which the watcher must do nothing.
    pub fn due(&self, room_id: &str, now: SystemTime) -> Option<SystemTime> {
        self.alone_since(room_id)
            .filter(|since| grace_elapsed(*since, now, self.grace))
    }
}

impl Default for AloneWatch {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether an alone run that started at `since` has outlived `grace` as of
/// `now`. A `since` in the future (clock skew between this host and the
/// homeserver stamping membership events) is never elapsed — the conservative
/// answer, since the cost of waiting another sweep is nothing and the cost of
/// leaving early is a bot that walks out of a live room.
pub fn grace_elapsed(since: SystemTime, now: SystemTime, grace: Duration) -> bool {
    now.duration_since(since)
        .is_ok_and(|elapsed| elapsed >= grace)
}

/// Whether the daemon is the room's last member: it is the only joined user and
/// nobody has an invite outstanding.
///
/// The pending-invite clause is not in the issue's wording but is load-bearing:
/// `create_room` can leave the daemon alone in a room it just made for someone
/// who has not accepted yet (and `invite` can produce the same shape in an
/// existing room). Leaving *that* room would delete the thing the operator was
/// in the middle of setting up.
pub fn is_last_member(own_user: &UserId, joined: &[OwnedUserId], pending_invites: usize) -> bool {
    pending_invites == 0 && joined.len() == 1 && joined[0] == own_user
}

/// Whether `tags` carry the spec's `m.server_notice` tag — the mechanism a
/// client is told to identify a server-notices room by.
pub fn tagged_server_notice(tags: Option<&Tags>) -> bool {
    tags.is_some_and(|tags| tags.contains_key(&TagName::ServerNotice))
}

/// Render `at` as an RFC 3339 UTC timestamp (second precision), the format
/// `envelope.rs` already validates for `completion-v1`'s `started_at` /
/// `completed_at`. Hand-rolled because the daemon has no date dependency and
/// one log line does not justify adding one; the civil-from-days arithmetic is
/// the standard (Howard Hinnant) algorithm and is unit-tested below.
pub fn rfc3339_utc(at: SystemTime) -> String {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Days since 1970-01-01 -> (year, month, day). Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A Matrix `origin_server_ts` as a `SystemTime`. The homeserver's clock, not
/// this host's — which is the point: a membership event's own timestamp says
/// when the room emptied, whereas "now" only says when this daemon looked.
pub fn systime_from_ms(ts: MilliSecondsSinceUnixEpoch) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(u64::from(ts.0))
}

/// Leave `room` and then forget it, optionally recording `reason` on the
/// membership event.
///
/// `Room::leave()` in matrix-sdk 0.18 sends no reason, and the reason is
/// exactly what remaining members see in the `m.room.member` event — so when
/// one is given, the reasoned CS-API leave goes out first. `Room::leave()` then
/// drives the store's `Joined -> Left` transition (tolerating the 403 a
/// server that already considers us gone may answer with, see the SDK's
/// `leave_impl`), which is the precondition `Room::forget()` enforces.
pub async fn leave_and_forget(client: &Client, room: &Room, reason: Option<&str>) -> Result<()> {
    if let Some(reason) = reason {
        let mut request = leave_room::v3::Request::new(room.room_id().to_owned());
        request.reason = Some(reason.to_owned());
        client
            .send(request)
            .await
            .with_context(|| format!("leaving {} (with reason)", room.room_id()))?;
    }
    // Skip when a concurrent sync already processed the leave above: `leave()`
    // errors on an already-`Left` room, and all we still need from it is the
    // store transition that has then already happened.
    if room.state() != RoomState::Left {
        room.leave()
            .await
            .with_context(|| format!("leaving {}", room.room_id()))?;
    }
    room.forget()
        .await
        .with_context(|| format!("forgetting {}", room.room_id()))
}

/// When the daemon's current run of aloneness in `room` started, as best the
/// room itself can say: the newest membership event among the users who are
/// *gone* (left or banned). That is the moment the last of them walked out.
///
/// `None` when the room offers no such timestamp (no departed members, or only
/// stripped/invite-state events, which carry none) — the caller then falls back
/// to "now", which merely starts the grace period from this observation.
pub async fn alone_since_from_room(room: &Room) -> Option<SystemTime> {
    let departed = room
        .members_no_sync(RoomMemberships::LEAVE | RoomMemberships::BAN)
        .await
        .ok()?;
    departed
        .iter()
        .filter_map(|member| member.event().timestamp())
        .map(u64::from)
        .max()
        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms))
}

/// Whether the daemon is `room`'s last joined member right now, read from the
/// store. Members are fetched once per room if lazy loading left the list
/// incomplete (`Room::members` is a no-op once synced), so this cannot answer
/// "alone" merely because the member list had not arrived yet.
pub async fn daemon_is_alone(room: &Room, own_user: &UserId) -> Result<bool> {
    let joined: Vec<OwnedUserId> = room
        .members(RoomMemberships::JOIN)
        .await
        .with_context(|| format!("reading joined members of {}", room.room_id()))?
        .iter()
        .map(|member| member.user_id().to_owned())
        .collect();
    let pending_invites = room
        .members_no_sync(RoomMemberships::INVITE)
        .await
        .with_context(|| format!("reading invited members of {}", room.room_id()))?
        .len();
    Ok(is_last_member(own_user, &joined, pending_invites))
}

/// Why a room that is otherwise due to be left must be kept anyway, as a phrase
/// for the log line — `None` meaning "nothing vetoes leaving it".
///
/// Two vetoes, both of which describe rooms where "the daemon is the only
/// member" is a *normal steady state* rather than an abandoned conversation:
///
/// * A **Space** (`m.space`, #27): a container of rooms that carries no
///   messages. The fleet's own "2AM Fleet" Space has exactly one member — this
///   bot — by design, and auto-leaving it would dismantle the hierarchy
///   `list_rooms`/`add_to_space` build on.
/// * A **server-notices room**: the homeserver owns it and it is the only
///   channel an admin has to reach the bot account.
pub fn auto_leave_veto(is_space: bool, is_server_notice: bool) -> Option<&'static str> {
    if is_space {
        Some("Space (m.space) — a container, not a conversation")
    } else if is_server_notice {
        Some("server-notices room")
    } else {
        None
    }
}

/// Whether `room` is a server-notices room, which `leave_when_alone` must never
/// touch: the homeserver owns it, re-creates it at will, and it is the only
/// channel an admin has to reach this account.
///
/// Detection is the spec's own mechanism — the `m.server_notice` room tag the
/// homeserver applies and clients are told to look for. There is no other
/// portable signal: the notices account's user id is per-homeserver
/// configuration, not a well-known localpart.
///
/// The usual shape of a notices room (the daemon plus the notices account, both
/// joined) is already excluded by [`is_last_member`] — two joined members is
/// not alone. The tag covers the case that *is* reachable: a notices account
/// that has left, leaving the daemon alone in a room the server still owns.
///
/// A failed tag read answers `true`. Declining to leave a room we could not
/// classify costs an operator one `leave` op; the opposite mistake costs the
/// account its only admin channel.
pub async fn is_server_notice_room(room: &Room) -> bool {
    match room.tags().await {
        Ok(tags) => tagged_server_notice(tags.as_ref()),
        Err(err) => {
            eprintln!(
                "safehoused: could not read tags of {} ({err:#}) — not auto-leaving a room \
                 that may be a server-notices room",
                room.room_id()
            );
            true
        }
    }
}

/// One pass of the `leave_when_alone` enforcer over every joined room: refresh
/// each room's observation, then leave+forget the ones whose alone run has
/// outlived the grace period.
///
/// Best-effort per room, like `reconcile_left_rooms` and
/// `replay_thread_history`: a membership read or a leave that fails is logged
/// and skipped, never fatal and never retried in a tight loop (the next sweep
/// is the retry).
///
/// Also the boot-time pass (`main.rs` calls it once before the sync loop), so a
/// daemon restarting into a room that emptied while it was down acts on the
/// room's own last-leave timestamp rather than starting the clock over.
pub async fn sweep_alone_rooms(client: &Client, watch: &AloneWatch) {
    let Some(own_user) = client.user_id() else {
        return;
    };
    let now = SystemTime::now();
    for room in client.joined_rooms() {
        let room_id = room.room_id().to_string();
        let alone = match daemon_is_alone(&room, own_user).await {
            Ok(alone) => alone,
            Err(err) => {
                eprintln!("safehoused: alone-check for {room_id} skipped: {err:#}");
                continue;
            }
        };
        let observation = if alone {
            Some(alone_since_from_room(&room).await.unwrap_or(now))
        } else {
            None
        };
        watch.observe(&room_id, observation);
        let Some(since) = watch.due(&room_id, now) else {
            continue;
        };
        // Short-circuited rather than evaluating both: a Space needs no tag
        // read (and must not produce that read's failure log).
        let veto = if room.is_space() {
            auto_leave_veto(true, false)
        } else {
            auto_leave_veto(false, is_server_notice_room(&room).await)
        };
        if let Some(veto) = veto {
            if watch.mark_declined(&room_id) {
                println!(
                    "safehoused: not leaving {room_id} despite being its only member ({veto})"
                );
            }
            continue;
        }
        println!(
            "safehoused: leaving {room_id} (alone since {})",
            rfc3339_utc(since)
        );
        match leave_and_forget(client, &room, Some(ALONE_LEAVE_REASON)).await {
            Ok(()) => watch.clear(&room_id),
            Err(err) => eprintln!("safehoused: leaving {room_id} failed: {err:#}"),
        }
    }
}

/// The enforcer task: [`sweep_alone_rooms`] on a timer, for the life of the
/// daemon. Spawned only when `leave_when_alone` is on, so with the default
/// config not one line of this module's watching half ever runs.
pub async fn watch_alone_rooms(client: Client, watch: std::sync::Arc<AloneWatch>) {
    loop {
        tokio::time::sleep(ALONE_SWEEP_INTERVAL).await;
        sweep_alone_rooms(&client, &watch).await;
    }
}

/// The `leave_when_alone` decision core (#201). Every one of these exercises
/// the logic that decides *whether* to leave — the grace period, the
/// leave-and-rejoin reset, the last-member test, and the server-notice
/// exclusion — without a live homeserver, which is the whole reason that logic
/// is factored out of the room-walking sweep.
#[cfg(test)]
mod tests {
    use matrix_sdk::ruma::{
        events::tag::{TagInfo, Tags},
        user_id,
    };

    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    const GRACE: Duration = Duration::from_secs(600);

    #[test]
    fn an_alone_run_keeps_the_first_timestamp_it_was_given() {
        // The deadline must be anchored to when the room emptied, not to the
        // last time anything happened to look at it — otherwise a room that is
        // observed every sweep never becomes due.
        let watch = AloneWatch::with_grace(GRACE);
        assert_eq!(watch.observe("!r:x", Some(at(1_000))), Some(at(1_000)));
        assert_eq!(watch.observe("!r:x", Some(at(1_300))), Some(at(1_000)));
        assert_eq!(watch.alone_since("!r:x"), Some(at(1_000)));
    }

    #[test]
    fn leaving_only_becomes_due_once_the_grace_period_has_elapsed() {
        let watch = AloneWatch::with_grace(GRACE);
        watch.observe("!r:x", Some(at(1_000)));
        assert_eq!(watch.due("!r:x", at(1_000)), None);
        assert_eq!(watch.due("!r:x", at(1_599)), None, "one second short");
        assert_eq!(watch.due("!r:x", at(1_600)), Some(at(1_000)));
    }

    #[test]
    fn a_rejoin_inside_the_grace_period_resets_the_clock() {
        // The issue's explicit requirement: a quick leave-and-rejoin must not
        // trigger a leave. Total elapsed time since the *first* departure is
        // well past the grace period by the end of this test; what matters is
        // that the run was interrupted.
        let watch = AloneWatch::with_grace(GRACE);
        watch.observe("!r:x", Some(at(1_000)));
        watch.observe("!r:x", None); // they came back
        assert_eq!(watch.alone_since("!r:x"), None);
        assert_eq!(watch.due("!r:x", at(2_000)), None);

        // ...and they leave again. The clock starts from the second departure.
        watch.observe("!r:x", Some(at(1_900)));
        assert_eq!(watch.due("!r:x", at(2_000)), None, "only 100s into the run");
        assert_eq!(watch.due("!r:x", at(2_500)), Some(at(1_900)));
    }

    #[test]
    fn an_un_tracked_room_is_never_due() {
        let watch = AloneWatch::with_grace(GRACE);
        assert_eq!(watch.due("!never-seen:x", at(9_999)), None);
    }

    #[test]
    fn clearing_a_room_stops_it_from_coming_due() {
        let watch = AloneWatch::with_grace(GRACE);
        watch.observe("!r:x", Some(at(1_000)));
        watch.clear("!r:x");
        assert_eq!(watch.due("!r:x", at(5_000)), None);
    }

    #[test]
    fn a_declined_room_is_only_reported_once() {
        let watch = AloneWatch::with_grace(GRACE);
        assert!(watch.mark_declined("!notices:x"));
        assert!(!watch.mark_declined("!notices:x"));
        assert!(watch.mark_declined("!other:x"));
    }

    #[test]
    fn a_future_alone_since_is_never_elapsed() {
        // Clock skew between this host and the homeserver that stamped the
        // membership event must not read as "elapsed long ago".
        assert!(!grace_elapsed(at(2_000), at(1_000), GRACE));
    }

    #[test]
    fn only_the_daemon_joined_is_alone() {
        let own = user_id!("@safehoused:x");
        assert!(is_last_member(own, &[own.to_owned()], 0));
    }

    #[test]
    fn another_joined_member_is_not_alone() {
        let own = user_id!("@safehoused:x");
        assert!(!is_last_member(
            own,
            &[own.to_owned(), user_id!("@robb:x").to_owned()],
            0
        ));
    }

    #[test]
    fn a_pending_invite_is_not_alone() {
        // A room the operator just created and invited someone into must not be
        // auto-left out from under them.
        let own = user_id!("@safehoused:x");
        assert!(!is_last_member(own, &[own.to_owned()], 1));
    }

    #[test]
    fn a_room_the_daemon_is_not_joined_to_is_not_alone() {
        // Defensive: whatever this is, it is not "the daemon is the last one
        // here", so it must never be a leave trigger.
        let own = user_id!("@safehoused:x");
        assert!(!is_last_member(own, &[user_id!("@robb:x").to_owned()], 0));
        assert!(!is_last_member(own, &[], 0));
    }

    #[test]
    fn a_space_is_never_auto_left() {
        // #27's fleet Space has exactly one member — this bot — by design.
        // "Alone in it" is its steady state, not an abandoned room.
        assert_eq!(
            auto_leave_veto(true, false),
            Some("Space (m.space) — a container, not a conversation")
        );
        assert_eq!(
            auto_leave_veto(true, true),
            Some("Space (m.space) — a container, not a conversation")
        );
    }

    #[test]
    fn a_server_notices_room_is_never_auto_left() {
        assert_eq!(auto_leave_veto(false, true), Some("server-notices room"));
    }

    #[test]
    fn an_ordinary_empty_room_has_no_veto() {
        assert_eq!(auto_leave_veto(false, false), None);
    }

    #[test]
    fn the_server_notice_tag_is_detected() {
        let mut tags = Tags::new();
        tags.insert(TagName::ServerNotice, TagInfo::new());
        assert!(tagged_server_notice(Some(&tags)));
    }

    #[test]
    fn other_tags_and_no_tags_are_not_server_notices() {
        assert!(!tagged_server_notice(None));
        assert!(!tagged_server_notice(Some(&Tags::new())));
        let mut tags = Tags::new();
        tags.insert(TagName::Favorite, TagInfo::new());
        tags.insert(TagName::User("u.fleet".parse().unwrap()), TagInfo::new());
        assert!(!tagged_server_notice(Some(&tags)));
    }

    #[test]
    fn rfc3339_utc_renders_known_instants() {
        assert_eq!(rfc3339_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(at(1_790_944_496)), "2026-10-02T12:34:56Z");
        // A leap day, and the last second of a year.
        assert_eq!(rfc3339_utc(at(1_583_020_800)), "2020-03-01T00:00:00Z");
        assert_eq!(rfc3339_utc(at(1_767_225_599)), "2025-12-31T23:59:59Z");
    }
}
