# Nuschl 0.3.0

## Video calls and screen sharing

- **Calls** are available in early form in the Linux, Windows, macOS and web
  apps. Start or join a call from a channel or direct conversation, including
  on stock Mumble servers. Nuschl shows a call card with **Join**. Stock Mumble
  users receive a link to join in their browser. Share your camera and screen
  at the same time and minimize the call while you use chat. The desktop has a
  local preview and a full-screen gallery with the option to pin a feed.
- **Direct video:** a presenter's desktop app sends video straight to one or
  two viewers' desktop apps when its upload carries the video to each of them.
  When a direct connection slows down or fails, the video moves back to the
  call host's server, which also carries group calls, video in both directions
  and browser participants. Call media is encrypted. **Relay-only connections**
  sends it through the call host's TURN relay.
- **Shared audio on Linux:** while you share your screen, **Share audio** also
  sends the sound of your other applications. Nuschl's own voice and sounds
  stay out of it, and your microphone stays as it is. Share audio starts off
  and ends with the screen share. Each received screen share has a speaker on
  its tile, in the desktop app and on the call page, that mutes the stream or
  sets its volume. The desktop app remembers the volume you set for the host
  who invited you.
- **Video encoding on Linux:** the Nix-built desktop app encodes AV1 with
  Nuschl's own rate control, on the GPU through VA-API or with libaom where no
  GPU encodes AV1. It sends screens in temporal layers and key frames on
  request, follows bitrate changes without new key frames, and sharpens a
  still screen over the next frames. Screen frames may take up to 8 MiB.
  Hardware-encoded pictures show at their own size in the desktop app and on
  the call page in Chromium and Safari.
- **Screen sharing quality:** screen shares take their bitrate from the
  measured connection, separately from resolution and frame rate, and keep it
  through quiet periods. The call server switches each viewer between layers
  at frames that viewer can decode and leaves out whole frames for a viewer
  that falls behind. It forwards key frames up to 8 MiB whole and asks for key
  frames only when a viewer needs one. It reserves for a viewer's cameras the
  tier that viewer asks for, so a quiet screen reaches its rate on a fast link.
  Browser screen shares in AV1 use temporal layers at one resolution, which
  fixes pictures that froze for seconds at key frames. On the call page,
  camera tiles grow with the window, so Auto gets 720p at 4K.
- **Sharp, Smooth and Low latency:** each of your own streams keeps its
  resolution (Sharp) or its frame rate (Smooth), with Low latency as an option,
  in the desktop app and on the call page. Screens start Sharp, cameras
  Smooth. A viewer's quality menu is Auto plus an optional limit. **Video stats**
  shows frame rate, resolution, bitrate, the route and what limits the stream.
  On the call server it names the reason, such as "upload too low for direct".
- **Call host:** the NixOS module `services.nuschl-stream` serves the call page
  and runs the SFU, which hands out credentials for your TURN server. Operators
  can set egress and traffic budgets for relayed calls.

## Permissions

- **Channel types:** Open, Members only, Password, Announcements and Private
  set who may join, speak and write in one step, in a channel's permissions and
  in **Create sub-channel…**. A type writes standard Mumble rules, so it works
  on stock Mumble servers too.
- **Private channels on Nuschl servers:** the server sends each client only the
  channels it may reach and the people in them. A Private channel, its
  subchannels and the people inside stay hidden from everyone the channel
  leaves out. A channel created as Private is hidden from the start.
- **Effect preview:** unsaved changes show what saving does for Everyone,
  registered users and the channel's groups, here and in subchannels, for
  example "Registered users may no longer speak here and in 1 subchannel".
- **Moving people:** with Move permission, drag someone onto another channel
  in the desktop app, the web app or the terminal client. **Move to channel…**
  in a person's menu does the same from the keyboard.
- **Advanced rules in words:** rules the table cannot show are described in
  short sentences, such as "Members of @admin may move people here".
- Permissions show as a table with explanations. **Check access** shows who may
  do what in a channel. Groups are picked from a list, channel passwords work
  as in Mumble, and sub-channels are temporary by choice. Channels are sorted
  as Mumble sorts them.

## Images and files

- **Images** are sent re-encoded without metadata unless you pick **Original**.
  The message box offers Low, High and Original for its images, and Settings →
  Images sets the starting choice, High. Low and High turn photos upright, fit
  the server's image limit and clear the colour behind transparent areas.
  Original offers the file through file transfer. With **Strip metadata**
  ticked, as it is for every new message, Nuschl removes location, camera and
  other metadata from JPEG, PNG, WebP, GIF and BMP files and keeps their image
  data, orientation and colour profile. HEIC, AVIF, TIFF, JPEG XL and SVG files
  can be sent as Original with Strip metadata unticked.
- A sent message's files go to its own conversation, also after you switch to
  another one.
- **Downloads** arrive in Nuschl's cache folder and move to the place you chose
  once complete. When saving fails, the download stays and **Save as…** saves
  it to another place.
- A message whose image cannot be prepared returns to the message box with a
  note, an offer that cannot reach the transfer server ends with a note, and
  **Stop offering** ends an offer for good.
