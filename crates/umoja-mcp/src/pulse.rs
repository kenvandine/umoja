//! A short-lived PulseAudio client (served by pipewire-pulse) for the
//! sound tools: the same interface the panel and gnome-shell's libgvc use,
//! run synchronously on a worker thread with libpulse's own main loop.

use libpulse_binding as pa;
use pa::callbacks::ListResult;
use pa::context::{Context, FlagSet, State};
use pa::mainloop::standard::{IterateResult, Mainloop};
use pa::operation::{Operation, State as OpState};
use pa::volume::{ChannelVolumes, Volume};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// The server's names for whatever is the default right now.
const DEFAULT_SINK: &str = "@DEFAULT_SINK@";
const DEFAULT_SOURCE: &str = "@DEFAULT_SOURCE@";

/// A sink (output) or source (input), not counting sinks' monitors.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub name: String,
    pub description: String,
    pub default: bool,
    pub input: bool,
    pub volume: ChannelVolumes,
    pub muted: bool,
}

impl Device {
    /// The level as gnome-shell's slider and `wpctl get-volume` show it:
    /// the loudest channel, in percent of 100%.
    pub fn percent(&self) -> u32 {
        percent(&self.volume)
    }
}

pub fn percent(v: &ChannelVolumes) -> u32 {
    let max = if v.len() > 0 { v.max().0 } else { 0 };
    (max as f64 / Volume::NORMAL.0 as f64 * 100.0).round() as u32
}

/// `v` scaled so its loudest channel is at `pct` percent, keeping the
/// balance between channels as libgvc does.
pub fn scaled(mut v: ChannelVolumes, pct: u32) -> ChannelVolumes {
    let target = Volume((Volume::NORMAL.0 as f64 * pct as f64 / 100.0).round() as u32);
    if v.len() == 0 {
        v.set(2, target);
    } else if v.max().0 == 0 {
        let n = v.len();
        v.set(n, target);
    } else {
        v.scale(target);
    }
    v
}

pub struct Client {
    // Declared first so it's freed before its main loop.
    context: Context,
    mainloop: Mainloop,
}

impl Client {
    pub fn connect() -> Result<Self, String> {
        let mut mainloop = Mainloop::new().ok_or("couldn't start a PulseAudio main loop")?;
        let mut context =
            Context::new(&mainloop, "Umoja assistant").ok_or("couldn't create a PulseAudio context")?;
        context
            .connect(None, FlagSet::NOAUTOSPAWN, None)
            .map_err(|e| format!("couldn't reach the sound server: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match context.get_state() {
                State::Ready => break,
                State::Failed | State::Terminated => {
                    return Err("the sound server (pipewire-pulse) isn't running".into());
                }
                _ => {}
            }
            if Instant::now() > deadline {
                return Err("the sound server didn't answer".into());
            }
            if let IterateResult::Quit(_) | IterateResult::Err(_) = mainloop.iterate(true) {
                return Err("the sound server connection broke".into());
            }
        }
        Ok(Self { context, mainloop })
    }

    fn wait<F: ?Sized>(&mut self, op: Operation<F>) -> Result<(), String> {
        while op.get_state() == OpState::Running {
            if let IterateResult::Quit(_) | IterateResult::Err(_) = self.mainloop.iterate(true) {
                return Err("the sound server connection broke".into());
            }
        }
        Ok(())
    }

