# Sonance

A native Linux controller for Sonos speakers, written in Rust with GTK4 and
libadwaita. It talks to the speakers directly on your network: no Sonos
account, no cloud, no official app needed.

## Features

- **Playback.** Play/pause, skip, seek, shuffle and repeat. A cover-flow
  carousel on Now Playing: drag or swipe it to change songs.
- **Rooms.** Every room and group, grouping and ungrouping, group volume, and
  per-room volume (hover the volume icon in the player).
- **Queue.** Jump to, move, remove and clear tracks.
- **Sonos Favorites.** Play now, play next or add to the queue.
- **Spotify.** Search and browse your playlists and saved albums. Music plays
  through the Spotify account linked in your Sonos system, so the Sonos queue
  stays in charge.
- **Alarms and sleep timer.**
- **Media keys and desktop controls.** Sonance publishes MPRIS, so keyboard
  media keys, GNOME's top-bar media controls and the lock screen control your
  Sonos.
- **Live updates.** Speakers notify Sonance the moment something changes,
  including changes made from your phone, instead of being asked every
  second.
- **Runs in the background.** Closing the window keeps media keys and the PC
  sound output working; **Quit Sonance** in the menu really quits. It can
  also start at login, hidden (Preferences).
- **Speaker settings.** Bass, treble, loudness, night sound and speech
  enhancement (home-theatre models), status light and button lock.
- **3D Room tab.** Place your speakers and one or more listening spots in a
  3D model of the room. Sonance computes, per spot, the volume for each room,
  the left/right balance of stereo pairs, a bass trim for speakers near walls,
  and which way to turn each speaker. A switch applies it or plays "regular";
  switching off undoes exactly what tuning changed.
- **Automatic room EQ.** Calibration also measures each speaker's frequency
  response at your spot. Over Bluetooth, an 8-band EQ on each speaker's
  delay line corrects it; Sonos over Wi-Fi only has bass and treble, so the
  correction is mapped onto those. It mainly cuts peaks and boosts gently
  (+3 dB at most), with an adjustable strength. Webcam and laptop mics are
  coloured in the treble, so a treble rise shared by every speaker is treated
  as the mic and ignored. A measurement mic gives the best results.
- **Microphone calibration.** Plays a test sweep from each speaker in turn and
  measures, with any mic placed at your spot, its real delay and loudness
  there. Those measurements replace the model's estimates.
- **This PC's sound on the speakers.** A "Sonance" sound output: anything the
  PC plays (Spotify app, YouTube, games) goes to the speakers.
  - Only Sonos selected: streamed to the group over Wi-Fi, about 0.5 s behind
    the PC.
  - Any other Bluetooth speaker switched on: every speaker, the Sonos rooms
    included, is fed over Bluetooth from the PC. Each gets its own delay line,
    so they all line up at your listening spot, within about 5 ms once
    calibrated.

## Requirements

