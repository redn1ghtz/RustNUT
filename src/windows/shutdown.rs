use crate::config::Action;
use std::{io, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_SUCCESS, GetLastError, HANDLE},
    Security::*,
    System::{
        Power::SetSuspendState,
        Shutdown::*,
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: Token owns the handle returned by OpenProcessToken.
        unsafe { CloseHandle(self.0) };
    }
}

pub fn request(action: Action, force_close: bool) -> io::Result<()> {
    // SAFETY: output structures are initialized before use. The previous privilege
    // state is restored after requesting the local power action.
    unsafe {
        let mut handle = null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut handle,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let token = Token(handle);
        let mut request = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            ..Default::default()
        };
        if LookupPrivilegeValueW(
            std::ptr::null(),
            SE_SHUTDOWN_NAME,
            &mut request.Privileges[0].Luid,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        request.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
        let mut previous = TOKEN_PRIVILEGES::default();
        let mut size = 0;
        if AdjustTokenPrivileges(
            token.0,
            0,
            &request,
            std::mem::size_of_val(&previous) as u32,
            &mut previous,
            &mut size,
        ) == 0
            || GetLastError() != ERROR_SUCCESS
        {
            return Err(io::Error::last_os_error());
        }
        let reason =
            SHTDN_REASON_MAJOR_POWER | SHTDN_REASON_MINOR_ENVIRONMENT | SHTDN_REASON_FLAG_PLANNED;
        let result = match action {
            Action::Shutdown if force_close => {
                let status = InitiateShutdownW(
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    SHUTDOWN_POWEROFF | SHUTDOWN_FORCE_SELF | SHUTDOWN_FORCE_OTHERS,
                    reason,
                );
                if status == ERROR_SUCCESS {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(status as i32))
                }
            }
            _ => {
                let success = match action {
                    Action::Shutdown => ExitWindowsEx(EWX_POWEROFF, reason) != 0,
                    Action::Sleep | Action::Hibernate => {
                        SetSuspendState(action == Action::Hibernate, false, false)
                    }
                };
                if success {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            }
        };
        AdjustTokenPrivileges(token.0, 0, &previous, 0, null_mut(), null_mut());
        result
    }
}
