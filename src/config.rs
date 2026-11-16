use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Transport {
    #[default]
    StartTls,
    Plain,
    DirectTls,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Action {
    #[default]
    Shutdown,
    Sleep,
    Hibernate,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerOptions {
    pub action: Action,
    pub runtime: u64,
    pub delay: u64,
    pub extra_delay: u64,
    pub extra_enabled: bool,
    pub immediate: bool,
    pub on_battery: bool,
    pub force_close: bool,
    pub fsd: bool,
}

impl Default for PowerOptions {
    fn default() -> Self {
        Self {
            action: Action::Shutdown,
            runtime: 0,
            delay: 30,
            extra_delay: 15,
            extra_enabled: false,
            immediate: false,
            on_battery: false,
            force_close: false,
            fsd: false,
        }
    }
}

impl PowerOptions {
    pub fn wait(&self) -> u64 {
        if self.immediate {
            0
        } else {
            self.delay
                + if self.extra_enabled {
                    self.extra_delay
                } else {
                    0
                }
        }
    }
}

pub const RANGE_KEYS: [&str; 5] = [
    "input.voltage",
    "input.frequency",
    "output.voltage",
    "battery.voltage",
    "ups.load",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Range {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl Range {
    pub fn excludes(self, value: f64) -> bool {
        self.min.is_some_and(|n| value < n) || self.max.is_some_and(|n| value > n)
    }

    fn valid(self) -> bool {
        self.min
            .into_iter()
            .chain(self.max)
            .all(|n| (0.0..=100_000.0).contains(&n))
            && !self.min.zip(self.max).is_some_and(|(min, max)| min > max)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub enabled: bool,
    pub nominal_hz: u8,
    pub ranges: [Range; RANGE_KEYS.len()],
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            enabled: true,
            nominal_hz: 0,
            ranges: [
                (90.0, 264.0),
                (45.0, 65.0),
                (90.0, 264.0),
                (9.0, 60.0),
                (0.0, 100.0),
            ]
            .map(|(min, max)| Range {
                min: Some(min),
                max: Some(max),
            }),
        }
    }
}

impl Limits {
    pub fn complete(&mut self) {
        for (range, default) in self.ranges.iter_mut().zip(Self::default().ranges) {
            let min = range
                .min
                .unwrap_or_else(|| default.min.unwrap().min(range.max.unwrap_or(f64::MAX)));
            let max = range.max.unwrap_or_else(|| default.max.unwrap().max(min));
            *range = Range {
                min: Some(min),
                max: Some(max),
            };
        }
    }

    pub fn range(&self, key: &str) -> Option<Range> {
        if !self.enabled {
            return None;
        }
        let index = RANGE_KEYS.iter().position(|&item| item == key)?;
        let range = self.ranges[index];
        if key == "input.frequency"
            && (range == Range::default() || range == Self::default().ranges[1])
            && matches!(self.nominal_hz, 50 | 60)
        {
            let nominal = f64::from(self.nominal_hz);
            return Some(Range {
                min: Some(nominal - 5.0),
                max: Some(nominal + 5.0),
            });
        }
        Some(range)
    }

    pub fn flags(&self, key: &str, value: f64) -> bool {
        self.range(key).is_some_and(|range| range.excludes(value))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub ups: String,
    pub transport: Transport,
    pub user: String,
    pub secret: Vec<u8>,
    pub poll: u64,
    pub russian: bool,
    pub dark: bool,
    #[serde(default)]
    // Existing files without this field retain their explicit light/dark setting.
    pub system_theme: bool,
    pub notifications: bool,
    pub shutdown: bool,
    pub charge: u8,
    pub battery_seconds: u64,
    pub power: PowerOptions,
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 3493,
            ups: String::new(),
            transport: Transport::StartTls,
            user: String::new(),
            secret: Vec::new(),
            poll: 5,
            russian: false,
            dark: false,
            system_theme: true,
            notifications: true,
            shutdown: false,
            charge: 15,
            battery_seconds: 0,
            power: PowerOptions::default(),
            limits: Limits::default(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.host.len() > 253
            || self
                .host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || self.port == 0
        {
            return Err("Invalid server address or port.");
        }
        if self.ups.len() > 255
            || self.ups.chars().any(char::is_control)
            || self.user.len() > 255
            || self.user.chars().any(char::is_control)
        {
            return Err("Invalid UPS name or username.");
        }
        if self.user.is_empty() != self.secret.is_empty() || self.secret.len() > 8192 {
            return Err("Provide both a username and a password, or leave both empty.");
        }
        if self.transport == Transport::Plain && !self.user.is_empty() {
            return Err("Authentication requires TLS.");
        }
        if !(2..=60).contains(&self.poll) || self.charge > 100 || self.battery_seconds > 86400 {
            return Err("Polling must be 2–60 s, charge 0–100%, and battery duration 0–86400 s.");
        }
        if [self.power.runtime, self.power.delay, self.power.extra_delay]
            .into_iter()
            .any(|n| n > 86400)
        {
            return Err("Power thresholds and delays must be 0–86400 s.");
        }
        if !matches!(self.limits.nominal_hz, 0 | 50 | 60)
            || !self.limits.ranges.iter().all(|range| range.valid())
        {
            return Err("Check telemetry limits. Minimum cannot exceed maximum.");
        }
        if self.shutdown
            && (self.host.is_empty()
                || self.ups.is_empty()
                || (self.charge == 0
                    && self.battery_seconds == 0
                    && self.power.runtime == 0
                    && !self.power.on_battery
                    && !self.power.fsd))
        {
            return Err("Power actions require a server, a named UPS and at least one trigger.");
        }
        Ok(())
    }
}
