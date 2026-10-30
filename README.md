# RustPlay

An AirPlay screen-mirroring receiver for Windows, written in Rust, with sound. Open it,
then on your iPhone or iPad: Control Center → Screen Mirroring → **RustPlay**.

- **Rename it:** click the name under the icon. The new name shows up on the iPhone within
  a few seconds and is remembered.
- **Volume:** use the slider in the control bar while mirroring. You can also use the
  scroll wheel over the bar, ↑/↓, or M to mute. The iPhone's own volume buttons work
  too.
- **Full screen:** double-click the screen or press F.
- **Settings:** click Settings at the top left, or press Ctrl+,.
  - **Appearance:** Auto (follows Windows), Light or Dark.
  - **Require Code:** when this is checked, each new connection needs a 4-digit code.
    RustPlay shows a new code on screen, and you type it on the iPhone or iPad when it
    asks for the AirPlay password. After three wrong tries RustPlay pauses briefly, and
    the next try gets a new code.
- **Drop (send and receive files):** pick it in the boxes at the bottom. RustPlay is a
  full [LocalSend](https://localsend.org) device, so it works with the free LocalSend
  app on iPhone (and on Android, Mac and Linux). Real AirDrop can't reach a Windows PC:
  it needs Apple's AWDL / Wi-Fi Aware link, which Windows Wi-Fi drivers don't expose.
  - **iPhone → PC:** tap Share, choose LocalSend, then RustPlay. Files are accepted
    automatically and saved to Downloads. Text messages show up and can be copied.
  - **PC → iPhone:** drop files on the window (or click to choose), then click the
    iPhone under Nearby. LocalSend has to be open on the iPhone.
- **Mouse and keyboard (experimental):** click the keyboard-and-mouse toggle in the control
  bar. The PC then shows up as a Bluetooth mouse and keyboard. Pair it once on the iPad
  (Settings → Bluetooth, tap the PC's name), then click the mirrored screen to take
  control. Input only goes to the iPad while RustPlay is the window in front and the
  screen has the pointer. Esc, or switching to another window, gives it back. Ctrl acts
  as ⌘ (Ctrl+C copies), Alt as ⌥, and right Ctrl as Control. The PC's Bluetooth has to
  support the peripheral role. While control is on, double-click doesn't toggle full
  screen; use F instead. Turning the toggle off, or closing RustPlay, disconnects the
  iPad, so its on-screen keyboard comes back. It stays in the iPad's list as "Not
  Connected" and reconnects by itself the next time you turn the toggle on. Windows has
  no API for that disconnect (the driver's disconnect request only covers classic
  Bluetooth), so RustPlay switches Bluetooth off and on for a moment, but only when no
  other Bluetooth device is connected.

## Run

```bash
cargo run --release
```

The first launch shows a Windows Firewall prompt. Allow it, or the iPhone can't connect.
Both devices must be on the same network. If a firewall like Portmaster or a VPN like
Mullvad is running, allow incoming LAN connections for `rustplay.exe` there too. Drop
uses LocalSend's port 53317, so don't run the LocalSend desktop app at the same time.

Settings (name, volume, appearance, connection code) are stored in `%APPDATA%\app.rustplay.receiver\settings.json`,
next to `localsend.pem`, the certificate other LocalSend devices know this PC by, and
`rustplay.log`, the log of the last run. To install it somewhere else, copy just
`target\release\rustplay.exe`; the interface is built into it. Only one copy runs at a
time. Opening it again brings the open window forward, because two copies would both
pose as a Bluetooth mouse and knock each other's iPad off.

## How it works

- `src/airplay/mdns.rs` advertises `_airplay._tcp` and `_raop._tcp` over Bonjour, and
  re-advertises when the receiver is renamed.
- `src/airplay/rtsp.rs` handles the control connection on port 7000: `/info`, pairing,
  FairPlay, `SETUP`, volume and `TEARDOWN`.
- `src/airplay/pairing.rs` is legacy pairing (Ed25519 + X25519, AES-CTR).
- `src/airplay/code.rs` is the optional connection code: RFC 2617 digest authentication
  on the first `SETUP`, with a one-time code and limited guesses, like UxPlay's `-pw`.
- `src/airplay/fairplay.rs` runs the FairPlay handshake. The vendored C code in
  `vendor/playfair` decrypts the session key.
- `src/airplay/mirror.rs` decrypts the H.264 stream (AES-128-CTR) and survives the iPhone
  locking and reconnecting.
- `src/airplay/audio.rs` decrypts the audio (AES-128-CBC per RTP packet), decodes
  AAC-ELD with fdk-aac, and plays it through the default output with a ~70 ms jitter
  buffer.
- `src/drop.rs` runs Drop on LocalSend's own Rust core (`vendor/localsend-core`). That
  covers multicast discovery, the HTTPS server with certificate pinning, auto-accepting
  uploads into Downloads, and sending to devices under Nearby.
- `src/remote.rs` publishes a Bluetooth LE HID service (HID over GATT: a keyboard and a
  mouse report) through WinRT's `GattServiceProvider`, and turns the window's captured
  input into HID reports.
- `src/ui_socket.rs` streams video and status to the window over a token-protected
  localhost WebSocket, and takes commands (rename, volume, stop) back.
- `ui/` holds the interface. It decodes video with WebCodecs and animates with real
  spring physics (SwiftUI-style response/damping).
- `icons/draw_icon.py` draws the icon (Ferris the crab on an AirPlay screen) and the
  layers the UI uses for its parallax tile.

For testing, `RUSTPLAY_NAME="RustPlay Dev"` runs a second copy with its own name, device
ID and settings, and `RUSTPLAY_HIDE=1` keeps its window hidden.

## License

RustPlay is licensed under the GNU General Public License v3.0 (see `LICENSE`).

The protocol follows [UxPlay](https://github.com/FDH2/UxPlay). `vendor/playfair` is
GPL-3.0 (see its LICENSE.md), so a distributed build of RustPlay falls under the GPL-3.0.
fdk-aac comes with its own license (the Fraunhofer FDK AAC license). `vendor/localsend-core`
is LocalSend's core library under the Apache License 2.0 (see its LICENSE and VENDORED.md).