- Linux with **PipeWire** (with `pipewire-pulse`) and **BlueZ**
- **libadwaita 1.7 or newer** (Fedora 42+, Ubuntu 25.04+, Debian 13, Arch)
- **Rust 1.85 or newer**. Older distro packages may be too old; use
  [rustup](https://rustup.rs) if so.
- `ffmpeg` with the MP3 encoder, `pactl`, and `pw-cat`/`pw-play`
- Sonos speakers on the same network (S2; S1 should work but is untested)
- For Bluetooth mode: a Bluetooth adapter, and Sonos speakers with Bluetooth
  (Era 100/300, Move, Roam)

Packages:

```sh
# Arch
sudo pacman -S rust gtk4 libadwaita pipewire pipewire-pulse wireplumber ffmpeg bluez bluez-utils
# Fedora
sudo dnf install cargo gtk4-devel libadwaita-devel pipewire-utils pipewire-pulseaudio ffmpeg-free bluez
# Debian / Ubuntu
sudo apt install cargo libgtk-4-dev libadwaita-1-dev pipewire-bin pipewire-pulse pulseaudio-utils ffmpeg bluez
```

## Install

```sh
git clone https://github.com/SabaJepeto69/sonance
cd sonance
./install.sh            # builds and installs to ~/.local, adds an app-menu entry
./install.sh uninstall  # removes it
```

Or just build and run it: `cargo run --release`.

### Firewall

Speakers connect back to this PC on two TCP ports:

- **8899**: they fetch the PC's sound from here ("Play this PC's sound").
- **8900**: live updates. Without it Sonance still works, but asks the
  speakers every second instead.

If you run a firewall, allow them from your LAN only (adjust the subnet to
yours):

```sh
sudo ufw allow from 192.168.1.0/24 to any port 8899:8900 proto tcp      # ufw
sudo firewall-cmd --add-port=8899-8900/tcp --permanent && sudo firewall-cmd --reload   # firewalld
```

Spotify sign-in uses a local callback on `127.0.0.1:8898`, which needs no rule.

## First run

Sonance finds your speakers by itself. Firewalls often drop the replies to
SSDP discovery; when that happens it scans the local /24 for port 1400
instead, which only needs outbound connections. Found speakers are remembered
in `~/.config/sonance/config.json`.

### Connecting Spotify

Spotify requires each user to register their own (free) developer app:

1. Open <https://developer.spotify.com/dashboard> and create an app. Tick
   "Web API". The app owner needs Spotify Premium.
2. Add the redirect URI `http://127.0.0.1:8898/callback`.
3. In Sonance: **Browse → Spotify**, paste the app's Client ID, press
   **Connect**.

Playback goes through the Spotify account linked in your Sonos system (link it
once in the official Sonos app). Sonance learns which account that is from any
Spotify favorite, alarm or queue item. If Spotify playback fails on a fresh
system, save one Spotify favorite in the Sonos app.

Development-mode Spotify apps return at most 10 search results per type.

### Bluetooth speakers

Open the room menu (top left) → **Add Bluetooth speaker…**, put the speaker in
pairing mode, and scan. Sonos speakers pair the same way: hold the Bluetooth
button until the light flashes blue. Then switch on **Play this PC's sound**
and the speakers you want.

In Bluetooth mode Sonance pauses the Sonos group, ungroups its rooms (each
takes its own Bluetooth link) and connects them. Switching off regroups them
and returns them to Wi-Fi.

### The Room tab and calibration

Set the room size, drag the speakers and spots into place (drag empty space to
orbit, scroll over a speaker to turn it), and pick a spot. Without measurements
the plan uses distances and angles. For real numbers, turn on **Play this PC's
sound**, put a microphone (a webcam's works) at the spot, and press
**Calibrate with microphone…**.

## Command line

- `sonance --probe` prints a read-only dump of what Sonance sees on the
  network: speakers, groups, queue, favorites, alarms.
- `sonance --background` starts with no window (used by start at login).
  Launching Sonance again brings the window back.
- `SONANCE_MONITOR=HDMI-1 sonance` opens the window on that monitor. Wayland
  gives apps no placement control, so it briefly goes fullscreen there.

## How it works

- `src/sonos/`: UPnP/SOAP control of the speakers (transport, queue,
  grouping, volume, EQ, alarms), DIDL metadata, discovery.
- `src/spotify.rs`: Spotify Web API with PKCE sign-in; search and library
  only.
- `src/room/`: the room model and the maths from positions and
  measurements to settings.
- `src/audio/`: the PipeWire `sonance` output, the Wi-Fi WAV stream,
  Bluetooth through BlueZ's D-Bus API, Sonance's own delay lines, and
  sweep-based measurement.
- `src/ui/`: the GTK interface, including the Cairo-drawn 3D room and the
  "liquid glass" menus.

## Limitations

- Sonos can't delay a single speaker over Wi-Fi: every speaker in a group plays
  at the same instant. Per-speaker timing works only in Bluetooth mode.
- One adapter driving several Bluetooth speakers at once may stutter; a second
  USB Bluetooth dongle helps.
- Sonos switches to its Bluetooth input about 2.5 s after it first hears sound
  over it, so the very start of playback can be cut on Sonos speakers in
  Bluetooth mode.
- Sonos Radio shortcuts in Favorites only open in the official app.
- Speaker setup, Trueplay and adding music services still need the official
  app once.

## Licence

MIT. See [LICENSE](LICENSE).
