// Sonance Island: a Dynamic Island in the middle of the top bar for Sonance.
// Collapsed it is a small black pill (cover, title, level bars); hovering it
// springs it open into the song, a seek bar, the transport buttons and the
// group volume.
//
// Sonance publishes its controls with the MPRIS interfaces but under its own
// bus name, so GNOME's media keys and media controls leave it alone; this
// extension is the only thing that talks to it.
// SPDX-License-Identifier: MIT

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Graphene from 'gi://Graphene';
import Pango from 'gi://Pango';
import Soup from 'gi://Soup?version=3.0';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as Slider from 'resource:///org/gnome/shell/ui/slider.js';

const BUS_NAME = 'dev.sonance.Sonance.Controls';
const PATH = '/org/mpris/MediaPlayer2';

const COMPACT_WIDTH = 230;
const EXPANDED_WIDTH = 440;
const EXPANDED_HEIGHT = 206;
const OPEN_MS = 420;
const CLOSE_MS = 300;
const CLOSE_DELAY_MS = 220;
// Volume changes go to the speakers at most this often while dragging.
const VOLUME_EVERY_MS = 120;

const PlayerIface = `
<node>
  <interface name="org.mpris.MediaPlayer2.Player">
    <method name="PlayPause"/>
    <method name="Next"/>
    <method name="Previous"/>
    <method name="SetPosition">
      <arg type="o" direction="in"/>
      <arg type="x" direction="in"/>
    </method>
    <signal name="Seeked">
      <arg type="x"/>
    </signal>
    <property name="Metadata" type="a{sv}" access="read"/>
    <property name="PlaybackStatus" type="s" access="read"/>
    <property name="Volume" type="d" access="readwrite"/>
    <property name="Position" type="x" access="read"/>
    <property name="CanGoNext" type="b" access="read"/>
    <property name="CanGoPrevious" type="b" access="read"/>
    <property name="CanSeek" type="b" access="read"/>
  </interface>
</node>`;
const PlayerProxy = Gio.DBusProxy.makeProxyWrapper(PlayerIface);

