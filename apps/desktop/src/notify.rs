// SPDX-License-Identifier: EUPL-1.2
//! Desktop notifications for messages that arrive while Nuschl is in the
//! background (direct messages, your voice channel, mentions of your name).
//! Linux uses the freedesktop notification service over D-Bus, with an
//! image's thumbnail attached. Where the bus offers only the portals (Flatpak
//! and other sandboxes), notes go through the notification portal with
//! Nuschl's icon. Other systems are not implemented yet.

/// One notification. Message text is untrusted: the service's body is escaped,
/// since notification servers may interpret markup there, and the portal's
/// body is plain text; the summary is plain text by specification and only
/// loses control characters.
pub struct Note {
    /// Replaces the previous notification with the same key (one per conversation).
    pub key: String,
    pub summary: String,
    pub body: String,
    /// RGBA8 thumbnail.
    pub image: Option<(u32, u32, Vec<u8>)>,
}

/// Largest thumbnail side, as for images in the timeline.
const MAX_THUMBNAIL_SIDE: u32 = 4096;

/// A message image's RGBA pixels for a notification, if its size is plausible
/// and matches the pixel data. The size is computed in `usize`, so a hostile
/// width and height cannot wrap around to match a short buffer.
pub fn thumbnail(width: u32, height: u32, rgba: &str) -> Option<(u32, u32, Vec<u8>)> {
    use base64::Engine as _;
    let sides = 1..=MAX_THUMBNAIL_SIDE;
    if !sides.contains(&width) || !sides.contains(&height) {
        return None;
    }
    let pixels = base64::engine::general_purpose::STANDARD
        .decode(rgba)
        .ok()?;
    (pixels.len() == width as usize * height as usize * 4).then_some((width, height, pixels))
}

/// Escape the characters notification markup gives meaning to.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in unformatted(text) {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Body text without control characters other than line breaks.
fn unformatted(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars().filter(|&c| c == '\n' || !c.is_control())
}

/// Plain text for the summary: no control characters or line breaks.
pub fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .chars()
        .take(120)
        .collect()
}

/// Start the notification sender; notes go to it through the returned channel.
#[cfg(target_os = "linux")]
pub fn start(handle: &tokio::runtime::Handle) -> tokio::sync::mpsc::Sender<Note> {
    let (sender, notes) = tokio::sync::mpsc::channel::<Note>(16);
    handle.spawn(async move {
        match zbus::Connection::session().await {
            Ok(connection) => send(&connection, notes).await,
            Err(error) => tracing::warn!(%error, "no session bus: desktop notifications are off"),
        }
    });
    sender
}

/// Replies that mean the bus has no notification service.
#[cfg(target_os = "linux")]
const NO_SERVICE: [&str; 2] = [
    "org.freedesktop.DBus.Error.ServiceUnknown",
    "org.freedesktop.DBus.Error.NameHasNoOwner",
];

/// Shows each note through the notification service. When the bus has no
/// such service, this note and every later one go through the notification
/// portal.
#[cfg(target_os = "linux")]
async fn send(connection: &zbus::Connection, mut notes: tokio::sync::mpsc::Receiver<Note>) {
    let mut replaces = std::collections::HashMap::<String, u32>::new();
    let mut portal = false;
    while let Some(Note {
        key,
        summary,
        body,
        image,
    }) = notes.recv().await
    {
        if !portal {
            let previous = replaces.get(&key).copied().unwrap_or(0);
            match notify(connection, previous, &summary, &body, image).await {
                Ok(id) => {
                    replaces.insert(key, id);
                    continue;
                }
                Err(zbus::Error::MethodError(name, ..)) if NO_SERVICE.contains(&name.as_str()) => {
                    tracing::info!(%name, "desktop notifications go through the notification portal");
                    portal = true;
                }
                Err(error) => {
                    tracing::warn!(%error, "desktop notification failed");
                    continue;
                }
            }
        }
        if let Err(error) = add_notification(connection, &key, &summary, &body).await {
            tracing::warn!(%error, "desktop notification failed");
        }
    }
}