    /// The outputs (or inputs), the default one marked.
    pub fn devices(&mut self, input: bool) -> Result<Vec<Device>, String> {
        let defaults = Rc::new(RefCell::new((None::<String>, None::<String>)));
        let op = {
            let d = defaults.clone();
            self.context.introspect().get_server_info(move |i| {
                *d.borrow_mut() = (
                    i.default_sink_name.as_ref().map(|n| n.to_string()),
                    i.default_source_name.as_ref().map(|n| n.to_string()),
                );
            })
        };
        self.wait(op)?;
        let (sink, source) = defaults.take();
        let default = if input { source } else { sink };
        let list = Rc::new(RefCell::new(Vec::new()));
        let item = {
            let default = default.clone();
            move |name: Option<&str>, description: Option<&str>, volume, muted| {
                let name = name.unwrap_or_default().to_string();
                Device {
                    default: Some(&name) == default.as_ref(),
                    description: description.map(str::to_string).unwrap_or_else(|| name.clone()),
                    name,
                    input,
                    volume,
                    muted,
                }
            }
        };
        let intro = self.context.introspect();
        if input {
            let l = list.clone();
            let op = intro.get_source_info_list(move |r| {
                if let ListResult::Item(i) = r
                    && i.monitor_of_sink.is_none()
                {
                    l.borrow_mut()
                        .push(item(i.name.as_deref(), i.description.as_deref(), i.volume, i.mute));
                }
            });
            self.wait(op)?;
        } else {
            let l = list.clone();
            let op = intro.get_sink_info_list(move |r| {
                if let ListResult::Item(i) = r {
                    l.borrow_mut()
                        .push(item(i.name.as_deref(), i.description.as_deref(), i.volume, i.mute));
                }
            });
            self.wait(op)?;
        }
        Ok(list.take())
    }

    /// The default output (or input).
    pub fn default_device(&mut self, input: bool) -> Result<Device, String> {
        self.devices(input)?
            .into_iter()
            .find(|d| d.default)
            .ok_or_else(|| format!("there is no {} device", if input { "input" } else { "output" }))
    }

    fn done(ok: Rc<RefCell<bool>>) -> Option<Box<dyn FnMut(bool)>> {
        Some(Box::new(move |s| *ok.borrow_mut() = s))
    }

    pub fn set_mute(&mut self, input: bool, muted: bool) -> Result<(), String> {
        let ok = Rc::new(RefCell::new(false));
        let mut intro = self.context.introspect();
        let op = if input {
            intro.set_source_mute_by_name(DEFAULT_SOURCE, muted, Self::done(ok.clone()))
        } else {
            intro.set_sink_mute_by_name(DEFAULT_SINK, muted, Self::done(ok.clone()))
        };
        self.wait(op)?;
        ok.take().then_some(()).ok_or_else(|| "the sound server refused the mute change".into())
    }

    pub fn set_volume(&mut self, input: bool, volume: &ChannelVolumes) -> Result<(), String> {
        let ok = Rc::new(RefCell::new(false));
        let mut intro = self.context.introspect();
        let op = if input {
            intro.set_source_volume_by_name(DEFAULT_SOURCE, volume, Self::done(ok.clone()))
        } else {
            intro.set_sink_volume_by_name(DEFAULT_SINK, volume, Self::done(ok.clone()))
        };
        self.wait(op)?;
        ok.take().then_some(()).ok_or_else(|| "the sound server refused the volume change".into())
    }

    pub fn set_default(&mut self, input: bool, name: &str) -> Result<(), String> {
        let ok = Rc::new(RefCell::new(false));
        let op = {
            let ok = ok.clone();
            let cb = move |s| *ok.borrow_mut() = s;
            if input {
                self.context.set_default_source(name, cb)
            } else {
                self.context.set_default_sink(name, cb)
            }
        };
        self.wait(op)?;
        ok.take().then_some(()).ok_or_else(|| "the sound server refused the device change".into())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.context.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_keeps_balance() {
        let mut v = ChannelVolumes::default();
        v.set(2, Volume(Volume::NORMAL.0 / 2));
        v.get_mut()[1] = Volume(Volume::NORMAL.0 / 4);
        assert_eq!(percent(&v), 50);
        let s = scaled(v, 80);
        assert_eq!(percent(&s), 80);
        assert!((s.get()[1].0 as i64 * 2 - s.get()[0].0 as i64).abs() <= 2, "right stays half of left");
        assert_eq!(percent(&scaled(ChannelVolumes::default(), 30)), 30);
    }
}
