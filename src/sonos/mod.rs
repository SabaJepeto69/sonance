pub mod didl;
pub mod discovery;
pub mod events;
pub mod soap;
pub mod xml;

use anyhow::{anyhow, Result};

pub use didl::Item;
use soap::{Soap, Svc};
use xml::{attr, elements, tag};

#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub uuid: String,
    pub name: String,
    pub ip: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub coordinator: Member,
    /// Visible members (stereo-pair partners and subs are hidden), coordinator first.
    pub members: Vec<Member>,
}

impl Group {
    pub fn name(&self) -> String {
        match self.members.len() {
            0 | 1 => self.coordinator.name.clone(),
            n => format!("{} + {}", self.coordinator.name, n - 1),
        }
    }
}

/// A physical speaker box.
#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    pub uuid: String,
    pub name: String,
    pub ip: String,
    /// UUID of the visible member (the "room") this box plays in.
    pub room: String,
    /// `LF`/`RF` for one half of a stereo pair.
    pub channel: Option<String>,
    /// e.g. "Sonos Era 100", "Sonos Move".
    pub model: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum PlayState {
    Playing,
    Paused,
    #[default]
    Stopped,
    Transitioning,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub state: PlayState,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art: Option<String>,
    pub position: u32,
    pub duration: u32,
    /// 1-based queue position; 0 when not playing from the queue.
    pub track_no: u32,
    pub play_mode: String,
    pub from_queue: bool,
    pub volume: u8,
    pub muted: bool,
}

impl Status {
    pub fn shuffle(&self) -> bool {
        self.play_mode.starts_with("SHUFFLE")
    }
    /// "off", "all" or "one".
    pub fn repeat(&self) -> &'static str {
        match self.play_mode.as_str() {
            "REPEAT_ALL" | "SHUFFLE" => "all",
            "REPEAT_ONE" | "SHUFFLE_REPEAT_ONE" => "one",
            _ => "off",
        }
    }
}

pub fn play_mode(shuffle: bool, repeat: &str) -> &'static str {
    match (shuffle, repeat) {
        (false, "all") => "REPEAT_ALL",
        (false, "one") => "REPEAT_ONE",
        (false, _) => "NORMAL",
        (true, "all") => "SHUFFLE",
        (true, "one") => "SHUFFLE_REPEAT_ONE",
        (true, _) => "SHUFFLE_NOREPEAT",
    }
}

#[derive(Clone, Debug, Default)]
pub struct Alarm {
    pub id: String,
    /// "HH:MM:SS"
    pub start: String,
    pub duration: String,
    /// ONCE, DAILY, WEEKDAYS, WEEKENDS or ON_<digits, 0 = Sunday>
    pub recurrence: String,
    pub enabled: bool,
    pub room_uuid: String,
    pub program_uri: String,
    pub program_meta: String,
    pub play_mode: String,
    pub volume: u8,
    pub include_linked: bool,
}

pub const BUZZER: &str = "x-rincon-buzzer:0";

#[derive(Clone, Debug, Default)]
pub struct Eq {
    pub bass: i32,
    pub treble: i32,
    pub loudness: bool,
    pub night: Option<bool>,
    pub speech: Option<bool>,
    pub led: bool,
    pub buttons_locked: bool,
}

/// Which Spotify account inside Sonos to play through.
#[derive(Clone, Debug)]
pub struct SpotifyAccount {
    pub sn: String,
    pub desc: String,
}

impl Default for SpotifyAccount {
    fn default() -> Self {
        Self { sn: "1".into(), desc: "SA_RINCON3079_X_#Svc3079-0-Token".into() }
    }
}

#[derive(Clone)]
pub struct Sonos {
    pub soap: Soap,
}

fn ok(r: Result<String>) -> Result<()> {
    r.map(|_| ())
}

impl Sonos {
    pub fn new() -> Self {
        Self { soap: Soap::new() }
    }

