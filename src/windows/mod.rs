mod shutdown;
mod storage;
mod tray;
mod view;

use crate::{
    config::{Action, Config, Limits, PowerOptions, RANGE_KEYS, Range, Transport},
    monitor::{Event, Worker},
    nut::{Error, Power, Values, number, power},
    policy::{Decision, Policy},
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ptr::{null, null_mut},
    sync::{Arc, Mutex},
    time::Instant,
};
use storage::wide;
use tray::Notice;
use view::gauges;
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::{
            GetMonitorInfoW, HDC, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO,
            MonitorFromRect,
        },
        System::{
            LibraryLoader::GetModuleHandleW,
            Threading::{CreateMutexW, INFINITE},
        },
        UI::{
            HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, GetSystemMetricsForDpi},
            Input::KeyboardAndMouse::{VK_ESCAPE, VK_F11},
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

const CLASS: windows_sys::core::PCWSTR = w!("RustNUT.Window");
const ICON_RESOURCE: windows_sys::core::PCWSTR = 1usize as windows_sys::core::PCWSTR;
const WAKE: u32 = WM_APP + 2;
const OPEN: u32 = WM_APP + 3;
const ACTION_ERROR: u32 = WM_APP + 4;
const PICK: u16 = 217;
const EXIT: u16 = 218;
const DEVELOPER_URL: windows_sys::core::PCWSTR = w!("https://github.com/redn1ghtz");
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
const STYLE: u32 = WS_OVERLAPPED
    | WS_CAPTION
    | WS_SYSMENU
    | WS_MINIMIZEBOX
    | WS_MAXIMIZEBOX
    | WS_THICKFRAME
    | WS_CLIPCHILDREN;
const EX_STYLE: u32 = WS_EX_CONTROLPARENT;

fn window_icon(window: HWND, instance: HINSTANCE, dpi: u32) {
    // SAFETY: the shared resource lives for the lifetime of the module and its window.
    unsafe {
        let size = GetSystemMetricsForDpi(SM_CXSMICON, dpi.max(96));
        let icon = LoadImageW(instance, ICON_RESOURCE, IMAGE_ICON, size, size, LR_SHARED);
        if !icon.is_null() {
            SendMessageW(window, WM_SETICON, ICON_SMALL as WPARAM, icon as LPARAM);
        }
    }
}

fn monitor_info(bounds: &RECT) -> Option<MONITORINFO> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: Windows reads bounds and fills the initialized monitor info.
    unsafe {
        let monitor = MonitorFromRect(bounds, MONITOR_DEFAULTTONEAREST);
        (GetMonitorInfoW(monitor, &mut info) != 0).then_some(info)
    }
}

fn frame(dpi: u32) -> RECT {
    let mut frame = RECT::default();
    // SAFETY: frame is initialized and writable.
    unsafe {
        AdjustWindowRectExForDpi(&mut frame, STYLE, 0, EX_STYLE, dpi.max(72));
    }
    frame
}

fn layout_dpi(dpi: u32, bounds: &RECT) -> i32 {
    let dpi = dpi.max(96);
    let chrome = frame(dpi);
    monitor_info(bounds).map_or(dpi as i32, |info| {
        let width = info.rcWork.right - info.rcWork.left - (chrome.right - chrome.left);
        let height = info.rcWork.bottom - info.rcWork.top - (chrome.bottom - chrome.top);
        (dpi as i32)
            .min(width * 96 / view::MIN_WIDTH)
            .min(height * 96 / view::MIN_HEIGHT)
            .max(60)
    })
}