function clock(us) {
    const s = Math.max(0, Math.floor(us / 1e6));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

function cssUrl(path) {
    return GLib.filename_to_uri(path, null).replace(/"/g, '%22');
}

export default class SonanceIslandExtension extends Extension {
    enable() {
        this._cancellable = new Gio.Cancellable();
        this._soup = new Soup.Session({timeout: 10});
        this._cacheDir = GLib.build_filenamev([GLib.get_user_cache_dir(), 'sonance-island']);
        GLib.mkdir_with_parents(this._cacheDir, 0o700);

        this._proxy = null;
        this._expanded = false;
        this._artUrl = null;
        this._position = 0;
        this._length = 0;
        this._playing = false;
        this._volumeTimer = 0;
        this._closeTimer = 0;
        this._tickTimer = 0;
        this._settingSliders = false;

        this._buildUi();
        this._watchId = Gio.bus_watch_name(Gio.BusType.SESSION, BUS_NAME,
            Gio.BusNameWatcherFlags.NONE,
            () => this._connect(),
            () => this._disconnect());
        this._monitorsId = Main.layoutManager.connect('monitors-changed', () => this._place(false));
        this._panelId = Main.panel.connect('notify::allocation', () => this._place(false));
    }

    disable() {
        Gio.bus_unwatch_name(this._watchId);
        Main.layoutManager.disconnect(this._monitorsId);
        Main.panel.disconnect(this._panelId);
        this._cancellable.cancel();
        this._soup.abort();
        this._disconnect();
        for (const id of ['_volumeTimer', '_closeTimer', '_tickTimer']) {
            if (this[id])
                GLib.source_remove(this[id]);
            this[id] = 0;
        }
        this._island.destroy();
        this._island = null;
        this._soup = null;
    }

    /* ------------------------------------------------------------------ UI */

    _buildUi() {
        this._island = new St.Widget({
            style_class: 'sonance-island',
            reactive: true,
            track_hover: true,
            visible: false,
            layout_manager: new Clutter.BinLayout(),
        });
        this._island.connect('notify::hover', () => this._onHover());
        this._island.connect('notify::height', () => {
            const h = this._island.height;
            this._island.set_style(`border-radius: ${Math.round(Math.min(h / 2, 38))}px;`);
        });

        // Clips the content while the pill grows; the pill itself keeps its shadow.
        const clip = new St.Widget({
            layout_manager: new Clutter.BinLayout(),
            clip_to_allocation: true,
            x_expand: true,
            y_expand: true,
        });
        this._island.add_child(clip);

        /* Collapsed: cover, title, level bars. */
        this._compact = new St.BoxLayout({
            style_class: 'sonance-island-compact',
            x_expand: true,
            y_expand: true,
        });
        this._smallArt = new St.Bin({style_class: 'sonance-island-compact-art', y_align: Clutter.ActorAlign.CENTER});
        this._smallTitle = new St.Label({
            style_class: 'sonance-island-compact-title',
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._smallTitle.clutter_text.ellipsize = Pango.EllipsizeMode.END;
        this._bars = new St.BoxLayout({style_class: 'sonance-island-bars', y_align: Clutter.ActorAlign.CENTER});
        for (let i = 0; i < 4; i++) {
            const bar = new St.Widget({style_class: 'sonance-island-bar', pivot_point: new Graphene.Point({x: 0.5, y: 0.5})});
            bar.scale_y = 0.3;
            this._bars.add_child(bar);
        }
        this._compact.add_child(this._smallArt);
        this._compact.add_child(this._smallTitle);
        this._compact.add_child(this._bars);
        clip.add_child(this._compact);

        /* Expanded. Fixed width and pinned to the top, so growing reveals it. */
        this._full = new St.BoxLayout({
            style_class: 'sonance-island-expanded',
            orientation: Clutter.Orientation.VERTICAL,
            width: EXPANDED_WIDTH,
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.START,
            opacity: 0,
            visible: false,
        });
        clip.add_child(this._full);

        const top = new St.BoxLayout();
        this._art = new St.Button({style_class: 'sonance-island-art', y_align: Clutter.ActorAlign.CENTER});
        this._art.connect('clicked', () => this._raise());
        const info = new St.BoxLayout({
            style_class: 'sonance-island-info',
            orientation: Clutter.Orientation.VERTICAL,
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._room = new St.Label({style_class: 'sonance-island-room'});
        this._title = new St.Label({style_class: 'sonance-island-title'});
        this._artist = new St.Label({style_class: 'sonance-island-artist'});
        for (const l of [this._room, this._title, this._artist]) {
            l.clutter_text.ellipsize = Pango.EllipsizeMode.END;
            info.add_child(l);
        }
        top.add_child(this._art);
        top.add_child(info);
        this._full.add_child(top);

        const seekRow = new St.BoxLayout({style: 'spacing: 8px;'});
        this._elapsed = new St.Label({style_class: 'sonance-island-time', y_align: Clutter.ActorAlign.CENTER});
        this._remaining = new St.Label({style_class: 'sonance-island-time', y_align: Clutter.ActorAlign.CENTER});
        this._seek = new Slider.Slider(0);
        this._seek.y_align = Clutter.ActorAlign.CENTER;
        this._seek.connect('drag-end', () => this._seekTo(this._seek.value));
        this._seek.connect('notify::value', () => {
            if (this._seek._grab)
                this._showTimes(this._seek.value * this._length);
        });
        this._seek.connect('scroll-event', () => Clutter.EVENT_STOP);
        seekRow.add_child(this._elapsed);
        seekRow.add_child(this._seek);
        seekRow.add_child(this._remaining);
        this._full.add_child(seekRow);

        const controls = new St.BoxLayout({
            style_class: 'sonance-island-controls',
            x_align: Clutter.ActorAlign.CENTER,
        });
        const button = (icon, size, action) => {
            const b = new St.Button({
                style_class: 'sonance-island-button',
                child: new St.Icon({icon_name: icon, icon_size: size}),
            });
            b.connect('clicked', action);
            controls.add_child(b);
            return b;
        };
        this._prev = button('media-skip-backward-symbolic', 22, () => this._proxy?.PreviousAsync().catch(() => {}));
        this._playButton = button('media-playback-start-symbolic', 30, () => this._proxy?.PlayPauseAsync().catch(() => {}));
        this._next = button('media-skip-forward-symbolic', 22, () => this._proxy?.NextAsync().catch(() => {}));
        this._full.add_child(controls);

        const volRow = new St.BoxLayout({style: 'spacing: 10px;'});
        this._volIcon = new St.Icon({
            style_class: 'sonance-island-volume-icon',
            icon_name: 'audio-volume-medium-symbolic',
            icon_size: 14,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._volume = new Slider.Slider(0);
        this._volume.y_align = Clutter.ActorAlign.CENTER;
        this._volume.connect('notify::value', () => this._onVolume());
        volRow.add_child(this._volIcon);
        volRow.add_child(this._volume);
        this._full.add_child(volRow);

        Main.layoutManager.addTopChrome(this._island, {trackFullscreen: true});
    }

    /* Geometry, centred on the top bar of the primary monitor. */
    _geometry(expanded) {
        const [px, py] = Main.panel.get_transformed_position();
        const [pw, ph] = Main.panel.get_transformed_size();
        const h = Math.max(24, ph - 6);
        const y = Math.round(py + (ph - h) / 2);
        const w = expanded ? EXPANDED_WIDTH : COMPACT_WIDTH;
        const height = expanded ? EXPANDED_HEIGHT : h;
        return {x: Math.round(px + (pw - w) / 2), y, width: w, height};
    }

    _place(animate) {
        if (!this._island)
            return;
        const g = this._geometry(this._expanded);
        if (!animate) {
            this._island.remove_transition('x');
            this._island.remove_transition('width');
            this._island.remove_transition('height');
            this._island.set_position(g.x, g.y);
            this._island.set_size(g.width, g.height);
            return;
        }
        this._island.ease({
            ...g,
            duration: this._expanded ? OPEN_MS : CLOSE_MS,
            // A little overshoot on the way open, like the real thing.
            mode: this._expanded ? Clutter.AnimationMode.EASE_OUT_BACK : Clutter.AnimationMode.EASE_OUT_QUINT,
        });
    }

    _onHover() {
        if (this._island.hover) {
            if (this._closeTimer) {
                GLib.source_remove(this._closeTimer);
                this._closeTimer = 0;
            }
            this._setExpanded(true);
            return;
        }
        if (this._closeTimer)
            return;
        this._closeTimer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, CLOSE_DELAY_MS, () => {
            this._closeTimer = 0;
            // Dragging a slider out of the island must not fold it away mid-drag.
            if (this._island.hover || this._seek._grab || this._volume._grab)
                return GLib.SOURCE_CONTINUE;
            this._setExpanded(false);
            return GLib.SOURCE_REMOVE;
        });
    }

    _setExpanded(on) {
        if (this._expanded === on)
            return;
        this._expanded = on;
        this._place(true);
        if (on) {
            this._full.show();
            this._compact.ease({opacity: 0, duration: 120, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
            this._full.ease({opacity: 255, duration: 260, delay: 90, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
            this._pollPosition();
            this._startTick();
        } else {
            this._full.ease({
                opacity: 0,
                duration: 120,
                mode: Clutter.AnimationMode.EASE_OUT_QUAD,
                onComplete: () => {
                    if (!this._expanded)
                        this._full.hide();
                },
            });
            this._compact.ease({opacity: 255, duration: 220, delay: 120, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
            this._stopTick();
        }
    }

    /* ------------------------------------------------------------ D-Bus */

    _connect() {
        new PlayerProxy(Gio.DBus.session, BUS_NAME, PATH, (proxy, error) => {
            if (error || !this._island) {
                if (error)
                    console.error(`sonance-island: ${error}`);
                return;
            }
            this._proxy = proxy;
            this._changedId = proxy.connect('g-properties-changed', () => this._update());
            this._seekedId = proxy.connectSignal('Seeked', (p, s, [pos]) => {
                this._position = pos;
                this._showTimes(pos);
            });
            this._update();
            this._pollPosition();
        }, this._cancellable);
    }

    _disconnect() {
        if (this._proxy) {
            this._proxy.disconnect(this._changedId);
            this._proxy.disconnectSignal(this._seekedId);
            this._proxy = null;
        }
        if (this._island) {
            this._expanded = false;
            this._island.hide();
            this._stopTick();
        }
    }

    _raise() {
        Gio.DBus.session.call(BUS_NAME, PATH, 'org.mpris.MediaPlayer2', 'Raise',
            null, null, Gio.DBusCallFlags.NONE, -1, null, null);
    }

    /* Position isn't signalled (MPRIS asks players not to), so ask for it. */
    _pollPosition() {
        if (!this._proxy)
            return;
        Gio.DBus.session.call(BUS_NAME, PATH, 'org.freedesktop.DBus.Properties', 'Get',
            new GLib.Variant('(ss)', ['org.mpris.MediaPlayer2.Player', 'Position']),
            new GLib.VariantType('(v)'), Gio.DBusCallFlags.NONE, 2000, this._cancellable,
            (conn, res) => {
                try {
                    const [v] = conn.call_finish(res).deepUnpack();
                    this._position = v.unpack();
                    this._showTimes(this._position);
                } catch (e) {
                    // Sonance quit or is busy; the next tick tries again.
                }
            });
    }

    _startTick() {
        if (this._tickTimer)
            return;
        let n = 0;
        this._tickTimer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 1000, () => {
            // Count locally and re-sync every few seconds.
            if (this._playing)
                this._position = Math.min(this._length, this._position + 1e6);
            if (++n % 5 === 0)
                this._pollPosition();
            else
                this._showTimes(this._position);
            return GLib.SOURCE_CONTINUE;
        });
    }

    _stopTick() {
        if (this._tickTimer)
            GLib.source_remove(this._tickTimer);
        this._tickTimer = 0;
    }

    _seekTo(fraction) {
        if (!this._proxy || !this._length)
            return;
        const pos = Math.round(fraction * this._length);
        const track = this._proxy.Metadata?.['mpris:trackid']?.unpack() ?? '/dev/sonance/track/0';
        this._position = pos;
        this._showTimes(pos);
        this._proxy.SetPositionAsync(track, pos).catch(() => {});
    }

    _onVolume() {
        this._updateVolumeIcon();
        if (this._settingSliders || this._volumeTimer)
            return;
        this._volumeTimer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, VOLUME_EVERY_MS, () => {
            this._volumeTimer = 0;
            if (this._proxy)
                this._proxy.Volume = this._volume.value;
            return GLib.SOURCE_REMOVE;
        });
    }

    _updateVolumeIcon() {
        const v = this._volume.value;
        const level = v === 0 ? 'muted' : v < 0.34 ? 'low' : v < 0.67 ? 'medium' : 'high';
        this._volIcon.icon_name = `audio-volume-${level}-symbolic`;
    }

    /* ------------------------------------------------------------ display */

    _update() {
        const p = this._proxy;
        if (!p || !this._island)
            return;
        const md = p.Metadata ?? {};
        const title = md['xesam:title']?.unpack() ?? '';
        if (!title) {
            this._disconnectedLook();
            return;
        }
        const artist = (md['xesam:artist']?.deepUnpack() ?? []).filter(a => a).join(', ');
        const room = md['sonance:room']?.unpack() ?? '';
        this._length = Number(md['mpris:length']?.unpack() ?? 0);

        this._smallTitle.text = title;
        this._title.text = title;
        this._artist.text = artist || md['xesam:album']?.unpack() || '';
        this._room.text = room;
        this._room.visible = room !== '';

        const playing = p.PlaybackStatus === 'Playing';
        if (playing !== this._playing) {
            this._playing = playing;
            this._animateBars(playing);
        }
        this._playButton.child.icon_name = playing ? 'media-playback-pause-symbolic' : 'media-playback-start-symbolic';
        this._prev.reactive = this._next.reactive = p.CanGoNext ?? true;
        this._seek.reactive = this._length > 0;

        if (!this._volume._grab && this._volumeTimer === 0) {
            this._settingSliders = true;
            this._volume.value = p.Volume ?? 0;
            this._settingSliders = false;
            this._updateVolumeIcon();
        }
        this._setArt(md['mpris:artUrl']?.unpack() ?? null);
        this._showTimes(this._position);

        if (!this._island.visible) {
            this._place(false);
            this._island.opacity = 0;
            this._island.show();
            this._island.ease({opacity: 255, duration: 250, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
        }
    }

    _disconnectedLook() {
        this._expanded = false;
        this._island.hide();
        this._stopTick();
    }

    _showTimes(pos) {
        if (!this._island)
            return;
        this._elapsed.text = clock(pos);
        this._remaining.text = this._length ? `-${clock(this._length - pos)}` : '';
        if (!this._seek._grab) {
            this._settingSliders = true;
            this._seek.value = this._length ? Math.min(1, pos / this._length) : 0;
            this._settingSliders = false;
        }
    }

    _animateBars(on) {
        this._bars.get_children().forEach((bar, i) => {
            bar.remove_all_transitions();
            if (!on) {
                bar.ease({scale_y: 0.3, duration: 200, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
                return;
            }
            bar.scale_y = 0.3;
            bar.ease({
                scale_y: 1,
                duration: [420, 300, 520, 360][i],
                delay: [0, 120, 60, 180][i],
                mode: Clutter.AnimationMode.EASE_IN_OUT_SINE,
                repeatCount: -1,
                autoReverse: true,
            });
        });
    }

    /* Cover art comes from the speaker over HTTP; cached by URL. */
    _setArt(url) {
        if (url === this._artUrl)
            return;
        this._artUrl = url;
        if (!url) {
            this._paintArt(null);
            return;
        }
        if (url.startsWith('file://')) {
            this._paintArt(Gio.File.new_for_uri(url).get_path());
            return;
        }
        const name = GLib.compute_checksum_for_string(GLib.ChecksumType.SHA256, url, -1);
        const path = GLib.build_filenamev([this._cacheDir, name]);
        if (GLib.file_test(path, GLib.FileTest.EXISTS)) {
            this._paintArt(path);
            return;
        }
        const msg = Soup.Message.new('GET', url);
        if (!msg) {
            this._paintArt(null);
            return;
        }
        this._soup.send_and_read_async(msg, GLib.PRIORITY_DEFAULT, this._cancellable, (s, res) => {
            try {
                const bytes = s.send_and_read_finish(res);
                if (msg.get_status() !== Soup.Status.OK || !bytes?.get_size())
                    return;
                GLib.file_set_contents(path, bytes.get_data());
                if (this._artUrl === url)
                    this._paintArt(path);
            } catch (e) {
                if (!e.matches?.(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                    console.debug(`sonance-island: cover art: ${e}`);
            }
        });
    }

    _paintArt(path) {
        if (!this._island)
            return;
        const style = path ? `background-image: url("${cssUrl(path)}");` : '';
        this._smallArt.set_style(style);
        this._art.set_style(style);
    }
}
