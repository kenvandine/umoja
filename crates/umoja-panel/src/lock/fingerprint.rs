//! Fingerprint unlock, alongside the password like gnome-shell's
//! gdm-fingerprint conversation: while the screen is locked and the user
//! has enrolled fingers, the reader is claimed and listening, the prompt
//! says "(or place finger on reader)", and a match unlocks (after the PAM
//! account checks).
//!
//! The claim is also what keeps a `pam_fprintd` line in common-auth from
//! making the password check wait for the reader.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use umoja_common::fprint::{self, Device, ScanType, Verify};
use umoja_common::i18n;

/// No-match attempts before the reader stops listening (pam_fprintd's
/// default max-tries). Lifting the curtain again re-arms it.
const MAX_FAILURES: u32 = 3;

pub struct Callbacks {
    /// The hint under the password field; `None` hides it.
    pub hint: Box<dyn Fn(Option<String>)>,
    /// A failed or retry message, shown like a wrong password.
    pub error: Box<dyn Fn(String)>,
    /// A finger matched and the account is fine: unlock.
    pub matched: Box<dyn Fn()>,
}

pub struct Fingerprint {
    cb: Callbacks,
    device: RefCell<Option<Device>>,
    handler: RefCell<Option<glib::SignalHandlerId>>,
    scan: Cell<ScanType>,
    verifying: Cell<bool>,
    failures: Cell<u32>,
    /// Bumped by stop(); an in-flight start() that sees a newer value
    /// gives the reader back.
    generation: Cell<u64>,
}

impl Fingerprint {
    pub fn new(cb: Callbacks) -> Rc<Self> {
        Rc::new(Self {
            cb,
            device: RefCell::default(),
            handler: RefCell::default(),
            scan: Cell::new(ScanType::Press),
            verifying: Cell::new(false),
            failures: Cell::new(0),
            generation: Cell::new(0),
        })
    }

    /// Claim the reader and start listening, if the user has enrolled
    /// fingers and fingerprint login isn't turned off.
    pub fn start(self: &Rc<Self>) {
        if self.device.borrow().is_some() || !fprint::enabled_by_settings() {
            return;
        }
        let generation = self.generation.get();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let Some(device) = Device::default().await else {
                return;
            };
            if device.enrolled_fingers("").await.is_empty() || this.stale(generation) {
                return;
            }
            if let Err(e) = device.claim("").await {
                tracing::info!(error = %e, "could not claim the fingerprint reader");
                return;
            }
            if this.stale(generation) {
                device.release().await;
                return;
            }
            this.scan.set(device.scan_type());
            let weak = Rc::downgrade(&this);
            let id = device.connect_verify_status(move |v| {
                if let Some(this) = weak.upgrade() {
                    this.on_status(v);
                }
            });
            *this.handler.borrow_mut() = Some(id);
            *this.device.borrow_mut() = Some(device);
            this.failures.set(0);
            this.verify().await;
        });
    }

    fn stale(&self, generation: u64) -> bool {
        self.generation.get() != generation
    }

    async fn verify(self: &Rc<Self>) {
        let Some(device) = self.device.borrow().clone() else {
            return;
        };
        match device.verify_start().await {
            Ok(()) => {
                self.verifying.set(true);
                (self.cb.hint)(Some(fprint::or_hint(self.scan.get())));
                tracing::info!("fingerprint reader listening");
            }
            Err(e) => {
                tracing::info!(error = %e, "fingerprint VerifyStart failed");
                self.verifying.set(false);
                (self.cb.hint)(None);
            }
        }
    }

    async fn restart(self: &Rc<Self>) {
        let Some(device) = self.device.borrow().clone() else {
            return;
        };
        device.verify_stop().await;
        self.verifying.set(false);
        self.verify().await;
    }

    fn on_status(self: &Rc<Self>, v: Verify) {
        let this = self.clone();
        match v {
            Verify::Match => {
                tracing::info!("fingerprint matched");
                glib::spawn_future_local(async move {
                    this.stop_async().await;
                    let user = glib::user_name().to_string_lossy().into_owned();
                    let ok = gio::spawn_blocking(move || super::pam::account_ok(&user))
                        .await
                        .unwrap_or_else(|_| Err("account check failed".into()));
                    match ok {
                        Ok(()) => (this.cb.matched)(),
                        Err(e) => {
                            tracing::warn!(error = e, "account check after fingerprint failed");
                            (this.cb.error)(e);
                        }
                    }
                });
            }
            Verify::NoMatch => {
                let n = self.failures.get() + 1;
                self.failures.set(n);
                (self.cb.error)(i18n::shell("Sorry, that didn’t work. Please try again."));
                glib::spawn_future_local(async move {
                    if n < MAX_FAILURES {
                        this.restart().await;
                    } else {
                        let Some(device) = this.device.borrow().clone() else {
                            return;
                        };
                        // Keep the claim (see the module docs), stop listening.
                        device.verify_stop().await;
                        this.verifying.set(false);
                        (this.cb.hint)(None);
                    }
                });
            }
            Verify::Retry { result, done } => {
                (self.cb.error)(fprint::retry_message(&result, self.scan.get()));
                if done {
                    glib::spawn_future_local(async move { this.restart().await });
                }
            }
            Verify::Gone => {
                tracing::info!("fingerprint reader gone");
                (self.cb.hint)(None);
                glib::spawn_future_local(async move { this.stop_async().await });
            }
        }
    }

    /// The curtain was lifted again: listen again after too many misses.
    pub fn rearm(self: &Rc<Self>) {
        if self.device.borrow().is_none() {
            self.start();
            return;
        }
        if !self.verifying.get() {
            self.failures.set(0);
            let this = self.clone();
            glib::spawn_future_local(async move { this.verify().await });
        }
    }

    /// After resume the reader may have been reset: start over.
    pub fn reset(self: &Rc<Self>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            this.stop_async().await;
            this.start();
        });
    }

    pub fn stop(self: &Rc<Self>) {
        let this = self.clone();
        glib::spawn_future_local(async move { this.stop_async().await });
    }

    async fn stop_async(&self) {
        self.generation.set(self.generation.get() + 1);
        let device = self.device.borrow_mut().take();
        let handler = self.handler.borrow_mut().take();
        let Some(device) = device else { return };
        if let Some(id) = handler {
            device.disconnect(id);
        }
        if self.verifying.replace(false) {
            device.verify_stop().await;
        }
        device.release().await;
        (self.cb.hint)(None);
    }
}
