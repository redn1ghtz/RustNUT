use crate::{
    config::Config,
    nut::{Power, Values, has_fsd, number, power},
};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    None,
    Warn,
    ShutDown,
}

#[derive(Default)]
pub struct Policy {
    last: Option<Instant>,
    battery_since: Option<Instant>,
    pending: Option<Instant>,
    fsd: bool,
    qualifying: bool,
    held: bool,
}

impl Policy {
    pub fn interrupted(&mut self) {
        self.last = None;
        self.battery_since = None;
        self.pending = None;
        self.qualifying = false;
    }

    pub fn cancel(&mut self) {
        self.interrupted();
        self.held = true;
    }

    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn sample(
        &mut self,
        cfg: &Config,
        ups: &str,
        values: &Values,
        at: Instant,
        now: Instant,
    ) -> Decision {
        let gap = Duration::from_secs(cfg.poll.clamp(2, 60) + 6);
        if !cfg.shutdown
            || cfg.host.is_empty()
            || cfg.ups.is_empty()
            || cfg.ups != ups
            || now.saturating_duration_since(at) > Duration::from_secs(3)
        {
            self.interrupted();
            return Decision::None;
        }
        if self
            .last
            .is_some_and(|last| at <= last || at.duration_since(last) > gap)
        {
            self.interrupted();
        }
        let state = power(values);
        let on_battery = cfg.power.on_battery && matches!(state, Power::Battery | Power::Critical);
        let fsd = cfg.power.fsd && has_fsd(values);
        if !fsd && !matches!(state, Power::Battery | Power::Critical) {
            self.interrupted();
            if state == Power::Line {
                self.held = false;
            }
            return Decision::None;
        }
        if self.held {
            return Decision::None;
        }
        if self.fsd != fsd {
            self.interrupted();
            self.fsd = fsd;
        }
        let previous = self.last.is_some();
        self.last = Some(at);
        let since = *self.battery_since.get_or_insert(at);
        let elapsed = at.duration_since(since);
        let low = cfg.charge > 0
            && number(values, "battery.charge").is_some_and(|n| n <= f64::from(cfg.charge));
        let long = cfg.battery_seconds > 0 && elapsed >= Duration::from_secs(cfg.battery_seconds);
        let short_runtime = cfg.power.runtime > 0
            && number(values, "battery.runtime").is_some_and(|n| n <= cfg.power.runtime as f64);
        let qualifying =
            on_battery || fsd || (!cfg.power.on_battery && (low || long || short_runtime));
        let previously_qualifying = std::mem::replace(&mut self.qualifying, qualifying);
        if !qualifying {
            self.pending = None;
            return Decision::None;
        }
        if !previous && !on_battery {
            return Decision::None;
        }
        let wait = Duration::from_secs(cfg.power.wait());
        if wait.is_zero() {
            if !on_battery && !previously_qualifying {
                return Decision::None;
            }
            self.cancel();
            return Decision::ShutDown;
        }
        match self.pending {
            None => {
                self.pending = Some(now.max(at));
                Decision::Warn
            }
            Some(start) if at.saturating_duration_since(start) >= wait => {
                self.cancel();
                Decision::ShutDown
            }
            Some(_) => Decision::None,
        }
    }
}