/// Shows a note through the freedesktop notification service in place of
/// notification `replaces` (0: none) and returns its ID.
#[cfg(target_os = "linux")]
async fn notify(
    connection: &zbus::Connection,
    replaces: u32,
    summary: &str,
    body: &str,
    image: Option<(u32, u32, Vec<u8>)>,
) -> zbus::Result<u32> {
    use zbus::zvariant::Value;
    let mut hints = std::collections::HashMap::<&str, Value>::new();
    hints.insert("desktop-entry", Value::from("org.nuschl.Nuschl"));
    hints.insert("category", Value::from("im.received"));
    if let Some((width, height, rgba)) = image {
        let image = (
            width as i32,
            height as i32,
            (width * 4) as i32,
            true,
            8_i32,
            4_i32,
            rgba,
        );
        hints.insert("image-data", Value::from(image));
    }
    connection
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Nuschl",
                replaces,
                "org.nuschl.Nuschl",
                plain(summary),
                escape(body),
                Vec::<&str>::new(),
                hints,
                -1_i32,
            ),
        )
        .await?
        .body()
        .deserialize()
}

/// Shows a note through the notification portal. Its ID is the note's key,
/// so a later note with the same key replaces it.
#[cfg(target_os = "linux")]
async fn add_notification(
    connection: &zbus::Connection,
    key: &str,
    summary: &str,
    body: &str,
) -> zbus::Result<()> {
    use zbus::zvariant::Value;
    let notification = std::collections::HashMap::from([
        ("title", Value::from(plain(summary))),
        ("body", Value::from(unformatted(body).collect::<String>())),
        ("priority", Value::from("normal")),
        (
            "icon",
            Value::from(("themed", Value::from(vec!["org.nuschl.Nuschl"]))),
        ),
    ]);
    connection
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Notification"),
            "AddNotification",
            &(key, notification),
        )
        .await
        .map(drop)
}