    async fn av(&self, ip: &str, action: &str, extra: &[(&str, &str)]) -> Result<String> {
        let mut args = vec![("InstanceID", "0")];
        args.extend_from_slice(extra);
        self.soap.call(ip, Svc::AVTransport, action, &args).await
    }

    async fn rc(&self, ip: &str, action: &str, extra: &[(&str, &str)]) -> Result<String> {
        let mut args = vec![("InstanceID", "0")];
        args.extend_from_slice(extra);
        self.soap.call(ip, Svc::RenderingControl, action, &args).await
    }

    async fn grc(&self, ip: &str, action: &str, extra: &[(&str, &str)]) -> Result<String> {
        let mut args = vec![("InstanceID", "0")];
        args.extend_from_slice(extra);
        self.soap.call(ip, Svc::GroupRenderingControl, action, &args).await
    }

    // ---- topology -------------------------------------------------------

    async fn model(&self, ip: &str) -> String {
        let url = format!("http://{ip}:1400/xml/device_description.xml");
        match self.soap.http().get(url).send().await {
            Ok(r) => r.text().await.ok().and_then(|t| tag(&t, "modelName")).unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    /// Every physical box, including hidden stereo-pair partners, which the
    /// room view places separately. `room` is the visible member it belongs to.
    pub async fn units(&self, ip: &str) -> Result<Vec<Unit>> {
        let r = self.soap.call(ip, Svc::ZoneGroupTopology, "GetZoneGroupState", &[]).await?;
        let state = tag(&r, "ZoneGroupState").ok_or_else(|| anyhow!("no ZoneGroupState"))?;
        let mut units = Vec::new();
        for g in elements(&state, "ZoneGroup") {
            let members = elements(g, "ZoneGroupMember");
            for m in &members {
                if attr(m, "IsZoneBridge").as_deref() == Some("1") {
                    continue;
                }
                let (Some(uuid), Some(name), Some(loc)) = (attr(m, "UUID"), attr(m, "ZoneName"), attr(m, "Location")) else { continue };
                let Some(ip) = loc.strip_prefix("http://").and_then(|l| l.split(':').next()).map(String::from) else { continue };
                // ChannelMapSet="A:RF,RF;B:LF,LF": which side of a stereo pair this box plays.
                let channel = attr(m, "ChannelMapSet").and_then(|set| {
                    set.split(';').find_map(|e| e.strip_prefix(&format!("{uuid}:")).map(|c| c.split(',').next().unwrap_or("").to_string()))
                });
                let invisible = attr(m, "Invisible").as_deref() == Some("1");
                // A hidden partner belongs to the visible member with the same name.
                let room = if invisible {
                    members
                        .iter()
                        .find(|o| attr(o, "Invisible").as_deref() != Some("1") && attr(o, "ZoneName").as_deref() == Some(name.as_str()))
                        .and_then(|o| attr(o, "UUID"))
                        .unwrap_or_else(|| uuid.clone())
                } else {
                    uuid.clone()
                };
                units.push(Unit { uuid, name, ip, room, channel, model: String::new() });
            }
        }
        let models = futures::future::join_all(units.iter().map(|u| self.model(&u.ip))).await;
        for (u, m) in units.iter_mut().zip(models) {
            u.model = m;
        }
        units.sort_by(|a, b| (a.name.as_str(), a.channel.as_deref()).cmp(&(b.name.as_str(), b.channel.as_deref())));
        Ok(units)
    }

    pub async fn groups(&self, ip: &str) -> Result<Vec<Group>> {
        let r = self.soap.call(ip, Svc::ZoneGroupTopology, "GetZoneGroupState", &[]).await?;
        let state = tag(&r, "ZoneGroupState").ok_or_else(|| anyhow!("no ZoneGroupState"))?;
        let mut groups = Vec::new();
        for g in elements(&state, "ZoneGroup") {
            let coord = attr(g, "Coordinator").unwrap_or_default();
            let mut members: Vec<Member> = elements(g, "ZoneGroupMember")
                .into_iter()
                .filter(|m| attr(m, "Invisible").as_deref() != Some("1") && attr(m, "IsZoneBridge").as_deref() != Some("1"))
                .filter_map(|m| {
                    let loc = attr(m, "Location")?;
                    let ip = loc.strip_prefix("http://")?.split(':').next()?.to_string();
                    Some(Member { uuid: attr(m, "UUID")?, name: attr(m, "ZoneName")?, ip })
                })
                .collect();
            let Some(ci) = members.iter().position(|m| m.uuid == coord) else { continue };
            let c = members.remove(ci);
            members.sort_by(|a, b| a.name.cmp(&b.name));
            members.insert(0, c.clone());
            groups.push(Group { coordinator: c, members });
        }
        groups.sort_by(|a, b| a.coordinator.name.cmp(&b.coordinator.name));
        Ok(groups)
    }

    /// Puts `member` into the group led by `coordinator_uuid`.
    pub async fn join(&self, member_ip: &str, coordinator_uuid: &str) -> Result<()> {
        ok(self.av(member_ip, "SetAVTransportURI", &[("CurrentURI", &format!("x-rincon:{coordinator_uuid}")), ("CurrentURIMetaData", "")]).await)
    }

    pub async fn leave(&self, member_ip: &str) -> Result<()> {
        ok(self.av(member_ip, "BecomeCoordinatorOfStandaloneGroup", &[]).await)
    }

    // ---- transport (always talk to the group coordinator) ---------------

    pub async fn play(&self, ip: &str) -> Result<()> {
        ok(self.av(ip, "Play", &[("Speed", "1")]).await)
    }
    pub async fn pause(&self, ip: &str) -> Result<()> {
        ok(self.av(ip, "Pause", &[]).await)
    }
    pub async fn next(&self, ip: &str) -> Result<()> {
        ok(self.av(ip, "Next", &[]).await)
    }
    pub async fn previous(&self, ip: &str) -> Result<()> {
        ok(self.av(ip, "Previous", &[]).await)
    }
    pub async fn seek(&self, ip: &str, secs: u32) -> Result<()> {
        ok(self.av(ip, "Seek", &[("Unit", "REL_TIME"), ("Target", &didl::hms(secs))]).await)
    }
    pub async fn set_play_mode(&self, ip: &str, mode: &str) -> Result<()> {
        ok(self.av(ip, "SetPlayMode", &[("NewPlayMode", mode)]).await)
    }

    pub async fn status(&self, ip: &str) -> Result<Status> {
        let (ti, pi, ts, mi, vol) = tokio::join!(
            self.av(ip, "GetTransportInfo", &[]),
            self.av(ip, "GetPositionInfo", &[]),
            self.av(ip, "GetTransportSettings", &[]),
            self.av(ip, "GetMediaInfo", &[]),
            self.group_volume(ip),
        );
        let (ti, pi) = (ti?, pi?);
        let mut s = Status {
            state: match tag(&ti, "CurrentTransportState").as_deref() {
                Some("PLAYING") => PlayState::Playing,
                Some("PAUSED_PLAYBACK") => PlayState::Paused,
                Some("TRANSITIONING") => PlayState::Transitioning,
                _ => PlayState::Stopped,
            },
            position: didl::parse_time(&tag(&pi, "RelTime").unwrap_or_default()),
            duration: didl::parse_time(&tag(&pi, "TrackDuration").unwrap_or_default()),
            track_no: tag(&pi, "Track").and_then(|t| t.parse().ok()).unwrap_or(0),
            play_mode: ts.ok().and_then(|t| tag(&t, "PlayMode")).unwrap_or_else(|| "NORMAL".into()),
            ..Default::default()
        };
        let media_uri = mi.as_ref().ok().and_then(|m| tag(m, "CurrentURI")).unwrap_or_default();
        if s.duration > 0 {
            s.position = s.position.min(s.duration);
        }
        s.from_queue = media_uri.starts_with("x-rincon-queue:");
        if !s.from_queue {
            s.track_no = 0;
        }
        if let Some(meta) = tag(&pi, "TrackMetaData").filter(|m| m.contains("<DIDL")) {
            if let Some(item) = didl::parse(&meta, ip).into_iter().next() {
                s.title = item.title;
                s.artist = item.artist;
                s.album = item.album;
                s.art = item.art;
            }
            // Radio: the title is the station, the stream content is the song.
            if let Some(stream) = tag(&meta, "r:streamContent").filter(|x| !x.is_empty()) {
                if let Some(station) = mi.ok().and_then(|m| tag(&m, "CurrentURIMetaData")).and_then(|m| tag(&m, "dc:title")) {
                    s.album = station;
                }
                match stream.split_once(" - ") {
                    Some((a, t)) => (s.artist, s.title) = (a.into(), t.into()),
                    None => s.title = stream,
                }
            }
        }
        if let Ok((v, m)) = vol {
            s.volume = v;
            s.muted = m;
        }
        Ok(s)
    }

    // ---- volume ---------------------------------------------------------

    pub async fn group_volume(&self, coord_ip: &str) -> Result<(u8, bool)> {
        let v = self.grc(coord_ip, "GetGroupVolume", &[]).await?;
        let m = self.grc(coord_ip, "GetGroupMute", &[]).await?;
        Ok((
            tag(&v, "CurrentVolume").and_then(|x| x.parse().ok()).unwrap_or(0),
            tag(&m, "CurrentMute").as_deref() == Some("1"),
        ))
    }

    /// `snapshot` records the rooms' current balance for scaling; take it once
    /// per drag, or dragging through 0 would flatten every room to one level.
    pub async fn set_group_volume(&self, coord_ip: &str, vol: u8, snapshot: bool) -> Result<()> {
        if snapshot {
            self.grc(coord_ip, "SnapshotGroupVolume", &[]).await?;
        }
        ok(self.grc(coord_ip, "SetGroupVolume", &[("DesiredVolume", &vol.to_string())]).await)
    }

    pub async fn set_group_mute(&self, coord_ip: &str, mute: bool) -> Result<()> {
        ok(self.grc(coord_ip, "SetGroupMute", &[("DesiredMute", if mute { "1" } else { "0" })]).await)
    }

    pub async fn volume(&self, ip: &str) -> Result<u8> {
        let v = self.rc(ip, "GetVolume", &[("Channel", "Master")]).await?;
        Ok(tag(&v, "CurrentVolume").and_then(|x| x.parse().ok()).unwrap_or(0))
    }

    /// One side of a stereo pair (`LF`/`RF`, 0–100): this is Sonos's balance control.
    pub async fn channel_volume(&self, room_ip: &str, channel: &str) -> Result<u8> {
        let v = self.rc(room_ip, "GetVolume", &[("Channel", channel)]).await?;
        Ok(tag(&v, "CurrentVolume").and_then(|x| x.parse().ok()).unwrap_or(100))
    }

    pub async fn set_channel_volume(&self, room_ip: &str, channel: &str, vol: u8) -> Result<()> {
        ok(self.rc(room_ip, "SetVolume", &[("Channel", channel), ("DesiredVolume", &vol.to_string())]).await)
    }

    pub async fn set_volume(&self, ip: &str, vol: u8) -> Result<()> {
        ok(self.rc(ip, "SetVolume", &[("Channel", "Master"), ("DesiredVolume", &vol.to_string())]).await)
    }

    // ---- speaker settings -----------------------------------------------

    pub async fn eq(&self, ip: &str) -> Result<Eq> {
        let num = |r: Result<String>, t: &str| r.ok().and_then(|x| tag(&x, t)).and_then(|x| x.parse::<i32>().ok());
        let opt_eq = |r: Result<String>| r.ok().and_then(|x| tag(&x, "CurrentValue")).map(|v| v == "1");
        let (bass, treble, loud, night, speech, led, lock) = tokio::join!(
            self.rc(ip, "GetBass", &[]),
            self.rc(ip, "GetTreble", &[]),
            self.rc(ip, "GetLoudness", &[("Channel", "Master")]),
            self.rc(ip, "GetEQ", &[("EQType", "NightMode")]),
            self.rc(ip, "GetEQ", &[("EQType", "DialogLevel")]),
            self.soap.call(ip, Svc::DeviceProperties, "GetLEDState", &[]),
            self.soap.call(ip, Svc::DeviceProperties, "GetButtonLockState", &[]),
        );
        Ok(Eq {
            bass: num(bass, "CurrentBass").ok_or_else(|| anyhow!("speaker did not report EQ"))?,
            treble: num(treble, "CurrentTreble").unwrap_or(0),
            loudness: num(loud, "CurrentLoudness") == Some(1),
            night: opt_eq(night),
            speech: opt_eq(speech),
            led: led.ok().and_then(|x| tag(&x, "CurrentLEDState")).as_deref() != Some("Off"),
            buttons_locked: lock.ok().and_then(|x| tag(&x, "CurrentButtonLockState")).as_deref() == Some("On"),
        })
    }

    pub async fn set_bass(&self, ip: &str, v: i32) -> Result<()> {
        ok(self.rc(ip, "SetBass", &[("DesiredBass", &v.to_string())]).await)
    }
    pub async fn set_treble(&self, ip: &str, v: i32) -> Result<()> {
        ok(self.rc(ip, "SetTreble", &[("DesiredTreble", &v.to_string())]).await)
    }
    pub async fn set_loudness(&self, ip: &str, on: bool) -> Result<()> {
        ok(self.rc(ip, "SetLoudness", &[("Channel", "Master"), ("DesiredLoudness", if on { "1" } else { "0" })]).await)
    }
    /// EQType is NightMode or DialogLevel (home-theatre speakers only).
    pub async fn set_eq(&self, ip: &str, eq_type: &str, on: bool) -> Result<()> {
        ok(self.rc(ip, "SetEQ", &[("EQType", eq_type), ("DesiredValue", if on { "1" } else { "0" })]).await)
    }
    pub async fn set_led(&self, ip: &str, on: bool) -> Result<()> {
        ok(self.soap.call(ip, Svc::DeviceProperties, "SetLEDState", &[("DesiredLEDState", if on { "On" } else { "Off" })]).await)
    }
    pub async fn set_buttons_locked(&self, ip: &str, locked: bool) -> Result<()> {
        ok(self.soap.call(ip, Svc::DeviceProperties, "SetButtonLockState", &[("DesiredButtonLockState", if locked { "On" } else { "Off" })]).await)
    }

    // ---- sleep timer ----------------------------------------------------

    /// Remaining seconds, or None when no timer is set.
    pub async fn sleep_timer(&self, coord_ip: &str) -> Result<Option<u32>> {
        let r = self.av(coord_ip, "GetRemainingSleepTimerDuration", &[]).await?;
        Ok(tag(&r, "RemainingSleepTimerDuration").filter(|s| !s.is_empty()).map(|s| didl::parse_time(&s)))
    }

    pub async fn set_sleep_timer(&self, coord_ip: &str, secs: Option<u32>) -> Result<()> {
        let d = secs.map(didl::hms).unwrap_or_default();
        ok(self.av(coord_ip, "ConfigureSleepTimer", &[("NewSleepTimerDuration", &d)]).await)
    }

    // ---- content: queue, favorites --------------------------------------

    pub async fn browse(&self, ip: &str, object_id: &str) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        loop {
            let start = out.len().to_string();
            let r = self
                .soap
                .call(ip, Svc::ContentDirectory, "Browse", &[
                    ("ObjectID", object_id),
                    ("BrowseFlag", "BrowseDirectChildren"),
                    ("Filter", "*"),
                    ("StartingIndex", &start),
                    ("RequestedCount", "200"),
                    ("SortCriteria", ""),
                ])
                .await?;
            let items = didl::parse(&tag(&r, "Result").unwrap_or_default(), ip);
            let total: usize = tag(&r, "TotalMatches").and_then(|t| t.parse().ok()).unwrap_or(0);
            let got = items.len();
            out.extend(items);
            if got == 0 || out.len() >= total {
                return Ok(out);
            }
        }
    }

    /// `count` queue items from 0-based `start`, in one request.
    pub async fn queue_slice(&self, coord_ip: &str, start: u32, count: u32) -> Result<Vec<Item>> {
        let r = self
            .soap
            .call(coord_ip, Svc::ContentDirectory, "Browse", &[
                ("ObjectID", "Q:0"),
                ("BrowseFlag", "BrowseDirectChildren"),
                ("Filter", "*"),
                ("StartingIndex", &start.to_string()),
                ("RequestedCount", &count.to_string()),
                ("SortCriteria", ""),
            ])
            .await?;
        Ok(didl::parse(&tag(&r, "Result").unwrap_or_default(), coord_ip))
    }

    pub async fn queue(&self, coord_ip: &str) -> Result<Vec<Item>> {
        self.browse(coord_ip, "Q:0").await
    }

    /// (UpdateID, length): changes whenever the queue does, for cheap polling.
    pub async fn queue_version(&self, coord_ip: &str) -> Result<(String, usize)> {
        let r = self
            .soap
            .call(coord_ip, Svc::ContentDirectory, "Browse", &[
                ("ObjectID", "Q:0"),
                ("BrowseFlag", "BrowseDirectChildren"),
                ("Filter", "dc:title"),
                ("StartingIndex", "0"),
                ("RequestedCount", "1"),
                ("SortCriteria", ""),
            ])
            .await?;
        Ok((tag(&r, "UpdateID").unwrap_or_default(), tag(&r, "TotalMatches").and_then(|t| t.parse().ok()).unwrap_or(0)))
    }

    pub async fn favorites(&self, ip: &str) -> Result<Vec<Item>> {
        self.browse(ip, "FV:2").await
    }

    pub async fn playlists(&self, ip: &str) -> Result<Vec<Item>> {
        self.browse(ip, "SQ:").await
    }

    /// Picks the Spotify account Sonos already has linked. Favorites and alarms
    /// carry (sn, token) pairs; the queue shows which sn is in use right now,
    /// which matters when an account was re-linked and the old sn went stale.
    pub fn spotify_account(favs: &[Item], alarms: &[Alarm], queue: &[Item]) -> Option<SpotifyAccount> {
        let sn_of = |uri: &str| uri.split("sid=12&").nth(1)?.split('&').find_map(|kv| kv.strip_prefix("sn=")).map(String::from);
        let mut known: Vec<SpotifyAccount> = favs
            .iter()
            .map(|f| (f.uri.as_str(), f.meta.as_str()))
            .chain(alarms.iter().map(|a| (a.program_uri.as_str(), a.program_meta.as_str())))
            .filter_map(|(uri, meta)| Some(SpotifyAccount { sn: sn_of(uri)?, desc: tag(meta, "desc")? }))
            .collect();
        let current = queue.iter().find_map(|q| sn_of(&q.uri));
        if let Some(sn) = current {
            let desc = known.iter().find(|a| a.sn == sn).map(|a| a.desc.clone()).unwrap_or_else(|| SpotifyAccount::default().desc);
            return Some(SpotifyAccount { sn, desc });
        }
        known.sort_by_key(|a| a.sn.parse::<u32>().unwrap_or(0));
        known.pop()
    }

    pub async fn clear_queue(&self, coord_ip: &str) -> Result<()> {
        ok(self.av(coord_ip, "RemoveAllTracksFromQueue", &[]).await)
    }

    /// Removes `count` tracks starting at 1-based `start`.
    pub async fn remove_range_from_queue(&self, coord_ip: &str, start: u32, count: u32) -> Result<()> {
        ok(self
            .av(coord_ip, "RemoveTrackRangeFromQueue", &[("UpdateID", "0"), ("StartingIndex", &start.to_string()), ("NumberOfTracks", &count.to_string())])
            .await)
    }

    /// `update_id` is the queue version the caller saw; Sonos refuses the edit if it moved on.
    pub async fn remove_from_queue(&self, coord_ip: &str, track_no: u32, update_id: &str) -> Result<()> {
        ok(self.av(coord_ip, "RemoveTrackFromQueue", &[("ObjectID", &format!("Q:0/{track_no}")), ("UpdateID", update_id)]).await)
    }

    /// Moves the track at 1-based `from` so it sits before 1-based `before`.
    pub async fn move_in_queue(&self, coord_ip: &str, from: u32, before: u32, update_id: &str) -> Result<()> {
        ok(self
            .av(coord_ip, "ReorderTracksInQueue", &[
                ("StartingIndex", &from.to_string()),
                ("NumberOfTracks", "1"),
                ("InsertBefore", &before.to_string()),
                ("UpdateID", update_id),
            ])
            .await)
    }

    /// Adds to the queue; `position` 0 means the end. Returns the first new track number.
    pub async fn enqueue(&self, coord_ip: &str, uri: &str, meta: &str, position: u32, as_next: bool) -> Result<u32> {
        let r = self
            .av(coord_ip, "AddURIToQueue", &[
                ("EnqueuedURI", uri),
                ("EnqueuedURIMetaData", meta),
                ("DesiredFirstTrackNumberEnqueued", &position.to_string()),
                ("EnqueueAsNext", if as_next { "1" } else { "0" }),
            ])
            .await?;
        Ok(tag(&r, "FirstTrackNumberEnqueued").and_then(|t| t.parse().ok()).unwrap_or(1))
    }

    pub async fn play_from_queue(&self, coord: &Member, track_no: u32) -> Result<()> {
        self.av(&coord.ip, "SetAVTransportURI", &[("CurrentURI", &format!("x-rincon-queue:{}#0", coord.uuid)), ("CurrentURIMetaData", "")])
            .await?;
        self.av(&coord.ip, "Seek", &[("Unit", "TRACK_NR"), ("Target", &track_no.max(1).to_string())]).await?;
        self.play(&coord.ip).await
    }

    /// Plays a URI the way the Sonos app does: queueable things replace the
    /// queue, streams (radio) are played directly.
    pub async fn play_uri(&self, coord: &Member, uri: &str, meta: &str, queueable: bool) -> Result<()> {
        if queueable {
            self.clear_queue(&coord.ip).await?;
            self.enqueue(&coord.ip, uri, meta, 0, false).await?;
            self.play_from_queue(coord, 1).await
        } else {
            self.av(&coord.ip, "SetAVTransportURI", &[("CurrentURI", uri), ("CurrentURIMetaData", meta)]).await?;
            self.play(&coord.ip).await
        }
    }

    /// Plays a single track without throwing away the queue, like the Sonos app.
    pub async fn play_now_keep_queue(&self, coord: &Member, uri: &str, meta: &str) -> Result<()> {
        let s = self.status(&coord.ip).await?;
        let pos = if s.from_queue && s.track_no > 0 { s.track_no + 1 } else { 0 };
        let n = self.enqueue(&coord.ip, uri, meta, pos, pos > 0).await?;
        self.play_from_queue(coord, n).await
    }

    /// Inserts right after the current track.
    pub async fn play_next(&self, coord: &Member, uri: &str, meta: &str) -> Result<()> {
        let s = self.status(&coord.ip).await?;
        let pos = if s.from_queue && s.track_no > 0 { s.track_no + 1 } else { 0 };
        self.enqueue(&coord.ip, uri, meta, pos, true).await.map(|_| ())
    }

    pub async fn play_favorite(&self, coord: &Member, fav: &Item) -> Result<()> {
        let queueable = fav.is_container() || !is_stream(&fav.uri);
        self.play_uri(coord, &fav.uri, &fav.meta, queueable).await
    }

    // ---- alarms ---------------------------------------------------------

    pub async fn alarms(&self, ip: &str) -> Result<Vec<Alarm>> {
        let r = self.soap.call(ip, Svc::AlarmClock, "ListAlarms", &[]).await?;
        let list = tag(&r, "CurrentAlarmList").unwrap_or_default();
        let mut out: Vec<Alarm> = elements(&list, "Alarm")
            .into_iter()
            .map(|a| {
                let g = |n: &str| attr(a, n).unwrap_or_default();
                Alarm {
                    id: g("ID"),
                    start: g("StartTime"),
                    duration: g("Duration"),
                    recurrence: g("Recurrence"),
                    enabled: g("Enabled") == "1",
                    room_uuid: g("RoomUUID"),
                    program_uri: g("ProgramURI"),
                    program_meta: g("ProgramMetaData"),
                    play_mode: g("PlayMode"),
                    volume: g("Volume").parse().unwrap_or(20),
                    include_linked: g("IncludeLinkedZones") == "1",
                }
            })
            .collect();
        out.sort_by(|a, b| a.start.cmp(&b.start));
        Ok(out)
    }

    fn alarm_args(a: &Alarm) -> Vec<(&'static str, String)> {
        vec![
            ("StartLocalTime", a.start.clone()),
            ("Duration", if a.duration.is_empty() { "01:00:00".into() } else { a.duration.clone() }),
            ("Recurrence", a.recurrence.clone()),
            ("Enabled", if a.enabled { "1" } else { "0" }.into()),
            ("RoomUUID", a.room_uuid.clone()),
            ("ProgramURI", a.program_uri.clone()),
            ("ProgramMetaData", a.program_meta.clone()),
            ("PlayMode", if a.play_mode.is_empty() { "SHUFFLE_NOREPEAT".into() } else { a.play_mode.clone() }),
            ("Volume", a.volume.to_string()),
            ("IncludeLinkedZones", if a.include_linked { "1" } else { "0" }.into()),
        ]
    }

    /// Creates the alarm when `id` is empty, updates it otherwise.
    pub async fn save_alarm(&self, ip: &str, a: &Alarm) -> Result<()> {
        let mut args = Self::alarm_args(a);
        let action = if a.id.is_empty() {
            "CreateAlarm"
        } else {
            args.insert(0, ("ID", a.id.clone()));
            "UpdateAlarm"
        };
        let refs: Vec<(&str, &str)> = args.iter().map(|(k, v)| (*k, v.as_str())).collect();
        ok(self.soap.call(ip, Svc::AlarmClock, action, &refs).await)
    }

    pub async fn delete_alarm(&self, ip: &str, id: &str) -> Result<()> {
        ok(self.soap.call(ip, Svc::AlarmClock, "DestroyAlarm", &[("ID", id)]).await)
    }
}

/// Radio and other live streams can't go in the queue.
pub fn is_stream(uri: &str) -> bool {
    ["x-sonosapi-stream:", "x-sonosapi-radio:", "x-rincon-mp3radio:", "x-sonosapi-hls:", "aac:", "hls-radio:", "x-rincon-stream:"]
        .iter()
        .any(|p| uri.starts_with(p))
}

/// Builds the URI + metadata that make Sonos play a Spotify object through
/// the Spotify account already linked in the Sonos system.
pub fn spotify_target(acc: &SpotifyAccount, spotify_uri: &str, title: &str) -> Option<(String, String)> {
    let kind = spotify_uri.split(':').nth(1)?;
    let enc = urlencoding::encode(spotify_uri);
    let sn = &acc.sn;
    let (id, uri, class) = match kind {
        "track" => (
            format!("10032020{enc}"),
            format!("x-sonos-spotify:{enc}?sid=12&flags=8232&sn={sn}"),
            "object.item.audioItem.musicTrack",
        ),
        "album" => (
            format!("1004206c{enc}"),
            format!("x-rincon-cpcontainer:1004206c{enc}?sid=12&flags=108&sn={sn}"),
            "object.container.album.musicAlbum",
        ),
        "playlist" => (
            format!("1006206c{enc}"),
            format!("x-rincon-cpcontainer:1006206c{enc}?sid=12&flags=108&sn={sn}"),
            "object.container.playlistContainer",
        ),
        _ => return None,
    };
    Some((uri, didl::service_meta(&id, title, class, &acc.desc)))
}
