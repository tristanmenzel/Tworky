use std::collections::HashMap;
use std::fs;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_lite::{FutureExt, StreamExt};
use serde::Serialize;
use zbus::blocking::connection::Builder;
use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type;
use zbus::zvariant::{DynamicDeserialize, DynamicType, OwnedObjectPath, OwnedValue, Value};
use zbus::{MatchRule, MessageStream};

use super::{LockKind, PowerSource};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const REQUEST: &str = "org.freedesktop.portal.Request";
const DBUS: &str = "org.freedesktop.DBus";
const SYSFS_POWER_SUPPLY: &str = "/sys/class/power_supply";
/// The portal's `Inhibit` flag for suspend.
const INHIBIT_SUSPEND: u32 = 4;
const TIMEOUT: Duration = Duration::from_secs(2);

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(0);

pub struct SleepLock {
    held: Held,
    reason: String,
}

enum Held {
    Portal(PortalInhibit),
    Logind { _fd: OwnedFd },
}

/// The portal holds an inhibit until `Request.Close` on its path, or until this connection drops.
struct PortalInhibit {
    connection: Connection,
    request: Option<OwnedObjectPath>,
    owner: String,
}

impl Drop for PortalInhibit {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            let _ =
                self.connection
                    .call_method(Some(PORTAL), &request, Some(REQUEST), "Close", &());
        }
    }
}

trait Portal {
    fn inhibit(&self, reason: &str) -> Result<PortalInhibit>;
}

trait Logind {
    fn inhibit(&self, reason: &str) -> Result<OwnedFd>;
}

struct DesktopPortal;

impl Portal for DesktopPortal {
    fn inhibit(&self, reason: &str) -> Result<PortalInhibit> {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        if !desktop_supports_portal(&desktop) {
            bail!("the portal isn't used on desktop {desktop:?}");
        }
        let connection = connect(Builder::session()?)?;
        start_portal(&connection)?;
        let owner = portal_owner(&connection)?;
        let sender = connection
            .unique_name()
            .context("no unique name on the session bus")?;
        let token = format!(
            "tworky_{}_{}",
            std::process::id(),
            NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
        );
        let expected = request_path(sender.as_str(), &token);
        let mut responses = subscribe(&connection, &owner, &expected)?;
        let deadline = Instant::now() + TIMEOUT;
        let options = HashMap::from([
            ("reason", Value::from(reason)),
            ("handle_token", Value::from(token.as_str())),
        ]);
        let request: OwnedObjectPath = call(
            &connection,
            PORTAL,
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Inhibit",
            "Inhibit",
            &("", INHIBIT_SUSPEND, options),
        )
        .context("portal Inhibit")?;
        if request.as_str() != expected {
            responses = subscribe(&connection, &owner, request.as_str())?;
        }
        let inhibit = PortalInhibit {
            connection,
            request: Some(request),
            owner,
        };
        match wait_for_response(responses, deadline) {
            Some(0) => Ok(inhibit),
            Some(code) => bail!("the portal refused the inhibit (response {code})"),
            None => bail!("the portal didn't answer the inhibit request"),
        }
    }
}

/// The portal emits `Response` on the request path it derives from our unique name and token,
/// possibly before `Inhibit` returns, so the match must be in place before the call.
fn request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
}

fn subscribe(connection: &Connection, owner: &str, path: &str) -> Result<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(owner)?
        .interface(REQUEST)?
        .member("Response")?
        .path(path)?
        .build();
    Ok(MessageIterator::for_match_rule(rule, connection, None)?.into_inner())
}

fn wait_for_response(mut responses: MessageStream, deadline: Instant) -> Option<u32> {
    let response = async {
        while let Some(message) = responses.next().await {
            let Ok(message) = message else { continue };
            if let Ok((code, _)) = message
                .body()
                .deserialize::<(u32, HashMap<String, OwnedValue>)>()
            {
                return Some(code);
            }
        }
        None
    };
    let timeout = async {
        async_io::Timer::at(deadline).await;
        None
    };
    async_io::block_on(response.or(timeout))
}

struct SystemLogind;

impl Logind for SystemLogind {
    fn inhibit(&self, reason: &str) -> Result<OwnedFd> {
        let connection = connect(Builder::system()?)?;
        let fd: zbus::zvariant::OwnedFd = call(
            &connection,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "Inhibit",
            &("sleep", "Tworky", reason, "block"),
        )
        .context("logind Inhibit")?;
        Ok(fd.into())
    }
}