fn fit(window: HWND, bounds: &RECT, monitor_dpi: u32) -> i32 {
    let dpi = layout_dpi(monitor_dpi, bounds);
    let chrome = frame(monitor_dpi);
    let work = monitor_info(bounds).map(|i| i.rcWork).unwrap_or(*bounds);
    let width = (view::scaled(862, dpi) + chrome.right - chrome.left).min(work.right - work.left);
    let height = (view::scaled(746, dpi) + chrome.bottom - chrome.top).min(work.bottom - work.top);
    let x = (work.left + work.right - width) / 2;
    let y = (work.top + work.bottom - height) / 2;
    // SAFETY: the window is owned by this thread.
    unsafe {
        SetWindowPos(
            window,
            null_mut(),
            x,
            y,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    dpi
}

type Inbox = Arc<Mutex<VecDeque<Event>>>;

thread_local! { static APP: RefCell<Option<App>> = const { RefCell::new(None) }; }
thread_local! { static POWER_CHANGE: Cell<Option<bool>> = const { Cell::new(None) }; }

#[derive(Clone, Copy, PartialEq)]
enum Link {
    Idle,
    Connecting,
    Connected,
    Failed(Error, u64),
    Credential,
    Suspended,
}

struct App {
    window: HWND,
    instance: HINSTANCE,
    taskbar: u32,
    dpi: i32,
    fullscreen: Option<WINDOWPLACEMENT>,
    cfg: Config,
    settings: bool,
    shapes: Option<view::shapes::Session>,
    forget: bool,
    worker: Option<Worker>,
    inbox: Inbox,
    tray: tray::Tray,
    policy: Policy,
    ups: String,
    devices: Vec<String>,
    values: Values,
    link: Link,
    last_power: Power,
    was_connected: bool,
    rendered: Option<(Link, bool, bool)>,
    table_dirty: bool,
    shutdown_failed: bool,
}

impl App {
    fn tr(&self, en: &'static str, ru: &'static str) -> &'static str {
        if self.cfg.russian { ru } else { en }
    }

    fn restart(&mut self) {
        self.worker = None;
        self.inbox = Inbox::default();
        self.policy.interrupted();
        self.clear_values();
        self.devices.clear();
        self.ups.clear();
        self.last_power = Power::Unknown;
        self.was_connected = false;
        self.shutdown_failed = false;
        self.link = Link::Idle;
        if !self.cfg.host.is_empty() {
            match storage::decrypt(&self.cfg) {
                Ok(password) => {
                    let inbox = self.inbox.clone();
                    let handle = self.window as usize;
                    self.worker = Worker::start(self.cfg.clone(), password, move |event| {
                        let wake = {
                            let mut queue = inbox.lock().unwrap();
                            let was_empty = queue.is_empty();
                            if queue.len() == 32 {
                                queue.clear();
                                queue.push_back(Event::Connecting);
                            }
                            queue.push_back(event);
                            was_empty
                        };
                        if wake {
                            // SAFETY: PostMessage passes only integer payloads across threads.
                            unsafe { PostMessageW(handle as HWND, WAKE, 0, 0) };
                        }
                    })
                    .ok();
                    self.link = if self.worker.is_some() {
                        Link::Connecting
                    } else {
                        Link::Failed(Error::Connect, 0)
                    };
                }
                Err(_) => self.link = Link::Credential,
            }
        }
        self.refresh();
    }

    fn clear_values(&mut self) {
        self.table_dirty |= !self.values.is_empty();
        self.values.clear();
    }

    fn state(&self) -> Power {
        if self.link == Link::Connected {
            power(&self.values)
        } else {
            Power::Unknown
        }
    }

    fn status(&self, state: Power) -> &'static str {
        match self.link {
            Link::Idle => self.tr("Set up your connection", "Настройте подключение"),
            Link::Connecting => self.tr("Connecting…", "Подключение…"),
            Link::Credential => self.tr("Password unavailable", "Пароль недоступен"),
            Link::Suspended => self.tr("Monitoring paused", "Мониторинг приостановлен"),
            Link::Failed(..) => self.tr("Disconnected", "Нет соединения"),
            Link::Connected => match state {
                Power::Line => self.tr("On utility power", "Питание от сети"),
                Power::Battery => self.tr("On battery", "Питание от батареи"),
                Power::Critical => self.tr("Battery low", "Низкий заряд батареи"),
                Power::Unknown if crate::nut::has_fsd(&self.values) => {
                    self.tr("Server requested FSD", "Сигнал FSD от сервера")
                }
                Power::Unknown => self.tr("Power state unknown", "Питание неизвестно"),
            },
        }
    }

    fn error_name(&self, error: Error) -> &str {
        match error {
            Error::Resolve => self.tr("Name resolution failed", "Не удалось разрешить имя сервера"),
            Error::Connect => self.tr("Could not connect", "Не удалось подключиться"),
            Error::Timeout => self.tr("Server timed out", "Сервер не ответил вовремя"),
            Error::Disconnected => self.tr("Connection closed", "Соединение закрыто"),
            Error::Protocol => self.tr("Invalid NUT response", "Некорректный ответ NUT"),
            Error::Tls => self.tr(
                "TLS or certificate validation failed",
                "Ошибка TLS или проверки сертификата",
            ),
            Error::Authentication => self.tr("Authentication failed", "Ошибка авторизации"),
            Error::Stale => self.tr("UPS data is stale", "Данные ИБП устарели"),
            Error::UnknownUps => self.tr("UPS is unavailable", "ИБП недоступен"),
            Error::Rejected => self.tr("Server rejected the request", "Сервер отклонил запрос"),
        }
    }

    fn detail(&self) -> String {
        if self.shutdown_failed {
            return self
                .tr(
                    "Windows could not perform the requested power action.",
                    "Windows не выполнила действие питания.",
                )
                .into();
        }
        if self.policy.pending() {
            return self
                .tr(
                    "Power action pending. You can cancel it from this window or the tray.",
                    "Ожидается действие питания. Его можно отменить в окне или в трее.",
                )
                .into();
        }
        match self.link {
            Link::Idle => self
                .tr(
                    "Open Settings to enter the address of your NUT server.",
                    "Укажите адрес NUT-сервера в настройках.",
                )
                .into(),
            Link::Failed(error, seconds) => format!(
                "{}  ·  {} {seconds} {}",
                self.error_name(error),
                self.tr("retry in", "повтор через"),
                self.tr("s", "с")
            ),
            Link::Credential => self
                .tr(
                    "Enter the password again in Settings.",
                    "Заново введите пароль в настройках.",
                )
                .into(),
            Link::Suspended => self
                .tr(
                    "Waiting for Windows to resume",
                    "Ожидание выхода Windows из сна",
                )
                .into(),
            Link::Connecting => format!("{}:{}", self.cfg.host, self.cfg.port),
            Link::Connected => {
                let transport = if self.cfg.transport == Transport::Plain {
                    "TCP"
                } else {
                    "TLS"
                };
                let model =
                    crate::nut::value(&self.values, &["device.model", "ups.model"]).unwrap_or("");
                let device = if model.is_empty() {
                    self.ups.clone()
                } else {
                    format!("{model} · {}", self.ups)
                };
                format!(
                    "{device}\r\n{}:{} · {transport}",
                    self.cfg.host, self.cfg.port
                )
            }
        }
    }

    fn no_data(&self) -> &'static str {
        self.tr("No data", "Нет данных")
    }

    fn refresh(&mut self) {
        let state = self.state();
        let status = self.status(state);
        let label = if self.link == Link::Connected {
            let charge = self.metric("battery.charge", " %");
            format!("{status} · {charge}")
        } else {
            status.into()
        };
        self.tray.update(&label);
        if self.settings || !view::visible_for_update(self.window) {
            return;
        }
        let pending = self.policy.pending();
        let dashboard = (self.link, pending, self.shutdown_failed);
        if !self.table_dirty && self.rendered == Some(dashboard) {
            return;
        }
        let volts = self.tr(" V", " В");
        let units = [volts, volts, self.tr(" Hz", " Гц"), " %", " %", volts];
        let frequency = crate::nut::frequency_key(&self.values);
        let keys = [
            "input.voltage",
            "output.voltage",
            frequency,
            "battery.charge",
            "ups.load",
            "battery.voltage",
        ];
        let meters = std::array::from_fn(|i| {
            let key = keys[i];
            let reading =
                number(&self.values, key).filter(|&n| n <= 100_000.0 && (i != 3 || n <= 100.0));
            let (low, high) = match i {
                0 | 1 => (0.0, 270.0),
                2 => self
                    .cfg
                    .limits
                    .range(key)
                    .and_then(|r| r.min.zip(r.max))
                    .filter(|(min, max)| max > min)
                    .unwrap_or((40.0, 70.0)),
                5 => (6.0, 20.0),
                _ => (0.0, 100.0),
            };
            gauges::Meter {
                reading,
                low,
                high,
                flagged: reading.is_some_and(|n| self.cfg.limits.flags(key, n)),
            }
        });
        view::set_meters(self.window, state, meters);
        let names = [
            self.tr("Input voltage", "Входное напряжение"),
            self.tr("Output voltage", "Выходное напряжение"),
            if frequency == "output.frequency" {
                self.tr("Output frequency", "Частота на выходе")
            } else {
                self.tr("Input frequency", "Частота на входе")
            },
            self.tr("Battery charge", "Заряд батареи"),
            self.tr("Load", "Нагрузка"),
            self.tr("Battery voltage", "Напряжение батареи"),
        ];
        for (i, meter) in meters.into_iter().enumerate() {
            let reading = format_reading(meter.reading, units[i], self.no_data());
            let note = if i == 4 {
                let watts = crate::nut::real_power(&self.values);
                if watts.is_some() || meter.reading.is_some() {
                    format_reading(watts, self.tr(" W", " Вт"), self.no_data())
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            view::text(
                self.window,
                gauges::VALUE_IDS[i],
                &format!("{}\n{reading}\n{note}", names[i]),
            );
        }
        view::text(self.window, view::STATUS, status);
        view::text(
            self.window,
            view::DETAIL,
            &self.detail().replace("\r\n", " · "),
        );
        let identity = if self.link == Link::Connected {
            let maker = crate::nut::value(&self.values, &["device.mfr", "ups.mfr"])
                .unwrap_or(self.no_data());
            let serial = crate::nut::value(&self.values, &["device.serial", "ups.serial"])
                .unwrap_or(self.no_data());
            format!(
                "{} {maker}  ·  {} {serial}",
                self.tr("Manufacturer", "Производитель"),
                self.tr("Serial no.", "Серийный №")
            )
        } else {
            String::new()
        };
        view::text(self.window, gauges::IDENTITY, &identity);
        view::text(
            self.window,
            gauges::RUNTIME_TEXT,
            &format!("{}  {}", self.tr("Runtime", "Автономность"), self.runtime()),
        );
        if self.table_dirty {
            let mut flagged = 0;
            let no_data = self.no_data();
            view::set_list(self.window, self.values.keys(), |key, row| {
                let flag = self.cfg.limits.range(key).is_some_and(|range| {
                    number(&self.values, key).is_some_and(|n| range.excludes(n))
                });
                flagged += usize::from(flag);
                let val = crate::nut::value(&self.values, &[key]).unwrap_or(no_data);
                if flag {
                    row.push_str("! ");
                }
                row.push_str(key);
                row.push('\t');
                row.push_str(val);
            });
            let summary = if flagged > 0 {
                format!("{} {flagged}", self.tr("Out of range ·", "Вне диапазона ·"))
            } else if self.values.is_empty() {
                self.tr("Waiting for UPS readings", "Ожидание показаний ИБП")
                    .to_owned()
            } else {
                String::new()
            };
            view::text(self.window, view::SUMMARY, &summary);
            self.table_dirty = false;
        }
        // SAFETY: the button is a direct child of the dashboard.
        unsafe {
            if self
                .rendered
                .is_none_or(|(_, was_pending, _)| was_pending != pending)
            {
                ShowWindow(
                    view::child(self.window, view::CANCEL),
                    if pending { SW_SHOWNA } else { SW_HIDE },
                );
            }
        }
        self.rendered = Some(dashboard);
    }

    fn metric(&self, key: &str, unit: &str) -> String {
        format_reading(
            number(&self.values, key).filter(|n| *n <= 100000.0),
            unit,
            self.no_data(),
        )
    }

    fn runtime(&self) -> String {
        let Some(seconds) = number(&self.values, "battery.runtime").filter(|n| *n <= 31_536_000.0)
        else {
            return self.no_data().into();
        };
        let seconds = seconds as u64;
        format!(
            "{:02}:{:02}:{:02}",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    }

    fn drain(&mut self) {
        let events = std::mem::take(&mut *self.inbox.lock().unwrap());
        let mut notice = None;
        let mut events = events.into_iter().peekable();
        while let Some(event) = events.next() {
            match event {
                Event::Connecting => {
                    self.policy.interrupted();
                    self.link = Link::Connecting;
                    self.clear_values();
                }
                Event::Devices(devices) => self.devices = devices,
                Event::Failed(error, seconds) => {
                    self.policy.interrupted();
                    self.link = Link::Failed(error, seconds);
                    self.clear_values();
                    if self.was_connected && self.cfg.notifications {
                        notice = Some(Notice::Disconnected);
                    }
                    self.was_connected = false;
                    self.last_power = Power::Unknown;
                }
                Event::Data { ups, values, at } => {
                    self.link = Link::Connected;
                    if self.ups != ups {
                        self.ups = ups;
                        self.rendered = None;
                    }
                    if self.values != values {
                        self.values = values;
                        self.table_dirty = true;
                    }
                    let state = self.state();
                    if self.cfg.notifications && self.last_power != state {
                        notice = match state {
                            Power::Line if self.last_power != Power::Unknown => {
                                Some(Notice::UtilityRestored)
                            }
                            Power::Battery => Some(Notice::OnBattery),
                            Power::Critical => Some(Notice::LowBattery),
                            _ => notice,
                        };
                    }
                    if state == Power::Line {
                        self.shutdown_failed = false;
                    }
                    self.last_power = state;
                    self.was_connected = true;
                    // An older sample may have been superseded while a modal dialog
                    // was open. Only the latest pending sample can arm or act.
                    if events.peek().is_some() {
                        self.policy.interrupted();
                        continue;
                    }
                    match self
                        .policy
                        .sample(&self.cfg, &self.ups, &self.values, at, Instant::now())
                    {
                        Decision::None => {}
                        Decision::Warn => notice = Some(Notice::PowerPending),
                        Decision::ShutDown => {
                            let handle = self.window as usize;
                            let action = self.cfg.power.action;
                            let force_close = self.cfg.power.force_close;
                            self.shutdown_failed = std::thread::Builder::new()
                                .name("windows-power".into())
                                .spawn(move || {
                                    if shutdown::request(action, force_close).is_err() {
                                        // SAFETY: the posted message carries no pointers.
                                        unsafe { PostMessageW(handle as HWND, ACTION_ERROR, 0, 0) };
                                    }
                                })
                                .is_err();
                        }
                    }
                }
            }
        }
        self.refresh();
        if let Some(notice) = notice {
            self.tray.notify(notice, self.cfg.russian);
        }
    }

    fn dark_theme(&self) -> bool {
        if self.cfg.system_theme {
            storage::system_dark()
        } else {
            self.cfg.dark
        }
    }

    fn update_theme(&self) {
        if self.shapes.is_some() {
            view::install(self.window, self.dark_theme(), self.dpi);
        }
    }

    fn page(&mut self, settings: bool) {
        self.shapes.get_or_insert_with(view::shapes::Session::start);
        let dark = self.dark_theme();
        view::without_redraw(self.window, || {
            view::clear(self.window);
            view::install(self.window, dark, self.dpi);
            self.settings = settings;
            view::set_page(self.window, !settings);
            self.rendered = None;
            self.table_dirty = true;
            let layout = view::Layout::new(self.window, self.instance, self.dpi, dark);
            if settings {
                self.forget = false;
                self.form(&layout);
                self.sync_controls();
            } else {
                layout.dashboard(|en, ru| self.tr(en, ru));
            }
            self.refresh();
        });
    }

    fn toggle_fullscreen(&mut self) {
        let window = self.window;
        // SAFETY: this thread owns the window and all placement structures are initialized.
        unsafe {
            if let Some(placement) = self.fullscreen.take() {
                SetWindowLongW(window, GWL_STYLE, STYLE as i32);
                SetWindowPlacement(window, &placement);
                SetWindowPos(
                    window,
                    null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            } else {
                let mut bounds = RECT::default();
                GetWindowRect(window, &mut bounds);
                let Some(monitor) = monitor_info(&bounds) else {
                    return;
                };
                let mut placement = WINDOWPLACEMENT {
                    length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                    ..Default::default()
                };
                if GetWindowPlacement(window, &mut placement) == 0 {
                    return;
                }
                self.fullscreen = Some(placement);
                if IsZoomed(window) != 0 {
                    ShowWindow(window, SW_RESTORE);
                }
                SetWindowLongW(
                    window,
                    GWL_STYLE,
                    (WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN | WS_MINIMIZEBOX | WS_SYSMENU) as i32,
                );
                let r = monitor.rcMonitor;
                SetWindowPos(
                    window,
                    null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
        }
        view::resize(window);
    }

    fn sync_controls(&self) {
        if !self.settings {
            return;
        }
        let window = self.window;
        let secure = view::choice(window, view::TLS) != 2;
        let named = !view::read(window, view::HOST).trim().is_empty()
            && !view::read(window, view::UPS).trim().is_empty();
        let auto = named && view::checked(window, view::SHUTDOWN);
        let action = view::choice(window, view::ACTION);
        let force_available = auto && action == 0;
        let battery_label = match action {
            1 => self.tr("Sleep on battery power", "Сон при работе от батареи"),
            2 => self.tr(
                "Hibernate on battery power",
                "Гибернация при работе от батареи",
            ),
            _ => self.tr(
                "Shut down PC on battery power",
                "Выключать ПК при работе от батареи",
            ),
        };
        view::text(window, view::ON_BATTERY, battery_label);
        let on_battery = view::checked(window, view::ON_BATTERY);
        let delay = auto && !view::checked(window, view::IMMEDIATE);
        let extra = delay && view::checked(window, view::EXTRA);
        for id in [view::USER, view::SECRET, view::FORGET] {
            view::enable(window, id, secure);
        }
        view::enable(window, view::SHUTDOWN, named);
        let help = if !named {
            self.tr(
                "Enter the NUT server address and the UPS name in Connection.",
                "Укажите адрес сервера NUT и имя ИБП на вкладке «Подключение».",
            )
        } else {
            self.tr(
                "Power actions affect this Windows PC.",
                "Действия питания выполняются на этом компьютере.",
            )
        };
        view::text(window, view::POWER_HELP, help);
        for id in [view::CHARGE, view::RUNTIME, view::SECONDS] {
            view::enable(window, id, auto && !on_battery);
        }
        for id in [view::ON_BATTERY, view::FSD, view::ACTION, view::IMMEDIATE] {
            view::enable(window, id, auto);
        }
        view::enable(window, view::FORCE_CLOSE, force_available);
        for id in [view::DELAY, view::EXTRA] {
            view::enable(window, id, delay);
        }
        view::enable(window, view::EXTRA_DELAY, extra);
        let limits = view::checked(window, view::LIMITS_ENABLED);
        view::enable(window, view::FREQUENCY, limits);
        for id in view::RANGE..view::RANGE + 2 * RANGE_KEYS.len() as u16 {
            view::enable(window, id, limits);
        }
        // SAFETY: parent redraw refreshes the edit frames.
        unsafe {
            InvalidateRect(window, null(), 0);
        }
    }

    fn form(&self, layout: &view::Layout) {
        use view::{FIELD_LABEL, GROUP_NOTE, GROUP_TITLE, Section};
        let t = |en, ru| self.tr(en, ru);
        layout.header(
            t("Settings", "Настройки"),
            t("RustNUT preferences", "Параметры RustNUT"),
        );
        layout.button(view::BACK, t("Overview", "Обзор"), (604, 26, 144, 39));
        for (id, label, x) in [
            (view::CONNECTION_TAB, t("Connection", "Подключение"), 28),
            (view::POWER_TAB, t("Power actions", "Действия питания"), 181),
            (view::LIMITS_TAB, t("Telemetry ranges", "Диапазоны"), 334),
        ] {
            layout.button(id, label, (x, 91, 141, 36));
        }

        layout.section(Section::Connection);
        layout.label(
            GROUP_TITLE,
            t("NUT server", "Сервер NUT"),
            (48, 158, 312, 31),
        );
        layout.field(
            view::HOST,
            t("Server address", "Адрес сервера"),
            &self.cfg.host,
            253,
            (48, 201, 220, 34),
        );
        layout.field(
            view::PORT,
            t("Port", "Порт"),
            &self.cfg.port.to_string(),
            5,
            (280, 201, 80, 34),
        );
        layout.field_label(
            view::TLS,
            t("Security", "Защита соединения"),
            (48, 269, 160, 21),
        );
        layout.combo(
            view::TLS,
            &[
                "STARTTLS",
                t("TLS tunnel", "TLS-туннель"),
                t("Plain TCP", "Обычный TCP"),
            ],
            match self.cfg.transport {
                Transport::StartTls => 0,
                Transport::DirectTls => 1,
                Transport::Plain => 2,
            },
            (48, 292, 160, 130),
        );
        layout.field(
            view::POLL,
            t("Poll interval, s", "Интервал опроса, с"),
            &self.cfg.poll.to_string(),
            2,
            (220, 269, 140, 34),
        );
        layout.field(
            view::UPS,
            t("UPS name (blank = automatic)", "Имя ИБП (пусто = авто)"),
            &self.cfg.ups,
            255,
            (48, 337, 196, 34),
        );
        layout.button(PICK, t("Discover…", "Выбрать…"), (256, 360, 104, 34));
        layout.field(
            view::USER,
            t("Username (optional)", "Имя пользователя"),
            &self.cfg.user,
            255,
            (48, 405, 150, 34),
        );
        layout.field(
            view::SECRET,
            t("Password", "Пароль"),
            "",
            1024,
            (210, 405, 150, 34),
        );
        if !self.cfg.secret.is_empty() {
            view::cue(self.window, view::SECRET, t("Saved", "Сохранён"));
        }
        layout.button(
            view::FORGET,
            t("Clear credentials", "Очистить учётные данные"),
            (48, 482, 220, 35),
        );

        layout.label(
            GROUP_TITLE,
            t("Preferences", "Приложение"),
            (416, 158, 312, 31),
        );
        layout.label(FIELD_LABEL, t("Language", "Язык"), (416, 201, 312, 21));
        layout.combo(
            view::RU,
            &["Русский", "English"],
            usize::from(!self.cfg.russian),
            (416, 224, 312, 112),
        );
        layout.label(FIELD_LABEL, t("Theme", "Тема"), (416, 269, 312, 21));
        layout.combo(
            view::DARK,
            &[
                t("Light", "Светлая"),
                t("Dark", "Тёмная"),
                t("System", "Системная"),
            ],
            if self.cfg.system_theme {
                2
            } else {
                usize::from(self.cfg.dark)
            },
            (416, 292, 312, 112),
        );
        layout.check(
            view::NOTIFY,
            t("Show UPS notifications", "Показывать уведомления"),
            self.cfg.notifications,
            (416, 355, 312, 32),
        );
        layout.check(
            view::START,
            t("Start with Windows", "Запускать с Windows"),
            storage::startup_enabled(),
            (416, 397, 312, 32),
        );
        layout.button(view::GITHUB, "GitHub · redn1ghtz", (416, 482, 220, 35));

        layout.section(Section::Power);
        layout.label(
            GROUP_TITLE,
            t("Triggers", "Условия запуска"),
            (48, 158, 312, 31),
        );
        layout.check(
            view::SHUTDOWN,
            t(
                "Enable automatic power actions",
                "Разрешить автоматические действия",
            ),
            self.cfg.shutdown,
            (48, 197, 312, 33),
        );
        layout.check(
            view::ON_BATTERY,
            "",
            self.cfg.power.on_battery,
            (48, 230, 312, 30),
        );
        layout.field(
            view::CHARGE,
            t("Battery charge at or below, %", "Заряд батареи не выше, %"),
            &self.cfg.charge.to_string(),
            3,
            (48, 268, 312, 34),
        );
        layout.field(
            view::RUNTIME,
            t(
                "Remaining runtime at or below, s",
                "Остаток автономности не больше, с",
            ),
            &self.cfg.power.runtime.to_string(),
            5,
            (48, 336, 312, 34),
        );
        layout.field(
            view::SECONDS,
            t("Time already on battery, s", "Время работы от батареи, с"),
            &self.cfg.battery_seconds.to_string(),
            5,
            (48, 404, 312, 34),
        );
        layout.label(
            GROUP_NOTE,
            t(
                "Set a threshold to 0 to disable it.",
                "Ноль отключает соответствующий порог.",
            ),
            (48, 471, 312, 25),
        );
        layout.check(
            view::FSD,
            t(
                "React to the NUT server's FSD flag",
                "Реагировать на сигнал FSD сервера",
            ),
            self.cfg.power.fsd,
            (48, 504, 312, 34),
        );

        layout.label(
            GROUP_TITLE,
            t("Action and timing", "Действие и задержка"),
            (416, 158, 312, 31),
        );
        layout.field_label(
            view::ACTION,
            t("Windows action", "Действие Windows"),
            (416, 198, 312, 21),
        );
        layout.combo(
            view::ACTION,
            &[
                t("Shut down", "Выключение"),
                t("Sleep", "Сон"),
                t("Hibernate", "Гибернация"),
            ],
            match self.cfg.power.action {
                Action::Shutdown => 0,
                Action::Sleep => 1,
                Action::Hibernate => 2,
            },
            (416, 221, 312, 130),
        );
        layout.check(
            view::IMMEDIATE,
            t("Act without a countdown", "Выполнять без отсчёта"),
            self.cfg.power.immediate,
            (416, 264, 312, 34),
        );
        layout.check(
            view::FORCE_CLOSE,
            t(
                "Shut down without confirmation",
                "Выключать без подтверждения",
            ),
            self.cfg.power.force_close,
            (416, 298, 312, 34),
        );
        layout.field(
            view::DELAY,
            t("Warning delay, s", "Задержка предупреждения, с"),
            &self.cfg.power.delay.to_string(),
            5,
            (416, 340, 312, 34),
        );
        layout.check(
            view::EXTRA,
            t("Add a second delay", "Добавить дополнительную задержку"),
            self.cfg.power.extra_enabled,
            (416, 403, 312, 34),
        );
        layout.field(
            view::EXTRA_DELAY,
            t("Additional delay, s", "Дополнительная задержка, с"),
            &self.cfg.power.extra_delay.to_string(),
            5,
            (416, 440, 312, 34),
        );
        layout.label(view::POWER_HELP, "", (416, 506, 312, 42));

        layout.section(Section::Limits);
        layout.label(
            GROUP_TITLE,
            t("Acceptable telemetry", "Допустимые показания"),
            (48, 158, 360, 31),
        );
        layout.label(
            GROUP_NOTE,
            t(
                "Out-of-range readings are marked on the dashboard.",
                "Отклонения выделяются на панели мониторинга.",
            ),
            (48, 191, 675, 24),
        );
        layout.check(
            view::LIMITS_ENABLED,
            t("Enable telemetry limits", "Включить ограничения"),
            self.cfg.limits.enabled,
            (48, 219, 365, 30),
        );
        layout.field_label(
            view::FREQUENCY,
            t("Nominal mains frequency", "Номинальная частота сети"),
            (48, 260, 300, 28),
        );
        layout.combo(
            view::FREQUENCY,
            &[t("Automatic", "Автоматически"), "50 Hz", "60 Hz"],
            match self.cfg.limits.nominal_hz {
                50 => 1,
                60 => 2,
                _ => 0,
            },
            (440, 253, 288, 116),
        );
        layout.label(FIELD_LABEL, t("Minimum", "Минимум"), (440, 296, 132, 24));
        layout.label(FIELD_LABEL, t("Maximum", "Максимум"), (588, 296, 132, 24));
        let names = [
            t("Input voltage, V", "Входное напряжение, В"),
            t("Input frequency, Hz", "Частота на входе, Гц"),
            t("Output voltage, V", "Выходное напряжение, В"),
            t("Battery voltage, V", "Напряжение батареи, В"),
            t("UPS load, %", "Нагрузка ИБП, %"),
        ];
        for (index, (name, range)) in names.into_iter().zip(&self.cfg.limits.ranges).enumerate() {
            let y = 320 + index as i32 * 37;
            layout.field_label(view::RANGE + (index * 2) as u16, name, (48, y + 7, 390, 25));
            for (column, value) in [range.min, range.max].into_iter().enumerate() {
                layout.boxed_edit(
                    view::RANGE + (index * 2 + column) as u16,
                    &value.map(|v| v.to_string()).unwrap_or_default(),
                    12,
                    (440 + column as i32 * 148, y, 140, 33),
                );
            }
        }
        layout.label(
            GROUP_NOTE,
            t(
                "Default frequency bounds narrow to ±5 Hz at 50/60 Hz.",
                "Для стандартных границ частоты 50/60 Гц допуск составляет ±5 Гц.",
            ),
            (48, 510, 675, 26),
        );

        layout.section(Section::Always);
        layout.label(view::ERROR, "", (28, 552, 720, 38));
        layout.button(
            view::RESET,
            t("Reset settings…", "Сбросить настройки…"),
            (28, 580, 214, 44),
        );
        layout.button(
            view::SAVE,
            t("Save settings", "Сохранить настройки"),
            (534, 580, 214, 44),
        );
        view::show_section(self.window, Section::Connection);
    }

    fn save(&mut self) -> Result<(), &'static str> {
        let window = self.window;
        let t = |en, ru| self.tr(en, ru);
        let read = |id| view::read(window, id).trim().to_owned();
        let checked = |id| view::checked(window, id);
        let numeric = t(
            "Enter valid port, polling interval and shutdown thresholds.",
            "Проверьте порт, интервал опроса и пороги выключения.",
        );
        let host = read(view::HOST);
        let port = read(view::PORT).parse().map_err(|_| numeric)?;
        let seconds = |id| read(id).parse::<u64>().map_err(|_| numeric);
        let bound = |id| {
            let raw = read(id);
            if raw.is_empty() {
                Ok(None)
            } else {
                raw.replace(',', ".").parse::<f64>().map(Some).map_err(|_| {
                    t(
                        "Enter a number or leave the telemetry bound blank.",
                        "Введите число или оставьте границу пустой.",
                    )
                })
            }
        };
        let mut ranges = [Range::default(); RANGE_KEYS.len()];
        for (index, range) in ranges.iter_mut().enumerate() {
            let id = view::RANGE + 2 * index as u16;
            *range = Range {
                min: bound(id)?,
                max: bound(id + 1)?,
            };
        }
        let plain = view::choice(window, view::TLS) == 2;
        let ups = read(view::UPS);
        let password = view::password(window);
        let user = if plain {
            String::new()
        } else {
            read(view::USER)
        };
        let secret = if plain {
            Vec::new()
        } else if !password.is_empty() {
            if password.len() > 1024 || password.chars().any(char::is_control) {
                return Err(t(
                    "Invalid password length or characters.",
                    "Недопустимая длина или символы пароля.",
                ));
            }
            storage::encrypt(&password).map_err(|_| {
                t(
                    "Windows could not encrypt the password.",
                    "Windows не смогла зашифровать пароль.",
                )
            })?
        } else if !self.forget
            && user == self.cfg.user
            && host == self.cfg.host
            && port == self.cfg.port
        {
            self.cfg.secret.clone()
        } else {
            Vec::new()
        };
        let theme = view::choice(window, view::DARK);
        let mut cfg = Config {
            host: host.clone(),
            port,
            ups: ups.clone(),
            transport: match view::choice(window, view::TLS) {
                0 => Transport::StartTls,
                1 => Transport::DirectTls,
                _ => Transport::Plain,
            },
            user,
            secret,
            poll: read(view::POLL).parse().map_err(|_| numeric)?,
            russian: view::choice(window, view::RU) == 0,
            dark: theme == 1,
            system_theme: theme == 2,
            notifications: checked(view::NOTIFY),
            shutdown: !host.is_empty() && !ups.is_empty() && checked(view::SHUTDOWN),
            charge: read(view::CHARGE).parse().map_err(|_| numeric)?,
            battery_seconds: seconds(view::SECONDS)?,
            power: PowerOptions {
                action: match view::choice(window, view::ACTION) {
                    1 => Action::Sleep,
                    2 => Action::Hibernate,
                    _ => Action::Shutdown,
                },
                runtime: seconds(view::RUNTIME)?,
                delay: seconds(view::DELAY)?,
                extra_delay: seconds(view::EXTRA_DELAY)?,
                immediate: checked(view::IMMEDIATE),
                on_battery: checked(view::ON_BATTERY),
                force_close: view::choice(window, view::ACTION) == 0 && checked(view::FORCE_CLOSE),
                extra_enabled: checked(view::EXTRA),
                fsd: checked(view::FSD),
            },
            limits: Limits {
                enabled: checked(view::LIMITS_ENABLED),
                nominal_hz: match view::choice(window, view::FREQUENCY) {
                    1 => 50,
                    2 => 60,
                    _ => 0,
                },
                ranges,
            },
        };
        cfg.validate().map_err(|error| self.config_error(error))?;
        cfg.limits.complete();
        let authority_changed = cfg.host != self.cfg.host
            || cfg.port != self.cfg.port
            || cfg.ups != self.cfg.ups
            || cfg.transport != self.cfg.transport
            || cfg.user != self.cfg.user
            || cfg.secret != self.cfg.secret;
        let protection_changed = authority_changed
            || cfg.charge != self.cfg.charge
            || cfg.battery_seconds != self.cfg.battery_seconds
            || cfg.power != self.cfg.power
            || !self.cfg.shutdown;
        if cfg.shutdown && protection_changed {
            let confirm = if cfg.power.force_close {
                t(
                    "Allow automatic shutdown without confirmation? Windows will forcibly close apps in all sessions on this PC. Unsaved changes may be lost.",
                    "Разрешить автоматическое выключение без подтверждения? Windows принудительно закроет приложения во всех сеансах на этом ПК. Несохранённые изменения могут быть потеряны.",
                )
            } else {
                t(
                    "Allow RustNUT to perform the selected Windows power action automatically when a configured trigger is confirmed?",
                    "Разрешить RustNUT автоматически выполнять выбранное действие питания при подтверждении настроенного условия?",
                )
            };
            if message(window, confirm, MB_YESNO | MB_DEFBUTTON2 | MB_ICONWARNING) != IDYES {
                return Ok(());
            }
        }
        storage::save(&cfg).map_err(|_| {
            t(
                "Could not save settings. Check write access to the folder containing rustnut.exe.",
                "Не удалось сохранить настройки. Проверьте права записи в папку с rustnut.exe.",
            )
        })?;
        let startup = checked(view::START);
        let startup_error =
            startup != storage::startup_enabled() && storage::set_startup(startup).is_err();
        if self.cfg.russian != cfg.russian || !cfg.notifications {
            self.tray.dismiss_notice();
        }
        self.cfg = cfg;
        if authority_changed {
            self.policy = Policy::default();
        }
        self.restart();
        self.page(false);
        if startup_error {
            message(
                window,
                self.tr(
                    "Settings saved. Windows rejected the startup change.",
                    "Настройки сохранены. Windows отклонила изменение автозапуска.",
                ),
                MB_OK | MB_ICONWARNING,
            );
        }
        Ok(())
    }

    fn reset(&mut self) {
        let prompt = self.tr("Reset the settings and clear the saved password? Automatic power actions and startup will be disabled.",
            "Сбросить настройки и удалить сохранённый пароль? Автозапуск и автоматические действия будут отключены.");
        if message(
            self.window,
            prompt,
            MB_YESNO | MB_DEFBUTTON2 | MB_ICONWARNING,
        ) != IDYES
        {
            return;
        }
        if storage::reset().is_err() {
            view::text(
                self.window,
                view::ERROR,
                self.tr(
                    "Could not remove settings.json next to the executable.",
                    "Не удалось удалить settings.json рядом с EXE.",
                ),
            );
            return;
        }
        let startup_error = storage::startup_enabled() && storage::set_startup(false).is_err();
        self.tray.dismiss_notice();
        self.cfg = storage::default_config();
        self.policy = Policy::default();
        self.restart();
        self.page(true);
        if startup_error {
            view::text(
                self.window,
                view::ERROR,
                self.tr(
                    "Settings reset. Windows did not remove the startup entry.",
                    "Настройки сброшены. Windows не удалила запись автозапуска.",
                ),
            );
        }
    }

    fn config_error(&self, error: &'static str) -> &'static str {
        if !self.cfg.russian {
            return error;
        }
        match error {
            "Invalid server address or port." => "Некорректный адрес или порт сервера.",
            "Invalid UPS name or username." => "Некорректное имя ИБП или пользователя.",
            "Provide both a username and a password, or leave both empty." => {
                "Заполните имя пользователя и пароль или оставьте оба поля пустыми."
            }
            "Authentication requires TLS." => "Для авторизации требуется TLS.",
            "Power thresholds and delays must be 0–86400 s." => {
                "Пороги и задержки должны быть в пределах 0–86400 с."
            }
            "Check telemetry limits. Minimum cannot exceed maximum." => {
                "Проверьте диапазоны. Минимум должен быть не выше максимума."
            }
            "Polling must be 2–60 s, charge 0–100%, and battery duration 0–86400 s." => {
                "Опрос 2–60 с, заряд 0–100 %, время от батареи 0–86400 с."
            }
            _ => "Для действий питания нужны адрес сервера, имя ИБП и хотя бы одно условие.",
        }
    }

    fn show(&mut self) {
        if self.shapes.is_none() {
            self.page(false);
        } else if self.cfg.system_theme {
            self.update_theme();
        }
        // SAFETY: the window is owned by this UI thread.
        unsafe {
            ShowWindow(
                self.window,
                if IsIconic(self.window) != 0 {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            SetForegroundWindow(self.window);
        }
        self.refresh();
    }

    fn power_change(&mut self, resume: bool) {
        if resume {
            self.restart();
        } else {
            self.worker = None;
            self.inbox = Inbox::default();
            self.policy.interrupted();
            self.clear_values();
            self.link = Link::Suspended;
            self.refresh();
        }
    }

    fn menu(&mut self, x: i32, y: i32) {
        let mut entries = vec![
            (OPEN as u16, self.tr("Open RustNUT", "Открыть RustNUT")),
            (view::SETTINGS, self.tr("Settings", "Настройки")),
        ];
        if self.policy.pending() {
            entries.push((
                view::CANCEL,
                self.tr("Cancel power action", "Отменить действие"),
            ));
        }
        entries.push((EXIT, self.tr("Exit", "Выход")));
        let id = popup(self.window, x, y, entries);
        self.command(id);
    }

    fn command(&mut self, id: u16) {
        if matches!(
            id,
            view::SHUTDOWN
                | view::ON_BATTERY
                | view::IMMEDIATE
                | view::EXTRA
                | view::LIMITS_ENABLED
        ) {
            self.sync_controls();
            return;
        }
        if self.settings {
            if let Some(section) = view::tab_section(id) {
                view::show_section(self.window, section);
                return;
            }
        }
        match id {
            id if id == OPEN as u16 => self.show(),
            view::SETTINGS => {
                if !self.settings {
                    self.page(true);
                }
                self.show();
            }
            view::BACK => self.page(false),
            view::GITHUB if self.settings => {
                // SAFETY: the built-in HTTPS URL is opened in the default browser after a click.
                let result = unsafe {
                    ShellExecuteW(
                        self.window,
                        w!("open"),
                        DEVELOPER_URL,
                        null(),
                        null(),
                        SW_SHOWNORMAL,
                    )
                };
                if result as isize <= 32 {
                    view::text(self.window, view::ERROR, self.tr(
                        "Could not open the browser. Visit github.com/redn1ghtz.",
                        "Не удалось открыть браузер. Адрес разработчика — github.com/redn1ghtz.",
                    ));
                }
            }
            view::RESET if self.settings => self.reset(),
            view::SAVE => {
                if let Err(error) = self.save() {
                    view::text(self.window, view::ERROR, error);
                }
            }
            view::FORGET => {
                self.forget = true;
                view::text(self.window, view::SECRET, "");
                view::cue(self.window, view::SECRET, "");
                view::text(self.window, view::USER, "");
            }
            PICK => {
                if self.devices.is_empty() {
                    view::text(
                        self.window,
                        view::ERROR,
                        self.tr(
                            "Save the connection first to discover UPS devices.",
                            "Сначала сохраните подключение, чтобы найти ИБП.",
                        ),
                    );
                    return;
                }
                let mut rect = RECT::default();
                // SAFETY: the RECT and button remain valid for this call.
                unsafe { GetWindowRect(view::child(self.window, PICK), &mut rect) };
                let selected = popup(
                    self.window,
                    rect.left,
                    rect.bottom,
                    self.devices
                        .iter()
                        .enumerate()
                        .map(|(i, name)| (i as u16 + 1, name.as_str())),
                );
                if let Some(name) = selected
                    .checked_sub(1)
                    .and_then(|i| self.devices.get(i as usize))
                {
                    view::text(self.window, view::UPS, name);
                    view::text(self.window, view::ERROR, "");
                }
            }
            view::CANCEL if self.policy.pending() => {
                self.policy.cancel();
                self.refresh();
            }
            EXIT => {
                // SAFETY: DestroyWindow is called on the thread that owns the window.
                unsafe { DestroyWindow(self.window) };
            }
            _ => {}
        }
    }
}

fn format_reading(reading: Option<f64>, unit: &str, unavailable: &str) -> String {
    reading.map_or_else(
        || unavailable.into(),
        |n| {
            if n.fract().abs() < 0.001 {
                format!("{n:.0}{unit}")
            } else {
                format!("{n:.1}{unit}")
            }
        },
    )
}

fn message(window: HWND, text: &str, style: u32) -> i32 {
    // SAFETY: the text buffer stays alive for the modal call.
    unsafe { MessageBoxW(window, wide(text).as_ptr(), w!("RustNUT"), style) }
}

fn popup<'a>(
    window: HWND,
    x: i32,
    y: i32,
    entries: impl IntoIterator<Item = (u16, &'a str)>,
) -> u16 {
    // SAFETY: each menu string is copied, and the popup is destroyed before returning.
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return 0;
        }
        for (id, text) in entries {
            AppendMenuW(
                menu,
                MF_STRING,
                id as usize,
                wide(text.replace('&', "&&")).as_ptr(),
            );
        }
        SetForegroundWindow(window);
        let selected = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            x,
            y,
            0,
            window,
            null(),
        );
        PostMessageW(window, WM_NULL, 0, 0);
        DestroyMenu(menu);
        selected as u16
    }
}

unsafe extern "system" fn wndproc(window: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(result) = view::handle(msg, wp, lp) {
        return result;
    }
    // SAFETY: handles refer to this live top-level window. Pointer message data
    // is only inspected for WM_DPICHANGED and remains valid for that call.
    unsafe {
        match msg {
            WM_ERASEBKGND | WM_QUERYENDSESSION => return 1,
            WM_DESTROY => {
                PostQuitMessage(0);
                return 0;
            }
            WM_CLOSE => {
                ShowWindow(window, SW_HIDE);
                return 0;
            }
            WM_SIZE if wp as u32 == SIZE_MINIMIZED => {
                ShowWindow(window, SW_HIDE);
                return 0;
            }
            WM_SIZE => {
                view::resize(window);
                return 0;
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(lp as *mut MINMAXINFO);
                let monitor_dpi = GetDpiForWindow(window);
                let mut bounds = RECT::default();
                GetWindowRect(window, &mut bounds);
                let dpi = layout_dpi(monitor_dpi, &bounds);
                let chrome = frame(monitor_dpi);
                info.ptMinTrackSize.x =
                    view::scaled(view::MIN_WIDTH, dpi) + chrome.right - chrome.left;
                info.ptMinTrackSize.y =
                    view::scaled(view::MIN_HEIGHT, dpi) + chrome.bottom - chrome.top;
                return 0;
            }
            WM_POWERBROADCAST => {
                match wp as u32 {
                    PBT_APMSUSPEND => POWER_CHANGE.set(Some(false)),
                    PBT_APMRESUMEAUTOMATIC => POWER_CHANGE.set(Some(true)),
                    _ => {}
                }
                return 1;
            }
            WAKE => return 0,
            _ => {}
        }
        let handled = APP.with(|slot| {
            let Ok(mut slot) = slot.try_borrow_mut() else {
                return false;
            };
            let Some(app) = slot.as_mut() else {
                return false;
            };
            match msg {
                WM_PAINT => view::paint(window, !app.settings),
                WM_PRINTCLIENT => view::print_background(window, wp as HDC, !app.settings),
                WM_COMMAND if wp >> 16 == 0 => app.command(wp as u16),
                WM_COMMAND
                    if ((wp >> 16) as u32 == 1
                        && matches!(wp as u16, view::TLS | view::ACTION))
                        || ((wp >> 16) as u32 == EN_CHANGE
                            && matches!(wp as u16, view::HOST | view::UPS)) =>
                {
                    app.sync_controls()
                }
                WM_COMMAND if matches!((wp >> 16) as u32, EN_SETFOCUS | EN_KILLFOCUS) => {
                    view::invalidate_edit_frame(window, wp as u16);
                }
                OPEN => app.show(),
                ACTION_ERROR => {
                    app.shutdown_failed = true;
                    app.refresh();
                    app.tray.notify(Notice::PowerFailed, app.cfg.russian);
                }
                tray::MESSAGE => {
                    let action = lp as u16 as u32;
                    match action {
                        WM_CONTEXTMENU => {
                            let mut point = POINT {
                                x: wp as i16 as i32,
                                y: (wp >> 16) as i16 as i32,
                            };
                            if point.x == -1 && point.y == -1 {
                                let icon = NOTIFYICONIDENTIFIER {
                                    cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
                                    hWnd: window,
                                    uID: 1,
                                    ..Default::default()
                                };
                                let mut rect = RECT::default();
                                if Shell_NotifyIconGetRect(&icon, &mut rect) >= 0 {
                                    point = POINT {
                                        x: rect.left,
                                        y: rect.top,
                                    };
                                } else {
                                    GetCursorPos(&mut point);
                                }
                            }
                            app.menu(point.x, point.y);
                        }
                        NIN_SELECT | NIN_KEYSELECT | NIN_BALLOONUSERCLICK => app.show(),
                        _ => {}
                    }
                }
                WM_DPICHANGED => {
                    let rect = &*(lp as *const RECT);
                    app.dpi = layout_dpi(wp as u16 as u32, rect);
                    if app.fullscreen.is_none() {
                        SetWindowPos(
                            window,
                            null_mut(),
                            rect.left,
                            rect.top,
                            rect.right - rect.left,
                            rect.bottom - rect.top,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                    app.update_theme();
                    window_icon(window, app.instance, wp as u16 as u32);
                }
                WM_SETTINGCHANGE if app.cfg.system_theme => app.update_theme(),
                _ if msg == app.taskbar => app.tray.restore(),
                _ => return false,
            }
            true
        });
        if handled {
            0
        } else {
            DefWindowProcW(window, msg, wp, lp)
        }
    }
}

pub fn run() {
    // SAFETY: the main UI thread owns the window, message loop, and native resources.
    unsafe {
        let mutex = CreateMutexW(null(), 0, w!("Local\\RustNUT.Instance"));
        if mutex.is_null() {
            message(
                null_mut(),
                "Could not initialize RustNUT.",
                MB_OK | MB_ICONERROR,
            );
            return;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let other = FindWindowW(CLASS, null());
            if !other.is_null() {
                PostMessageW(other, OPEN, 0, 0);
            }
            CloseHandle(mutex);
            return;
        }
        let instance = GetModuleHandleW(null());
        let icon = LoadIconW(instance, ICON_RESOURCE);
        let class = WNDCLASSW {
            hInstance: instance,
            lpszClassName: CLASS,
            lpfnWndProc: Some(wndproc),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hIcon: icon,
            ..Default::default()
        };
        RegisterClassW(&class);
        let window = CreateWindowExW(
            EX_STYLE,
            CLASS,
            w!("RustNUT"),
            STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            800,
            680,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if window.is_null() {
            message(
                null_mut(),
                "Could not create the RustNUT window.",
                MB_OK | MB_ICONERROR,
            );
            CloseHandle(mutex);
            return;
        }
        window_icon(window, instance, GetDpiForWindow(window));
        let load = storage::load();
        let invalid = load.is_err();
        let cfg = load.unwrap_or_else(|_| storage::default_config());
        let russian = cfg.russian;
        let mut bounds = RECT::default();
        GetWindowRect(window, &mut bounds);
        let dpi = fit(window, &bounds, GetDpiForWindow(window));
        APP.with_borrow_mut(|slot| {
            let mut app = App {
                window,
                instance,
                taskbar: RegisterWindowMessageW(w!("TaskbarCreated")),
                dpi,
                fullscreen: None,
                cfg,
                settings: false,
                shapes: None,
                forget: false,
                worker: None,
                inbox: Inbox::default(),
                tray: tray::Tray::new(window, icon),
                policy: Policy::default(),
                ups: String::new(),
                devices: Vec::new(),
                values: Values::new(),
                link: Link::Idle,
                last_power: Power::Unknown,
                was_connected: false,
                rendered: None,
                table_dirty: true,
                shutdown_failed: false,
            };
            app.restart();
            if !std::env::args_os().any(|arg| arg == "--background") {
                app.show();
            }
            *slot = Some(app);
        });
        if invalid {
            message(
                window,
                if russian {
                    "RustNUT не смог загрузить настройки. Проверьте их перед сохранением нового файла."
                } else {
                    "RustNUT could not load settings. Review them before saving a replacement."
                },
                MB_OK | MB_ICONWARNING,
            );
        }
        let mut msg = MSG::default();
        let mut watching_system = false;
        let mut watch: Option<storage::ThemeWatch> = None;
        loop {
            let system_theme =
                APP.with_borrow(|slot| slot.as_ref().is_some_and(|app| app.cfg.system_theme));
            if system_theme != watching_system {
                watching_system = system_theme;
                watch = system_theme.then(storage::ThemeWatch::new).flatten();
                if watch.is_some() {
                    APP.with_borrow(|slot| {
                        if let Some(app) = slot.as_ref() {
                            app.update_theme();
                        }
                    });
                }
            }
            if let Some(listener) = &watch {
                match MsgWaitForMultipleObjectsEx(
                    1,
                    &listener.event,
                    INFINITE,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                ) {
                    WAIT_OBJECT_0 => {
                        // Arm the next one-shot notification before reading the setting.
                        if !listener.arm() {
                            watch = None;
                        }
                        APP.with_borrow(|slot| {
                            if let Some(app) = slot.as_ref() {
                                app.update_theme();
                            }
                        });
                        continue;
                    }
                    WAIT_FAILED => {
                        watch = None;
                        continue;
                    }
                    _ => {
                        if PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) == 0 {
                            continue;
                        }
                        if msg.message == WM_QUIT {
                            break;
                        }
                    }
                }
            } else if GetMessageW(&mut msg, null_mut(), 0, 0) <= 0 {
                break;
            }
            let full_screen_key = msg.message == WM_KEYDOWN
                && matches!(msg.wParam as u16, VK_F11 | VK_ESCAPE)
                && APP.with_borrow_mut(|slot| {
                    let Some(app) = slot.as_mut() else {
                        return false;
                    };
                    if msg.wParam as u16 == VK_ESCAPE
                        && (app.fullscreen.is_none() || view::combo_open(window))
                    {
                        return false;
                    }
                    if msg.lParam & (1 << 30) == 0 {
                        app.toggle_fullscreen();
                    }
                    true
                });
            if !full_screen_key && IsDialogMessageW(window, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            APP.with_borrow_mut(|slot| {
                if let Some(app) = slot.as_mut() {
                    if let Some(resume) = POWER_CHANGE.take() {
                        app.power_change(resume);
                    }
                    let pending = !app.inbox.lock().unwrap().is_empty();
                    if pending && IsWindow(app.window) != 0 {
                        app.drain();
                    }
                }
            });
        }
        APP.with_borrow_mut(|slot| *slot = None);
        view::cleanup();
        CloseHandle(mutex);
    }
}
