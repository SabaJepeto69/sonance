// Sonance Island: a Dynamic Island in the middle of the top bar for Sonance.
//
// Collapsed it is a small black pill (cover, title, level bars): click it to
// play or pause, scroll on it for volume. Resting the pointer on it springs it
// open into the song, the current lyric, a seek bar, the transport buttons and
// the group volume. It also pops open briefly by itself: a new song, a volume
// change made elsewhere, alarms, the sleep timer, and Spotify playing on this
// PC (click that one to move it to the speakers).
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

const SIZES = {
    compact: {width: 230},
    volume: {width: 300},
    peek: {width: 400, height: 64},
    expanded: {width: 440},
};
const OPEN_MS = 420;
const CLOSE_MS = 300;
// Resting this long on the pill opens it, so a quick click can play/pause.
const OPEN_DELAY_MS = 200;
const CLOSE_DELAY_MS = 220;
const PEEK_MS = 3500;
const VOLUME_PEEK_MS = 1500;
// Volume changes go to the speakers at most this often while dragging.
const VOLUME_EVERY_MS = 120;
const SCROLL_STEP = 0.02;

const KIND_ICONS = {
    alarm: 'alarm-symbolic',
    sleep: 'weather-clear-night-symbolic',
    spotify: 'send-to-symbolic',
};

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

const IslandIface = `
<node>
  <interface name="dev.sonance.Sonance.Island">
    <method name="Activate">
      <arg type="s" direction="in"/>
    </method>
    <signal name="Notice">
      <arg type="s"/>
      <arg type="s"/>
      <arg type="s"/>
      <arg type="s"/>
    </signal>
    <property name="Lyric" type="s" access="read"/>
  </interface>
</node>`;
const IslandProxy = Gio.DBusProxy.makeProxyWrapper(IslandIface);