impl SleepLock {
    pub fn acquire(reason: &str) -> Result<SleepLock> {
        Ok(SleepLock {
            held: acquire_with(&DesktopPortal, &SystemLogind, reason)?,
            reason: reason.to_owned(),
        })
    }

    pub fn kind(&self) -> LockKind {
        match self.held {
            Held::Portal(_) => LockKind::Portal,
            Held::Logind { .. } => LockKind::Logind,
        }
    }

    /// A restarted portal has silently dropped the inhibit, so take it again.
    pub fn refresh(&mut self) -> Result<()> {
        let Held::Portal(inhibit) = &mut self.held else {
            return Ok(());
        };
        let current = portal_owner(&inhibit.connection).ok();
        if !owner_changed(&inhibit.owner, current.as_deref()) {
            return Ok(());
        }
        // The old request path means nothing to the new portal, and could even name a new
        // request, so it is never closed.
        inhibit.request = None;
        self.held = acquire_with(&DesktopPortal, &SystemLogind, &self.reason)?;
        Ok(())
    }
}

fn acquire_with(portal: &dyn Portal, logind: &dyn Logind, reason: &str) -> Result<Held> {
    match portal.inhibit(reason) {
        Ok(inhibit) => Ok(Held::Portal(inhibit)),
        Err(portal_error) => logind
            .inhibit(reason)
            .map(|fd| Held::Logind { _fd: fd })
            .with_context(|| format!("portal: {portal_error:#}")),
    }
}

fn owner_changed(taken_from: &str, current: Option<&str>) -> bool {
    current != Some(taken_from)
}

/// GNOME's and KDE's portal backends enforce a suspend inhibit; others may accept it and ignore it.
fn desktop_supports_portal(xdg_current_desktop: &str) -> bool {
    xdg_current_desktop
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("GNOME") || desktop.eq_ignore_ascii_case("KDE"))
}

/// Without a timeout, a portal that is activatable but has no working backend blocks for D-Bus's
/// default of about 25 s.
fn connect(builder: Builder) -> Result<Connection> {
    Ok(builder.method_timeout(Duration::from_secs(2)).build()?)
}

fn call<B, R>(
    connection: &Connection,
    destination: &str,
    path: &str,
    interface: &str,
    method: &str,
    body: &B,
) -> Result<R>
where
    B: Serialize + DynamicType,
    R: for<'d> DynamicDeserialize<'d>,
{
    let reply = connection.call_method(Some(destination), path, Some(interface), method, body)?;
    Ok(reply.body().deserialize()?)
}

fn dbus<B, R>(connection: &Connection, method: &str, body: &B) -> Result<R>
where
    B: Serialize + DynamicType,
    R: for<'d> DynamicDeserialize<'d>,
{
    call(
        connection,
        DBUS,
        "/org/freedesktop/DBus",
        DBUS,
        method,
        body,
    )
}

fn start_portal(connection: &Connection) -> Result<()> {
    if dbus::<_, bool>(connection, "NameHasOwner", &(PORTAL,))? {
        return Ok(());
    }
    let activatable: Vec<String> = dbus(connection, "ListActivatableNames", &())?;
    if !activatable.iter().any(|name| name == PORTAL) {
        bail!("the desktop portal isn't running or activatable");
    }
    dbus::<_, u32>(connection, "StartServiceByName", &(PORTAL, 0u32))?;
    Ok(())
}

fn portal_owner(connection: &Connection) -> Result<String> {
    dbus(connection, "GetNameOwner", &(PORTAL,))
}

pub fn power_source() -> PowerSource {
    match upower_on_battery() {
        Ok(true) => PowerSource::Battery,
        Ok(false) => PowerSource::Ac,
        Err(_) => sysfs_power_source(Path::new(SYSFS_POWER_SUPPLY)),
    }
}

pub fn has_battery() -> bool {
    sysfs_has_battery(Path::new(SYSFS_POWER_SUPPLY))
}

fn upower_on_battery() -> Result<bool> {
    let connection = connect(Builder::system()?)?;
    let value: OwnedValue = call(
        &connection,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower",
        "org.freedesktop.DBus.Properties",
        "Get",
        &("org.freedesktop.UPower", "OnBattery"),
    )?;
    Ok(bool::try_from(value)?)
}