#[cfg(not(target_os = "linux"))]
pub fn start(_: &tokio::runtime::Handle) -> tokio::sync::mpsc::Sender<Note> {
    // Windows toasts and macOS user notifications need platform bindings; not yet.
    tokio::sync::mpsc::channel::<Note>(1).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_text_cannot_inject_markup() {
        assert_eq!(
            escape("<img src=\"https://tracker.invalid/x\"> & <b>"),
            "&lt;img src=&quot;https://tracker.invalid/x&quot;&gt; &amp; &lt;b&gt;"
        );
    }

    #[test]
    fn thumbnails_match_their_pixel_data() {
        use base64::Engine as _;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let pixels = encode(&[0; 2 * 3 * 4]);
        assert_eq!(
            thumbnail(2, 3, &pixels).map(|(w, h, _)| (w, h)),
            Some((2, 3))
        );
        assert!(thumbnail(3, 3, &pixels).is_none());
        assert!(thumbnail(0, 3, &pixels).is_none());
        // 65536 * 65536 * 4 wraps to 0 in u32; the old check accepted an empty image.
        assert!(thumbnail(65536, 65536, "").is_none());
        assert!(thumbnail(1 << 30, 4, &encode(&[0; 16])).is_none());
        assert!(thumbnail(1, 1, "not base64!").is_none());
    }

    #[test]
    fn summaries_are_one_plain_line() {
        assert_eq!(plain("Eve\nin #general\u{7}"), "Eve in #general");
        assert_eq!(plain("a & <b>"), "a & <b>");
    }

    /// The sender on a private session bus, against fake services.
    #[cfg(target_os = "linux")]
    mod bus {
        use super::super::{Note, send};
        use std::collections::{BTreeMap, HashMap};
        use tokio::sync::mpsc::UnboundedSender;
        use zbus::zvariant::OwnedValue;

        /// A notification as a fake service received it.
        #[derive(Debug, PartialEq)]
        enum Shown {
            /// The notification service's replaced ID, summary and body.
            Service(u32, String, String),
            /// The portal's notification ID and fields.
            Portal(String, BTreeMap<String, String>),
        }

        struct Service(UnboundedSender<Shown>);

        #[zbus::interface(name = "org.freedesktop.Notifications")]
        impl Service {
            #[allow(clippy::too_many_arguments)]
            fn notify(
                &self,
                _app_name: String,
                replaces_id: u32,
                _app_icon: String,
                summary: String,
                body: String,
                _actions: Vec<String>,
                _hints: HashMap<String, OwnedValue>,
                _expire_timeout: i32,
            ) -> u32 {
                let _ = self.0.send(Shown::Service(replaces_id, summary, body));
                7
            }
        }

        struct Portal(UnboundedSender<Shown>);

        #[zbus::interface(name = "org.freedesktop.portal.Notification")]
        impl Portal {
            fn add_notification(&self, id: String, notification: HashMap<String, OwnedValue>) {
                let fields = notification
                    .into_iter()
                    .map(|(key, value)| (key, value.to_string()))
                    .collect();
                let _ = self.0.send(Shown::Portal(id, fields));
            }
        }

        /// A private session bus with no service files, so nothing from the
        /// host starts on it.
        struct Bus {
            daemon: std::process::Child,
            address: String,
            _directory: nuschl_testkit::TempDir,
        }

        impl Bus {
            fn start() -> Self {
                use std::io::BufRead as _;
                let directory = nuschl_testkit::temp_dir();
                let config = directory.path().join("bus.conf");
                std::fs::write(
                    &config,
                    format!(
                        "<busconfig><type>session</type><listen>unix:path={}</listen>\
                         <auth>EXTERNAL</auth><policy context=\"default\">\
                         <allow send_destination=\"*\"/><allow receive_sender=\"*\"/><allow own=\"*\"/>\
                         </policy></busconfig>",
                        directory.path().join("bus").display()
                    ),
                )
                .unwrap();
                let mut daemon = std::process::Command::new(nuschl_testkit::program("dbus-daemon"))
                    .args(["--nofork", "--nopidfile", "--print-address"])
                    .arg(format!("--config-file={}", config.display()))
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .expect("start dbus-daemon");
                // The daemon prints its address once it listens.
                let mut address = String::new();
                std::io::BufReader::new(daemon.stdout.take().unwrap())
                    .read_line(&mut address)
                    .unwrap();
                assert!(!address.is_empty(), "dbus-daemon exited");
                Bus {
                    daemon,
                    address: address.trim().into(),
                    _directory: directory,
                }
            }

            fn connect(&self) -> zbus::connection::Builder<'_> {
                zbus::connection::Builder::address(self.address.as_str()).unwrap()
            }
        }

        impl Drop for Bus {
            fn drop(&mut self) {
                let _ = self.daemon.kill();
                let _ = self.daemon.wait();
            }
        }

        /// Sends notes one by one on a bus with the fake portal and, with
        /// `service`, the fake notification service; returns what they showed.
        fn show(service: bool, notes: [(&str, &str); 2]) -> Vec<Shown> {
            let bus = Bus::start();
            let exchange = async {
                let (sent, mut shown) = tokio::sync::mpsc::unbounded_channel();
                let mut fakes = bus
                    .connect()
                    .name("org.freedesktop.portal.Desktop")
                    .unwrap()
                    .serve_at("/org/freedesktop/portal/desktop", Portal(sent.clone()))
                    .unwrap();
                if service {
                    fakes = fakes
                        .name("org.freedesktop.Notifications")
                        .unwrap()
                        .serve_at("/org/freedesktop/Notifications", Service(sent))
                        .unwrap();
                }
                let _fakes = fakes.build().await.unwrap();
                let connection = bus.connect().build().await.unwrap();
                let (sender, receiver) = tokio::sync::mpsc::channel(16);
                tokio::spawn(async move { send(&connection, receiver).await });
                let mut result = Vec::new();
                for (summary, body) in notes {
                    let note = Note {
                        key: "server:1".into(),
                        summary: summary.into(),
                        body: body.into(),
                        image: None,
                    };
                    sender.send(note).await.unwrap();
                    result.push(shown.recv().await.unwrap());
                }
                result
            };
            let limit = std::time::Duration::from_secs(120);
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async { tokio::time::timeout(limit, exchange).await })
                .expect("the fake services show both notes")
        }

        #[test]
        fn notes_go_through_the_portal_without_a_notification_service() {
            let portal = |body: &str| {
                let fields = [
                    ("body", body),
                    ("icon", r#"("themed", <["org.nuschl.Nuschl"]>)"#),
                    ("priority", "\"normal\""),
                    ("title", "\"Eve\""),
                ];
                let fields = fields.map(|(key, value)| (key.to_owned(), value.to_owned()));
                Shown::Portal("server:1".into(), BTreeMap::from(fields))
            };
            assert_eq!(
                show(false, [("Eve\u{7}", "a & <b>\nok\u{1b}"), ("Eve", "again")]),
                // The same key replaces the notification.
                [portal("\"a & <b>\\nok\""), portal("\"again\"")]
            );
        }

        #[test]
        fn notes_go_through_the_notification_service_where_it_runs() {
            assert_eq!(
                show(true, [("Eve", "a & <b>"), ("Eve", "again")]),
                [
                    Shown::Service(0, "Eve".into(), "a &amp; &lt;b&gt;".into()),
                    Shown::Service(7, "Eve".into(), "again".into()),
                ]
            );
        }
    }
}
