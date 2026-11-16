use super::storage::wide;
use std::mem::size_of;
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{Shell::*, WindowsAndMessaging::*},
};

pub const MESSAGE: u32 = WM_APP + 1;

#[derive(Clone, Copy)]
pub enum Notice {
    Disconnected,
    UtilityRestored,
    OnBattery,
    LowBattery,
    PowerPending,
    PowerFailed,
}

impl Notice {
    fn localized(self, russian: bool) -> (&'static str, &'static str, bool) {
        let (en, ru, warning) = match self {
            Self::Disconnected => (
                ("UPS disconnected", "Connection to the UPS was lost."),
                ("Нет связи с ИБП", "Соединение с ИБП потеряно."),
                true,
            ),
            Self::UtilityRestored => (
                (
                    "Utility power restored",
                    "The UPS has returned to utility power.",
                ),
                ("Питание восстановлено", "ИБП снова работает от сети."),
                false,
            ),
            Self::OnBattery => (
                ("Battery power", "The UPS is running on battery."),
                ("Питание от батареи", "ИБП работает от батареи."),
                true,
            ),
            Self::LowBattery => (
                ("Low battery", "The UPS reports low battery."),
                ("Низкий заряд", "ИБП сообщает о низком заряде."),
                true,
            ),
            Self::PowerPending => (
                (
                    "Scheduled power action",
                    "A power action is pending. You can cancel it in the RustNUT tray menu.",
                ),
                (
                    "Запланировано действие питания",
                    "Действие можно отменить в меню RustNUT в трее.",
                ),
                true,
            ),
            Self::PowerFailed => (
                (
                    "Power action failed",
                    "Windows could not perform the requested power action.",
                ),
                (
                    "Ошибка действия питания",
                    "Windows не выполнила действие питания.",
                ),
                true,
            ),
        };
        let (title, text) = if russian { ru } else { en };
        (title, text, warning)
    }
}

pub struct Tray {
    window: HWND,
    installed: bool,
    icon: HICON,
    last: Option<String>,
}

impl Tray {
    pub fn new(window: HWND, icon: HICON) -> Self {
        Self {
            window,
            installed: false,
            icon,
            last: None,
        }
    }

    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.window,
            uID: 1,
            ..Default::default()
        }
    }

    pub fn restore(&mut self) {
        self.installed = false;
        if let Some(label) = self.last.take() {
            self.update(&label);
        }
    }

    pub fn update(&mut self, label: &str) {
        if self.installed && self.last.as_deref() == Some(label) {
            return;
        }
        // SAFETY: the notification data and icon remain valid until the shell has copied them.
        unsafe {
            let mut data = self.data();
            data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
            data.uCallbackMessage = MESSAGE;
            data.hIcon = self.icon;
            data.szTip = bounded(&format!("RustNUT · {label}"));
            if !self.installed || Shell_NotifyIconW(NIM_MODIFY, &data) == 0 {
                self.installed = Shell_NotifyIconW(NIM_ADD, &data) != 0;
                if self.installed {
                    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                    Shell_NotifyIconW(NIM_SETVERSION, &data);
                }
            }
        }
        self.last = Some(label.to_owned());
    }

    pub fn notify(&self, notice: Notice, russian: bool) {
        if self.installed {
            let (title, text, warning) = notice.localized(russian);
            let mut data = self.data();
            data.uFlags = NIF_INFO | NIF_REALTIME;
            data.dwInfoFlags =
                NIIF_RESPECT_QUIET_TIME | if warning { NIIF_WARNING } else { NIIF_INFO };
            data.szInfoTitle = bounded(&format!("RustNUT · {title}"));
            data.szInfo = bounded(text);
            // SAFETY: data lives through the call and contains no borrowed pointers.
            unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
        }
    }

    pub fn dismiss_notice(&self) {
        if self.installed {
            let mut data = self.data();
            data.uFlags = NIF_INFO;
            // SAFETY: an empty szInfo tells Explorer to dismiss the notification for this icon.
            unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: the tray entry is owned by this window and the module owns its shared icon.
        unsafe { Shell_NotifyIconW(NIM_DELETE, &self.data()) };
    }
}

fn bounded<const N: usize>(text: &str) -> [u16; N] {
    let mut result = [0; N];
    let units = wide(text);
    let mut n = (units.len() - 1).min(N - 1);
    if n > 0 && (0xd800..=0xdbff).contains(&units[n - 1]) {
        n -= 1;
    }
    result[..n].copy_from_slice(&units[..n]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_notice_has_a_title_and_message_in_the_selected_language() {
        for notice in [
            Notice::Disconnected,
            Notice::UtilityRestored,
            Notice::OnBattery,
            Notice::LowBattery,
            Notice::PowerPending,
            Notice::PowerFailed,
        ] {
            let (en_title, en_text, en_warning) = notice.localized(false);
            let (ru_title, ru_text, ru_warning) = notice.localized(true);
            for text in [en_title, en_text] {
                assert!(text.is_ascii() && !text.is_empty());
            }
            for text in [ru_title, ru_text] {
                assert!(text.chars().any(|c| ('\u{0400}'..='\u{04ff}').contains(&c)));
            }
            assert_eq!(en_warning, ru_warning);
        }
    }
}