struct Supply {
    kind: String,
    online: bool,
}

/// Peripherals such as a wireless mouse report a `Battery` with scope `Device`; they don't power
/// the system.
fn system_supplies(dir: &Path) -> Vec<Supply> {
    let read = |path: &Path| fs::read_to_string(path).map(|s| s.trim().to_owned()).ok();
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| read(&path.join("scope")).as_deref() != Some("Device"))
        .filter_map(|path| {
            Some(Supply {
                kind: read(&path.join("type"))?,
                online: read(&path.join("online")).as_deref() == Some("1"),
            })
        })
        .collect()
}

fn sysfs_power_source(dir: &Path) -> PowerSource {
    let supplies = system_supplies(dir);
    if supplies.iter().any(|s| s.kind == "Mains" && s.online) {
        PowerSource::Ac
    } else if supplies.iter().any(|s| s.kind == "Battery") {
        PowerSource::Battery
    } else {
        PowerSource::Unknown
    }
}

fn sysfs_has_battery(dir: &Path) -> bool {
    system_supplies(dir).iter().any(|s| s.kind == "Battery")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingPortal;

    impl Portal for FailingPortal {
        fn inhibit(&self, _reason: &str) -> Result<PortalInhibit> {
            bail!("no portal")
        }
    }

    struct FakeLogind(bool);

    impl Logind for FakeLogind {
        fn inhibit(&self, _reason: &str) -> Result<OwnedFd> {
            if !self.0 {
                bail!("no logind");
            }
            Ok(fs::File::open("/dev/null")?.into())
        }
    }

    #[test]
    fn a_failing_portal_falls_back_to_logind() {
        let held = acquire_with(&FailingPortal, &FakeLogind(true), "why").unwrap();
        assert!(matches!(held, Held::Logind { .. }));
    }

    #[test]
    fn both_failing_reports_both() {
        let Err(error) = acquire_with(&FailingPortal, &FakeLogind(false), "why") else {
            panic!("expected an error");
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("no portal") && message.contains("no logind"),
            "{message}"
        );
    }

    #[test]
    fn request_path_follows_the_portal_convention() {
        assert_eq!(
            request_path(":1.42", "tworky_7_0"),
            "/org/freedesktop/portal/desktop/request/1_42/tworky_7_0"
        );
    }

    #[test]
    fn owner_change_means_reacquire() {
        assert!(!owner_changed(":1.5", Some(":1.5")));
        assert!(owner_changed(":1.5", Some(":1.9")));
        assert!(owner_changed(":1.5", None));
    }

    #[test]
    fn portal_only_on_gnome_or_kde() {
        for (desktop, expected) in [
            ("ubuntu:GNOME", true),
            ("KDE", true),
            ("gnome", true),
            ("sway", false),
            ("", false),
        ] {
            assert_eq!(desktop_supports_portal(desktop), expected, "{desktop:?}");
        }
    }

    fn supply(dir: &Path, name: &str, files: &[(&str, &str)]) {
        let path = dir.join(name);
        fs::create_dir(&path).unwrap();
        for (file, contents) in files {
            fs::write(path.join(file), format!("{contents}\n")).unwrap();
        }
    }

    #[test]
    fn sysfs_reads_mains_and_batteries() {
        let laptop = tempfile::tempdir().unwrap();
        supply(laptop.path(), "AC", &[("type", "Mains"), ("online", "1")]);
        supply(
            laptop.path(),
            "BAT0",
            &[("type", "Battery"), ("scope", "System")],
        );
        assert_eq!(sysfs_power_source(laptop.path()), PowerSource::Ac);
        assert!(sysfs_has_battery(laptop.path()));

        fs::write(laptop.path().join("AC/online"), "0\n").unwrap();
        assert_eq!(sysfs_power_source(laptop.path()), PowerSource::Battery);

        let desktop = tempfile::tempdir().unwrap();
        supply(
            desktop.path(),
            "hid-mouse-battery",
            &[("type", "Battery"), ("scope", "Device")],
        );
        assert_eq!(sysfs_power_source(desktop.path()), PowerSource::Unknown);
        assert!(!sysfs_has_battery(desktop.path()));

        let missing = desktop.path().join("missing");
        assert_eq!(sysfs_power_source(&missing), PowerSource::Unknown);
        assert!(!sysfs_has_battery(&missing));
    }
}
