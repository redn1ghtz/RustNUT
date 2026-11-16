#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(any(windows, test))]
mod config;
#[cfg(all(windows, test))]
mod icon;
#[cfg(any(windows, test))]
mod monitor;
#[cfg(any(windows, test))]
mod nut;
#[cfg(any(windows, test))]
mod policy;
#[cfg(test)]
mod tests;
#[cfg(windows)]
mod windows;

fn main() {
    #[cfg(windows)]
    windows::run();
    #[cfg(not(windows))]
    eprintln!("RustNUT requires Windows 10 or 11.");
}
