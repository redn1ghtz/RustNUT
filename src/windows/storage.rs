use crate::config::Config;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE, LocalFree},
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
    Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    System::{Registry::*, Threading::CreateEventW},
};
use windows_sys::core::w;
use zeroize::{Zeroize, Zeroizing};

const LIMIT: u64 = 64 * 1024;

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

fn path() -> io::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name("settings.json"))
}

pub fn default_config() -> Config {
    Config {
        // SAFETY: no pointers are passed to the parameterless OS query.
        russian: unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() } & 0x3ff
            == 0x19,
        ..Config::default()
    }
}

const PERSONALIZE: windows_sys::core::PCWSTR =
    w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");

pub fn system_dark() -> bool {
    let mut light: u32 = 1;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: the writable buffer holds exactly one DWORD.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PERSONALIZE,
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut light as *mut u32).cast(),
            &mut size,
        )
    };
    status == 0 && light == 0
}

pub struct ThemeWatch {
    key: HKEY,
    pub event: HANDLE,
}

impl ThemeWatch {
    pub fn new() -> Option<Self> {
        // SAFETY: this thread owns the registry key and the unnamed auto-reset event.
        unsafe {
            let mut key = null_mut();
            if RegOpenKeyExW(HKEY_CURRENT_USER, PERSONALIZE, 0, KEY_NOTIFY, &mut key) != 0 {
                return None;
            }
            let event = CreateEventW(null(), 0, 0, null());
            let watch = Self { key, event };
            (!event.is_null() && watch.arm()).then_some(watch)
        }
    }

    pub fn arm(&self) -> bool {
        // SAFETY: both handles remain live until Drop closes the key and then the event.
        unsafe {
            RegNotifyChangeKeyValue(self.key, 0, REG_NOTIFY_CHANGE_LAST_SET, self.event, 1) == 0
        }
    }
}

impl Drop for ThemeWatch {
    fn drop(&mut self) {
        // SAFETY: this watcher owns both handles.
        unsafe {
            RegCloseKey(self.key);
            if !self.event.is_null() {
                CloseHandle(self.event);
            }
        }
    }
}

pub fn load() -> io::Result<Config> {
    let mut file = match File::open(path()?) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(default_config()),
        Err(err) => return Err(err),
    };
    let mut bytes = Vec::new();
    (&mut file).take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(io::Error::other("Settings file is too large"));
    }
    let mut config: Config =
        serde_json::from_slice(&bytes).map_err(|_| io::Error::other("Settings file is invalid"))?;
    config.validate().map_err(io::Error::other)?;
    config.limits.complete();
    Ok(config)
}

pub fn save(config: &Config) -> io::Result<()> {
    let path = path()?;
    let staging = path.with_extension("rustnut.tmp");
    let mut file = File::create(&staging)?;
    let bytes = serde_json::to_vec_pretty(config)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    let from = wide(staging.as_os_str());
    let to = wide(path.as_os_str());
    // SAFETY: both path buffers are terminated and stay alive for the call.
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn reset() -> io::Result<()> {
    let path = path()?;
    for file in [path.with_extension("rustnut.tmp"), path] {
        match fs::remove_file(file) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
    }
    Ok(())
}

fn crypt(bytes: &[u8], protect: bool) -> io::Result<Zeroizing<Vec<u8>>> {
    if bytes.is_empty() {
        return Ok(Zeroizing::new(Vec::new()));
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes
            .len()
            .try_into()
            .map_err(|_| io::Error::other("Credential is too long"))?,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: DPAPI reads the input blob and allocates its own output via LocalAlloc.
    unsafe {
        let result = if protect {
            CryptProtectData(
                &input,
                windows_sys::core::w!("RustNUT NUT password"),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        if output.pbData.is_null() {
            return Ok(Zeroizing::new(Vec::new()));
        }
        let data = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let copy = Zeroizing::new(data.to_vec());
        data.zeroize();
        LocalFree(output.pbData.cast());
        Ok(copy)
    }
}

pub fn encrypt(password: &str) -> io::Result<Vec<u8>> {
    Ok(crypt(password.as_bytes(), true)?.to_vec())
}

pub fn decrypt(config: &Config) -> io::Result<Zeroizing<String>> {
    let decrypted = crypt(&config.secret, false)?;
    let password =
        std::str::from_utf8(&decrypted).map_err(|_| io::Error::other("Invalid credential"))?;
    Ok(Zeroizing::new(password.to_owned()))
}

const RUN_KEY: windows_sys::core::PCWSTR =
    windows_sys::core::w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const ENTRY: windows_sys::core::PCWSTR = windows_sys::core::w!("RustNUT");

pub fn startup_enabled() -> bool {
    startup_registered(RUN_KEY)
}

fn startup_registered(key: windows_sys::core::PCWSTR) -> bool {
    let mut size = 0;
    // SAFETY: the string pointers are static and the only writable argument is size.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            ENTRY,
            RRF_RT_REG_SZ,
            null_mut(),
            null_mut(),
            &mut size,
        ) == 0
    }
}

pub fn set_startup(enabled: bool) -> io::Result<()> {
    write_startup(RUN_KEY, enabled)
}

fn write_startup(key: windows_sys::core::PCWSTR, enabled: bool) -> io::Result<()> {
    // SAFETY: the caller retains the key path and the command buffer remains live during the write.
    let result = unsafe {
        if enabled {
            let mut cmd = std::ffi::OsString::from("\"");
            cmd.push(std::env::current_exe()?.as_os_str());
            cmd.push("\" --background");
            let cmd = wide(cmd);
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key,
                ENTRY,
                REG_SZ,
                cmd.as_ptr().cast(),
                (cmd.len() * 2) as u32,
            )
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, key, ENTRY)
        }
    };
    if result == 0 || (!enabled && result == ERROR_FILE_NOT_FOUND) {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trip_and_corruption() {
        let mut cfg = Config {
            secret: encrypt("тест \\ example").unwrap(),
            ..Config::default()
        };
        assert_eq!(decrypt(&cfg).unwrap().as_str(), "тест \\ example");
        assert!(!cfg.secret.windows(7).any(|bytes| bytes == b"example"));
        cfg.secret.truncate(6);
        assert!(decrypt(&cfg).is_err());
    }

    #[test]
    fn startup_registry_round_trip() {
        struct TestKey(Vec<u16>);
        impl Drop for TestKey {
            fn drop(&mut self) {
                // SAFETY: this is the unique leaf key created by this test.
                unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, self.0.as_ptr()) };
            }
        }
        let key = TestKey(wide(format!(
            r"Software\RustNUT.Test.{}",
            std::process::id()
        )));
        let path = key.0.as_ptr();
        let mut handle = null_mut();
        // SAFETY: path and the writable output handle are valid for this call.
        unsafe {
            assert_eq!(
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    path,
                    0,
                    null(),
                    REG_OPTION_VOLATILE,
                    KEY_SET_VALUE,
                    null(),
                    &mut handle,
                    null_mut()
                ),
                0
            );
            assert_eq!(RegCloseKey(handle), 0);
        }
        assert!(!startup_registered(path));
        write_startup(path, true).unwrap();
        assert!(startup_registered(path));
        write_startup(path, false).unwrap();
        assert!(!startup_registered(path));
        write_startup(path, false).unwrap();
    }
}