function clock(us) {
    const s = Math.max(0, Math.floor(us / 1e6));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

function cssUrl(path) {
    return GLib.filename_to_uri(path, null).replace(/"/g, '%22');
}

function ellipsize(label) {
    label.clutter_text.ellipsize = Pango.EllipsizeMode.END;
    return label;
}

export default class SonanceIslandExtension extends Extension {
    enable() {
        this._cancellable = new Gio.Cancellable();
        this._soup = new Soup.Session({timeout: 10});
        this._cacheDir = GLib.build_filenamev([GLib.get_user_cache_dir(), 'sonance-island']);
        GLib.mkdir_with_parents(this._cacheDir, 0o700);

        this._proxy = null;
        this._island = null;
        this._islandProxy = null;
        this._mode = 'compact';
        this._hasTrack = false;
        this._lastTitle = null;
        this._artUrl = null;
        this._artPath = null;
        this._position = 0;
        this._length = 0;
        this._playing = false;
        this._volumeValue = null;
        this._ownVolumeAt = 0;
        this._noticeAction = '';
        this._suppressOpen = false;
        this._settingSliders = false;
        this._timers = {};
        this._tickTimer = 0;

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
        this._stopTick();
        for (const name of Object.keys(this._timers))
            this._clearTimer(name);
        this._island.destroy();
        this._island = null;
        this._soup = null;
    }

    _timer(name, ms, fn) {
        this._clearTimer(name);
        this._timers[name] = GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
            delete this._timers[name];
            fn();
            return GLib.SOURCE_REMOVE;
        });
    }

    _clearTimer(name) {
        if (this._timers[name]) {
            GLib.source_remove(this._timers[name]);
            delete this._timers[name];
        }
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
        this._island.connect('button-press-event', () => this._onClick());
        this._island.connect('scroll-event', (a, e) => this._onScroll(e));

        // Clips the content while the pill grows; the pill itself keeps its shadow.
        const clip = new St.Widget({
            layout_manager: new Clutter.BinLayout(),
            clip_to_allocation: true,
            x_expand: true,
            y_expand: true,
        });
        this._island.add_child(clip);

        /* Collapsed: cover, title, level bars. */
        this._compact = new St.BoxLayout({style_class: 'sonance-island-compact', x_expand: true, y_expand: true});
        this._smallArt = new St.Bin({style_class: 'sonance-island-compact-art', y_align: Clutter.ActorAlign.CENTER});
        this._smallTitle = ellipsize(new St.Label({
            style_class: 'sonance-island-compact-title',
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        }));
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

        /* Volume pop-up: icon, level, percent. */
        this._volPeek = new St.BoxLayout({
            style_class: 'sonance-island-volpeek',
            x_expand: true,
            y_expand: true,
            opacity: 0,
            visible: false,
        });
        this._volPeekIcon = new St.Icon({icon_name: 'audio-volume-medium-symbolic', icon_size: 15, y_align: Clutter.ActorAlign.CENTER});
        this._volTrack = new St.Widget({
            style_class: 'sonance-island-level',
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._volFill = new St.Widget({style_class: 'sonance-island-level-fill'});
        this._volTrack.add_child(this._volFill);
        this._volTrack.connect('notify::width', () => this._paintVolPeek());
        this._volPeekLabel = new St.Label({style_class: 'sonance-island-volpeek-label', y_align: Clutter.ActorAlign.CENTER});
        this._volPeek.add_child(this._volPeekIcon);
        this._volPeek.add_child(this._volTrack);
        this._volPeek.add_child(this._volPeekLabel);
        clip.add_child(this._volPeek);

        /* Notice pop-up: picture or icon, title, line of text. */
        this._peek = new St.BoxLayout({
            style_class: 'sonance-island-peek',
            width: SIZES.peek.width,
            x_align: Clutter.ActorAlign.CENTER,
            y_expand: true,
            opacity: 0,
            visible: false,
        });
        this._peekArt = new St.Bin({style_class: 'sonance-island-peek-art', y_align: Clutter.ActorAlign.CENTER});
        this._peekIcon = new St.Icon({icon_size: 18, x_align: Clutter.ActorAlign.CENTER, y_align: Clutter.ActorAlign.CENTER});
        this._peekArt.set_child(this._peekIcon);
        const peekText = new St.BoxLayout({
            orientation: Clutter.Orientation.VERTICAL,
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._peekTitle = ellipsize(new St.Label({style_class: 'sonance-island-peek-title'}));
        this._peekBody = ellipsize(new St.Label({style_class: 'sonance-island-peek-body'}));
        peekText.add_child(this._peekTitle);
        peekText.add_child(this._peekBody);
        this._peek.add_child(this._peekArt);
        this._peek.add_child(peekText);
        clip.add_child(this._peek);

        /* Expanded. Fixed width and pinned to the top, so growing reveals it. */
        this._full = new St.BoxLayout({
            style_class: 'sonance-island-expanded',
            orientation: Clutter.Orientation.VERTICAL,
            width: SIZES.expanded.width,
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.START,
            opacity: 0,
            visible: false,
        });
        clip.add_child(this._full);

        const top = new St.BoxLayout();
        this._art = new St.Button({style_class: 'sonance-island-art', y_align: Clutter.ActorAlign.CENTER});
        this._art.connect('clicked', () => this._activate('raise'));
        const info = new St.BoxLayout({
            style_class: 'sonance-island-info',
            orientation: Clutter.Orientation.VERTICAL,
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._room = ellipsize(new St.Label({style_class: 'sonance-island-room'}));
        this._title = ellipsize(new St.Label({style_class: 'sonance-island-title'}));
        this._artist = ellipsize(new St.Label({style_class: 'sonance-island-artist'}));
        for (const l of [this._room, this._title, this._artist])
            info.add_child(l);
        top.add_child(this._art);
        top.add_child(info);
        this._full.add_child(top);

        this._lyric = ellipsize(new St.Label({style_class: 'sonance-island-lyric', visible: false}));
        this._lyric.clutter_text.line_alignment = Pango.Alignment.CENTER;
        this._full.add_child(this._lyric);

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
        // Scrolling over the seek bar adjusts the volume like the rest of the island.
        this._seek.connect('scroll-event', (a, e) => this._onScroll(e));
        seekRow.add_child(this._elapsed);
        seekRow.add_child(this._seek);
        seekRow.add_child(this._remaining);
        this._full.add_child(seekRow);

        const controls = new St.BoxLayout({style_class: 'sonance-island-controls', x_align: Clutter.ActorAlign.CENTER});
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
        this._volume.connect('notify::value', () => this._onVolumeSlider());
        volRow.add_child(this._volIcon);
        volRow.add_child(this._volume);
        this._full.add_child(volRow);

        this._layers = {compact: this._compact, volume: this._volPeek, peek: this._peek, expanded: this._full};
        Main.layoutManager.addTopChrome(this._island, {trackFullscreen: true});
    }

    /* Geometry, centred on the top bar of the primary monitor. */
    _geometry(mode) {
        const [px, py] = Main.panel.get_transformed_position();
        const [pw, ph] = Main.panel.get_transformed_size();
        const bar = Math.max(24, ph - 6);
        const width = SIZES[mode].width;
        let height = SIZES[mode].height ?? bar;
        if (mode === 'expanded')
            height = Math.ceil(this._full.get_preferred_height(width)[1]);
        return {x: Math.round(px + (pw - width) / 2), y: Math.round(py + (ph - bar) / 2), width, height};
    }

    _place(animate) {
        if (!this._island)
            return;
        const g = this._geometry(this._mode);
        if (!animate) {
            for (const p of ['x', 'y', 'width', 'height'])
                this._island.remove_transition(p);
            this._island.set_position(g.x, g.y);
            this._island.set_size(g.width, g.height);
            return;
        }
        const opening = g.width * g.height > this._island.width * this._island.height;
        this._island.ease({
            ...g,
            duration: opening ? OPEN_MS : CLOSE_MS,
            // A little overshoot on the way open, like the real thing.
            mode: opening ? Clutter.AnimationMode.EASE_OUT_BACK : Clutter.AnimationMode.EASE_OUT_QUINT,
        });
    }

    _setMode(mode) {
        if (!this._island)
            return;
        if (mode !== 'peek' && mode !== 'volume')
            this._clearTimer('peek');
        if (mode === 'compact' && !this._hasTrack) {
            // Nothing to collapse to: fade away.
            this._mode = 'compact';
            this._stopTick();
            this._island.ease({
                opacity: 0,
                duration: 200,
                mode: Clutter.AnimationMode.EASE_OUT_QUAD,
                onComplete: () => {
                    if (!this._hasTrack && this._mode === 'compact')
                        this._island.hide();
                },
            });
            return;
        }
        if (!this._island.visible) {
            this._mode = 'compact';
            this._place(false);
            this._island.opacity = 0;
            this._island.show();
        }
        this._island.ease({opacity: 255, duration: 200, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
        const previous = this._mode;
        this._mode = mode;
        for (const [name, layer] of Object.entries(this._layers)) {
            if (name === mode) {
                layer.show();
                layer.ease({opacity: 255, duration: 240, delay: name === previous ? 0 : 90, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
            } else if (layer.visible) {
                layer.ease({
                    opacity: 0,
                    duration: 120,
                    mode: Clutter.AnimationMode.EASE_OUT_QUAD,
                    onComplete: () => {
                        if (this._mode !== name)
                            layer.hide();
                    },
                });
            }
        }
        this._place(true);
        if (mode === 'expanded') {
            this._pollPosition();
            this._startTick();
        } else {
            this._stopTick();
        }
    }

    _onHover() {
        if (this._island.hover) {
            this._clearTimer('close');
            if (this._mode !== 'expanded' && !this._suppressOpen)
                this._timer('open', OPEN_DELAY_MS, () => this._setMode('expanded'));
            return;
        }
        this._suppressOpen = false;
        this._clearTimer('open');
        this._scheduleClose();
    }

    _scheduleClose() {
        this._timer('close', CLOSE_DELAY_MS, () => {
            // Dragging a slider out of the island must not fold it away mid-drag.
            if (this._island.hover || this._seek._grab || this._volume._grab) {
                this._scheduleClose();
                return;
            }
            if (this._mode === 'expanded')
                this._setMode('compact');
        });
    }

    _onClick() {
        if (this._mode === 'compact') {
            // Quick click on the pill: play/pause, and don't spring open under the pointer.
            this._clearTimer('open');
            this._suppressOpen = true;
            this._proxy?.PlayPauseAsync().catch(() => {});
            return Clutter.EVENT_STOP;
        }
        if (this._mode === 'peek' || this._mode === 'volume') {
            const action = this._mode === 'peek' ? this._noticeAction : '';
            if (action) {
                this._activate(action);
                this._suppressOpen = true;
                this._setMode('compact');
            } else {
                this._setMode('expanded');
            }
            return Clutter.EVENT_STOP;
        }
        return Clutter.EVENT_PROPAGATE;
    }

    _onScroll(event) {
        // A wheel notch also arrives as an emulated smooth scroll; count it once.
        if (!this._proxy || this._volumeValue === null || event.is_pointer_emulated())
            return Clutter.EVENT_STOP;
        let step = 0;
        switch (event.get_scroll_direction()) {
        case Clutter.ScrollDirection.UP:
            step = SCROLL_STEP;
            break;
        case Clutter.ScrollDirection.DOWN:
            step = -SCROLL_STEP;
            break;
        case Clutter.ScrollDirection.SMOOTH: {
            const [, dy] = event.get_scroll_delta();
            step = -dy * SCROLL_STEP;
            break;
        }
        default:
            return Clutter.EVENT_STOP;
        }
        const v = Math.max(0, Math.min(1, this._volume.value + step));
        this._volume.value = v; // sends it, throttled
        if (this._mode !== 'expanded')
            this._showVolumePeek(v);
        return Clutter.EVENT_STOP;
    }

    /* ------------------------------------------------------------ pop-ups */

    _peekFor(mode, ms) {
        this._setMode(mode);
        this._timer('peek', ms, () => {
            if (this._mode === mode)
                this._setMode(this._island.hover && !this._suppressOpen ? 'expanded' : 'compact');
        });
    }

    _showVolumePeek(v) {
        this._volumeValue = v;
        this._paintVolPeek();
        this._volPeekLabel.text = `${Math.round(v * 100)}`;
        this._volPeekIcon.icon_name = this._volumeIconName(v);
        this._peekFor('volume', VOLUME_PEEK_MS);
    }

    _paintVolPeek() {
        const [w, h] = this._volTrack.get_size();
        if (w > 0)
            this._volFill.set_size(Math.round(w * (this._volumeValue ?? 0)), h);
    }

    _showNotice(kind, title, body, action) {
        if (this._mode === 'expanded')
            return;
        this._noticeAction = action;
        this._peekTitle.text = title;
        this._peekBody.text = body;
        const icon = KIND_ICONS[kind];
        if (icon) {
            this._peekArt.set_style('');
            this._peekArt.add_style_class_name('icon');
            this._peekIcon.icon_name = icon;
            this._peekIcon.show();
        } else {
            this._peekArt.remove_style_class_name('icon');
            this._peekArt.set_style(this._artPath ? `background-image: url("${cssUrl(this._artPath)}");` : '');
            this._peekIcon.hide();
        }
        this._peekFor('peek', action ? PEEK_MS * 2 : PEEK_MS);
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
            this._changedId = proxy.connect('g-properties-changed', (p, changed) => {
                const keys = Object.keys(changed.deepUnpack());
                this._update(keys.includes('Volume'));
            });
            this._seekedId = proxy.connectSignal('Seeked', (p, s, [pos]) => {
                this._position = pos;
                this._showTimes(pos);
            });
            this._update(false);
            this._pollPosition();
        }, this._cancellable);

        new IslandProxy(Gio.DBus.session, BUS_NAME, PATH, (proxy, error) => {
            if (error || !this._island)
                return;
            this._islandProxy = proxy;
            this._lyricId = proxy.connect('g-properties-changed', () => this._showLyric());
            this._noticeId = proxy.connectSignal('Notice', (p, s, [kind, title, body, action]) =>
                this._showNotice(kind, title, body, action));
            this._showLyric();
        }, this._cancellable);
    }

    _disconnect() {
        if (this._proxy) {
            this._proxy.disconnect(this._changedId);
            this._proxy.disconnectSignal(this._seekedId);
            this._proxy = null;
        }
        if (this._islandProxy) {
            this._islandProxy.disconnect(this._lyricId);
            this._islandProxy.disconnectSignal(this._noticeId);
            this._islandProxy = null;
        }
        this._hasTrack = false;
        this._lastTitle = null;
        this._volumeValue = null;
        if (this._island) {
            this._mode = 'compact';
            this._island.hide();
            this._stopTick();
        }
    }

    _activate(action) {
        if (this._islandProxy) {
            this._islandProxy.ActivateAsync(action).catch(() => {});
        } else if (action === 'raise') {
            Gio.DBus.session.call(BUS_NAME, PATH, 'org.mpris.MediaPlayer2', 'Raise',
                null, null, Gio.DBusCallFlags.NONE, -1, null, null);
        }
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

    _onVolumeSlider() {
        this._volIcon.icon_name = this._volumeIconName(this._volume.value);
        if (this._settingSliders)
            return;
        this._ownVolumeAt = Date.now();
        if (this._timers.volume)
            return;
        this._timer('volume', VOLUME_EVERY_MS, () => {
            this._ownVolumeAt = Date.now();
            if (this._proxy)
                this._proxy.Volume = this._volume.value;
        });
    }

    _volumeIconName(v) {
        const level = v === 0 ? 'muted' : v < 0.34 ? 'low' : v < 0.67 ? 'medium' : 'high';
        return `audio-volume-${level}-symbolic`;
    }

    /* ------------------------------------------------------------ display */

    _update(volumeChanged) {
        const p = this._proxy;
        if (!p || !this._island)
            return;
        const md = p.Metadata ?? {};
        const title = md['xesam:title']?.unpack() ?? '';
        this._hasTrack = title !== '';
        if (!this._hasTrack) {
            if (this._mode === 'compact')
                this._setMode('compact');
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

        // The volume: pop up when it was changed somewhere else (phone, speaker buttons).
        const v = p.Volume ?? 0;
        const first = this._volumeValue === null;
        if (!this._volume._grab && !this._timers.volume) {
            this._settingSliders = true;
            this._volume.value = v;
            this._settingSliders = false;
            this._volIcon.icon_name = this._volumeIconName(v);
        }
        const elsewhere = Date.now() - this._ownVolumeAt > 1500;
        if (volumeChanged && !first && elsewhere && this._mode !== 'expanded' && Math.abs(v - this._volumeValue) > 0.001)
            this._showVolumePeek(v);
        this._volumeValue = v;

        this._setArt(md['mpris:artUrl']?.unpack() ?? null);
        this._showTimes(this._position);

        const songChanged = this._lastTitle !== null && this._lastTitle !== title;
        this._lastTitle = title;
        if (!this._island.visible) {
            this._setMode('compact');
        } else if (songChanged && playing && this._mode === 'compact') {
            // Wait a moment so the new cover has a chance to arrive.
            this._timer('songPeek', 400, () => {
                if (this._mode === 'compact')
                    this._showNotice('song', title, artist, '');
            });
        }
    }

    _showLyric() {
        const line = this._islandProxy?.Lyric ?? '';
        const wasVisible = this._lyric.visible;
        this._lyric.text = line;
        this._lyric.visible = line !== '';
        if (wasVisible !== this._lyric.visible && this._mode === 'expanded')
            this._place(true);
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
        this._artPath = path;
        const style = path ? `background-image: url("${cssUrl(path)}");` : '';
        this._smallArt.set_style(style);
        this._art.set_style(style);
        if (this._mode === 'peek' && !this._peekArt.has_style_class_name('icon'))
            this._peekArt.set_style(style);
    }
}