- **Link previews:** the desktop app creates previews on Windows and macOS too,
  and for long pages such as YouTube.

## Voice

- **Desktop voice playback** can use Deep PLC to fill missing speech packets.
  Nuschl uses opus-rs, a Rust port of libopus. Mute and unmute keep incoming
  voice playing. Voice keeps running when PipeWire moves it to a new default
  device, such as Bluetooth headphones.
- **Echo cancellation** finds the real echo delay, models stereo music and
  follows the microphone's clock, so it keeps removing echo when a busy system
  drops audio.
- **Mute:** when the server drops a quick mute or unmute, Nuschl sends your
  state again, up to five times, until the server shows it.
- **Volume:** users connected without a certificate get a volume slider and
  **Mute for me** too, for as long as they stay connected.

## Desktop

- **Version:** Settings → General and `--version` show the version with its
  commit, such as `0.3.0 (409afca)`, and **Copy version** copies it.
- **Status text** fits the default window size and keeps the layout still. The
  voice status reads Muted, Unmuted or Speaking, the header shows Not joined,
  Text only or Offline, and tooltips hold the full wording.
- **Notifications** go through the desktop's notification portal in sandboxes
  that offer only the portals, such as Flatpak.
- While you are in a call, its invitation card stays out of the video.
- Right and Left open, close and move through the channel tree. Dialogs keep
  their title and close button in view. Tiled Wayland windows keep their edges
  aligned when focus changes.
- New identities have a **recovery key** you can show or copy again from
  Settings.
- **Chat history:** the storage helper gets up to a minute to open its database
  on a slow disk and keeps running after a refused request.
- **Windows:** a per-user installer that runs with your own rights and adds a
  Start menu entry and an uninstall entry.

## Web app

- The web app connects saved servers on startup and reopens the last channel
  muted and deafened. Double-click a channel to join voice with a mouse or pen.
  Collapsed channels show how many people are inside. Ctrl+K opens the quick
  switcher and Ctrl+F searches the conversation. Voice playback adapts to
  uneven packet arrival, voice keeps running while a busy computer holds up
  the page's audio workers, and the page asks the browser to keep the screen
  on.
  When the browser stops the microphone, Nuschl mutes and shows **Microphone
  stopped**.

## Downloads

- **Windows x86_64:** [download the installer](https://github.com/nuschl-eu/nuschl/releases/download/v0.3.0/nuschl-0.3.0-windows-x86_64-setup.exe)
  or [the complete portable ZIP](https://github.com/nuschl-eu/nuschl/releases/download/v0.3.0/nuschl-0.3.0-windows-x86_64.zip).
  Unpack the ZIP into a writable folder and start `nuschl.exe`; keep the helper
  executables alongside it. Nuschl 0.2.0 on Windows can update itself.
- **macOS Intel and Apple silicon:** [download the complete universal app ZIP](https://github.com/nuschl-eu/nuschl/releases/download/v0.3.0/nuschl-0.3.0-macos-universal.zip), unpack it and move
  `Nuschl.app` to Applications. Requires macOS 14.2 or later. Native macOS runtime
  testing remains outstanding.
- **Linux:** install through the Nix flake at this release's tag, the APT
  repository at `https://dl.nuschl.eu/apt` (Debian 13 and Ubuntu 24.04, amd64)
  or the AUR package `nuschl-bin`. The Debian and AUR packages include native
  calls with software video encoding and decoding.

The installer and both ZIPs include all required helper programs and licenses.
The individual files listed under Assets support automatic updates.

Authenticode signatures for Windows and notarisation for macOS come later; the
macOS app is ad-hoc signed. See the [installation instructions](https://github.com/nuschl-eu/nuschl/blob/v0.3.0/docs/install.md)
for platform requirements, the APT setup and first-start prompts.

## Limits

- Anyone with the **call link** can join. The Mumble server can see that link.
  Voice chat uses the Mumble voice connection.
- **Shared audio:** the desktop app shares and plays it on Linux. The call
  page in a browser plays it on every system and can share a browser tab's
  audio.
- **Browser screen shares** reach every viewer at one resolution.
- **Older receivers:** desktop apps and call pages built before this release
  keep a 1 MiB frame limit and show no picture while a screen share's key
  frames are larger.
- **Private channels:** stock Mumble servers show them and the people in them
  to everyone. On Nuschl servers, Password channels stay listed for everyone.
- **Software video:** the Windows, macOS and portable Linux (Debian, AUR)
  builds encode video in software with rav1e. Windows and macOS capture runs
  up to 720p at 15 fps. Screen sharing was tested in a Windows 11 VM. Native
  macOS camera and screen-sharing permission testing remains outstanding.
- **Firefox** shows video that the desktop app encodes on the GPU with the
  encoder's padding, such as 2 extra pixel rows at 1080p or 16 extra columns
  at 3440 × 1440.
- Voice in a **phone browser** may pause when the screen turns off.

Ordinary Mumble voice and chat use transport encryption: the server can read
messages and hear voice. Local speech models, hosted identity recovery and mobile
apps remain unfinished.

Please report problems with the platform, release version and reproduction steps.
Leave private conversations, identity keys and server credentials out of reports.
