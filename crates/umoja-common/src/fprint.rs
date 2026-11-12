//! fprintd over D-Bus (`net.reactivated.Fprint`), for fingerprint unlock
//! and login the way gnome-shell does it: is there a reader, has this user
//! enrolled fingers, and verify while the password field stays usable.
//!
//! Holding a claim on the reader also keeps any `pam_fprintd` line in a
//! password PAM stack (e.g. common-auth with pam-auth-update's fprintd
//! profile) from grabbing the reader and making the password wait: its
//! Claim fails at once and PAM moves on to the password.

use gio::prelude::*;

const BUS_NAME: &str = "net.reactivated.Fprint";
const MANAGER_PATH: &str = "/net/reactivated/Fprint/Manager";
const MANAGER: &str = "net.reactivated.Fprint.Manager";
const DEVICE: &str = "net.reactivated.Fprint.Device";
const TIMEOUT_MS: i32 = 5000;

/// GNOME's `org.gnome.login-screen enable-fingerprint-authentication`
/// (shipped by gdm3; missing schema means on, as in GDM).
pub fn enabled_by_settings() -> bool {
    crate::gsettings::get_bool(
        "org.gnome.login-screen",
        "enable-fingerprint-authentication",
        true,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanType {
    Press,
    Swipe,
}

/// Outcome of one VerifyStatus signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verify {
    Match,
    NoMatch,
    /// Scan problem ("verify-retry-scan", "verify-swipe-too-short"...);
    /// the reader keeps waiting when `done` is false.
    Retry {
        result: String,
        done: bool,
    },
    /// The reader went away or failed; give up on it.
    Gone,
}

impl Verify {
    pub fn parse(result: &str, done: bool) -> Self {
        match result {
            "verify-match" => Self::Match,
            "verify-no-match" => Self::NoMatch,
            "verify-disconnected" | "verify-unknown-error" => Self::Gone,
            r => Self::Retry {
                result: r.to_string(),
                done,
            },
        }
    }
}

/// A short, gnome-shell-worded hint for a scan problem.
pub fn retry_message(result: &str, scan: ScanType) -> String {
    use crate::i18n::shell;
    match (result, scan) {
        ("verify-swipe-too-short", _) => shell("Swipe was too short, try again"),
        ("verify-finger-not-centered", _) => shell("Your finger was not centered, try again"),
        ("verify-remove-and-retry", _) => shell("Remove your finger and try again"),
        (_, ScanType::Swipe) => shell("Swipe your finger again"),
        (_, ScanType::Press) => shell("Place your finger on the fingerprint reader"),
    }
}

/// gnome-shell's hint under the password field while the reader listens.
pub fn or_hint(scan: ScanType) -> String {
    match scan {
        ScanType::Press => crate::i18n::shell("(or place finger on reader)"),
        ScanType::Swipe => crate::i18n::shell("(or swipe finger across reader)"),
    }
}

#[derive(Clone)]
pub struct Device {
    proxy: gio::DBusProxy,
}

impl Device {
    /// fprintd's default reader, or `None` (no fprintd, no reader).
    /// Starting fprintd is left to D-Bus activation.
    pub async fn default() -> Option<Self> {
        let conn = gio::bus_get_future(gio::BusType::System).await.ok()?;
        let reply = conn
            .call_future(
                Some(BUS_NAME),
                MANAGER_PATH,
                MANAGER,
                "GetDefaultDevice",
                None,
                Some(glib::VariantTy::new("(o)").unwrap()),
                gio::DBusCallFlags::NONE,
                TIMEOUT_MS,
            )
            .await
            .map_err(|e| tracing::debug!(error = %e, "no fingerprint reader"))
            .ok()?;
        let path = reply.child_value(0).str()?.to_string();
        let proxy = gio::DBusProxy::for_bus_future(
            gio::BusType::System,
            gio::DBusProxyFlags::DO_NOT_AUTO_START,
            None,
            BUS_NAME,
            &path,
            DEVICE,
        )
        .await
        .ok()?;
        Some(Self { proxy })
    }

    pub fn scan_type(&self) -> ScanType {
        match self
            .proxy
            .cached_property("scan-type")
            .and_then(|v| v.str().map(String::from))
            .as_deref()
        {
            Some("swipe") => ScanType::Swipe,
            _ => ScanType::Press,
        }
    }

    async fn call(
        &self,
        method: &str,
        args: Option<glib::Variant>,
    ) -> Result<glib::Variant, glib::Error> {
        self.proxy
            .call_future(method, args.as_ref(), gio::DBusCallFlags::NONE, TIMEOUT_MS)
            .await
    }

    /// Fingers `user` ("" = the caller) has enrolled on this reader.
    pub async fn enrolled_fingers(&self, user: &str) -> Vec<String> {
        match self
            .call("ListEnrolledFingers", Some((user,).to_variant()))
            .await
        {
            Ok(v) => v.get::<(Vec<String>,)>().map(|(f,)| f).unwrap_or_default(),
            // net.reactivated.Fprint.Error.NoEnrolledPrints is the usual one.
            Err(e) => {
                tracing::debug!(error = %e, "no enrolled fingers");
                Vec::new()
            }
        }
    }

    pub async fn claim(&self, user: &str) -> Result<(), glib::Error> {
        self.call("Claim", Some((user,).to_variant()))
            .await
            .map(|_| ())
    }

    pub async fn release(&self) {
        let _ = self.call("Release", None).await;
    }

    pub async fn verify_start(&self) -> Result<(), glib::Error> {
        self.call("VerifyStart", Some(("any",).to_variant()))
            .await
            .map(|_| ())
    }

    pub async fn verify_stop(&self) {
        let _ = self.call("VerifyStop", None).await;
    }

    /// `VerifyStatus(result, done)` from the reader.
    pub fn connect_verify_status(&self, f: impl Fn(Verify) + 'static) -> glib::SignalHandlerId {
        self.proxy.connect_local("g-signal", false, move |args| {
            let signal = args.get(2)?.get::<String>().ok()?;
            if signal != "VerifyStatus" {
                return None;
            }
            let params = args.get(3)?.get::<glib::Variant>().ok()?;
            let (result, done) = params.get::<(String, bool)>()?;
            f(Verify::parse(&result, done));
            None
        })
    }

    pub fn disconnect(&self, id: glib::SignalHandlerId) {
        self.proxy.disconnect(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verify_results() {
        assert_eq!(Verify::parse("verify-match", true), Verify::Match);
        assert_eq!(Verify::parse("verify-no-match", true), Verify::NoMatch);
        assert_eq!(Verify::parse("verify-disconnected", true), Verify::Gone);
        assert_eq!(
            Verify::parse("verify-retry-scan", false),
            Verify::Retry {
                result: "verify-retry-scan".into(),
                done: false
            }
        );
    }
}
